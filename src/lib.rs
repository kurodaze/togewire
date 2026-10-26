pub mod audio;
pub mod cache;
pub mod config;
pub mod server;
pub mod spotify;
pub mod types;

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
    use rand::RngExt;
    let mut bytes = vec![0u8; n_bytes];
    rand::rng().fill(&mut bytes[..]);
    hex(&bytes)
}

/// Exit unless `bin` runs from PATH.
pub fn require_bin(bin: &str, version_arg: &str, hint: &str) {
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
