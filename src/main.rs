mod audio;
mod cache;
mod config;
mod server;
mod spotify;
mod types;

use axum::serve::ListenerExt;
use clap::Parser;
use std::net::SocketAddr;
use std::time::Duration;
use tracing::{debug, info};
use tracing_subscriber::EnvFilter;

pub(crate) fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        })
}

/// One pooled client for Spotify, cover art and tool downloads; the web-player endpoints
/// expect a browser user agent.
pub(crate) fn http() -> &'static reqwest::Client {
    static CLIENT: std::sync::LazyLock<reqwest::Client> = std::sync::LazyLock::new(|| {
        reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(5))
            .timeout(std::time::Duration::from_secs(10))
            .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36")
            .build()
            .expect("HTTP client")
    });
    &CLIENT
}

pub(crate) fn random_hex(n_bytes: usize) -> String {
    let mut bytes = vec![0u8; n_bytes];
    getrandom::fill(&mut bytes).expect("OS RNG");
    hex(&bytes)
}

/// Exit unless `bin` runs from PATH.
fn require_bin(bin: &str, version_arg: &str, hint: &str) {
    let ok = std::process::Command::new(bin)
        .arg(version_arg)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success());
    if !ok {
        eprintln!("{bin} is required but was not found in PATH. {hint}");
        std::process::exit(1);
    }
}

/// Last `n` chars of a trimmed string - keeps tail of subprocess stderr.
pub(crate) fn tail_chars(s: &str, n: usize) -> String {
    let s = s.trim();
    let skip = s.chars().count().saturating_sub(n);
    s.chars().skip(skip).collect()
}

fn current_uid() -> u32 {
    // SAFETY: getuid has no preconditions and cannot fail.
    unsafe {
        unsafe extern "C" {
            fn getuid() -> u32;
        }
        getuid()
    }
}

/// Per-uid path in `/dev/shm` (falling back to the temp dir) for in-memory files.
pub(crate) fn shm_dir_path(prefix: &str) -> std::path::PathBuf {
    let base = std::path::Path::new("/dev/shm");
    let base = if base.is_dir() {
        base.to_path_buf()
    } else {
        std::env::temp_dir()
    };
    base.join(format!("{prefix}_{}", current_uid()))
}

/// Create `dir` with mode `0700` and verify it is a private directory owned by this user
/// (`/dev/shm` is shared, so a directory squatted by another user must never be used).
pub(crate) fn ensure_shm_dir(dir: &std::path::Path) {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt};
    let _ = std::fs::DirBuilder::new().mode(0o700).create(dir);
    if let Ok(meta) = std::fs::symlink_metadata(dir) {
        assert!(
            meta.is_dir() && meta.uid() == current_uid() && meta.mode() & 0o077 == 0,
            "{} is not a private directory owned by this user; remove it",
            dir.display()
        );
    }
}

/// A connection whose data sits unread this long is dropped (TCP_USER_TIMEOUT), so a listener
/// that stops reading can't keep its stream slot and buffered audio for good. As long as Linux
/// waits on a peer that stops answering, since a browser paused for a while stops reading too.
const UNREAD_LIMIT: Duration = Duration::from_mins(15);

#[derive(Parser)]
#[command(name = "togewire")]
struct Args {
    /// Server port
    #[arg(long, default_value_t = 7093)]
    port: u16,

    /// Server host
    #[arg(long, default_value = "0.0.0.0")]
    host: String,

    /// Skip librespot and cache every track with yt-dlp
    #[arg(long)]
    no_librespot: bool,

    /// YouTube cookies file for yt-dlp (default: auto-detected in data/)
    #[arg(long)]
    cookies: Option<String>,
}

