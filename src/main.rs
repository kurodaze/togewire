use axum::serve::ListenerExt;
use clap::Parser;
use std::net::SocketAddr;
use std::time::Duration;
use togewire::{config, server};
use tracing::{debug, info};
use tracing_subscriber::EnvFilter;

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

    togewire::require_bin("ffmpeg", "-version", "Install it and try again.");
    let (log_level, normalize, needs_setup) = {
        let cfg = config::get().read();
        (
            cfg.log_level.clone(),
            cfg.normalization != config::Normalization::Off,
            !cfg.has_admin_password(),
        )
    };
    if normalize {
        togewire::require_bin(
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
        togewire::audio::ytdlp::set_cli_cookies(Some(path.clone()));
    }
    if let Some(path) = togewire::audio::ytdlp::cookies_file() {
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
        });

        let addr = format!("{}:{}", args.host, args.port);
        info!("Server starting on http://{addr}");
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