fn main() {
    // librespot + reqwest both enable rustls crypto backends (ring + aws-lc-rs);
    // pick one process-wide provider before any TLS use.
    rustls::crypto::aws_lc_rs::default_provider()
        .install_default()
        .expect("failed to install rustls CryptoProvider");

    let banner = r"
 ███████████    ███████      █████████  ██████████ █████   ███   █████ █████ ███████████   ██████████
░█░░░███░░░█  ███░░░░░███   ███░░░░░███░░███░░░░░█░░███   ░███  ░░███ ░░███ ░░███░░░░░███ ░░███░░░░░█
░   ░███  ░  ███     ░░███ ███     ░░░  ░███  █ ░  ░███   ░███   ░███  ░███  ░███    ░███  ░███  █ ░
    ░███    ░███      ░███░███          ░██████    ░███   ░███   ░███  ░███  ░██████████   ░██████
    ░███    ░███      ░███░███    █████ ░███░░█    ░░███  █████  ███   ░███  ░███░░░░░███  ░███░░█
    ░███    ░░███     ███ ░░███  ░░███  ░███ ░   █  ░░░█████░█████░    ░███  ░███    ░███  ░███ ░   █
    █████    ░░░███████░   ░░█████████  ██████████    ░░███ ░░███      █████ █████   █████ ██████████
   ░░░░░       ░░░░░░░      ░░░░░░░░░  ░░░░░░░░░░      ░░░   ░░░      ░░░░░ ░░░░░   ░░░░░ ░░░░░░░░░░
";
    println!("{banner}");
    if cfg!(debug_assertions) {
        println!("  [DEV BUILD]\n");
    }

    let args = Args::parse();
    config::init();

    require_bin("ffmpeg", "-version", "Install it and try again.");
    let (log_level, normalize, needs_setup) = {
        let cfg = config::get().read();
        (
            cfg.log_level.clone(),
            cfg.normalization != config::Normalization::Off,
            !cfg.has_admin_password(),
        )
    };
    if normalize {
        require_bin(
            "rsgain",
            "-v",
            "Install it (e.g. sudo apt install rsgain) or disable normalization in config.",
        );
    }

    let level = match log_level.as_str() {
        "trace" | "debug" | "warn" | "error" => log_level.as_str(),
        _ if cfg!(debug_assertions) => "debug",
        _ => "info",
    };
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::new(format!("togewire={level},info")))
        .init();

    if args.no_librespot {
        config::force_no_librespot();
    }
    if let Some(ref path) = args.cookies {
        if !std::path::Path::new(path).is_file() {
            eprintln!("togewire: cookies file '{path}' not found");
            std::process::exit(1);
        }
        audio::ytdlp::set_cli_cookies(Some(path.clone()));
    }
    if let Some(path) = audio::ytdlp::cookies_file() {
        info!("YouTube cookies found at {path} (used when challenged)");
    } else {
        debug!("no YouTube cookies found in data/");
    }
    if needs_setup {
        // Printed, not logged: a log_level above info would hide it.
        let token = server::auth::generate_setup_token();
        println!("\n   One-Time Setup Token: {token}");
        println!("   This token is required to set your admin password.\n");
    }

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("Failed to build Tokio runtime");

    rt.block_on(async {
        let (router, app_state) = server::create_server();
        if config::get().read().is_authenticated() {
            app_state
                .spotify_client
                .sync_librespot_for_subscription()
                .await;
        }
        server::start_workers(&app_state);

        // Graceful shutdown on SIGINT/SIGTERM; ending the long-lived streams lets axum drain.
        let signal_state = app_state.clone();
        tokio::spawn(async move {
            let mut sigterm =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                    .expect("failed to listen for SIGTERM");
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = sigterm.recv() => {}
            }
            info!("Shutdown signal received...");
            signal_state.shutdown.cancel();
            signal_state.stream_manager.shutdown();
            signal_state.preparer.cache.shutdown_mem();
        });

        let addr = format!("{}:{}", args.host, args.port);
        info!(
            "togewire v{} starting on http://{addr}",
            env!("CARGO_PKG_VERSION")
        );
        let listener = tokio::net::TcpListener::bind(&addr)
            .await
            .expect("Failed to bind address")
            .tap_io(|tcp| {
                let _ = socket2::SockRef::from(&*tcp).set_tcp_user_timeout(Some(UNREAD_LIMIT));
            });

        let shutdown = app_state.shutdown.clone();
        axum::serve(
            listener,
            router.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(async move { shutdown.cancelled().await })
        .await
        .expect("Server error");
        info!("Server shutdown complete");
    });
}
