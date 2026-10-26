use anyhow::Result;
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::fs;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};

const PATH: &str = "configs/config.json";

static CONFIG: OnceLock<RwLock<Config>> = OnceLock::new();
/// Derived from the account plan each start and never persisted: a stale `false` from one
/// failed check would otherwise disable librespot for good.
static USE_LIBRESPOT: AtomicBool = AtomicBool::new(true);
/// Set by `--no-librespot`; pins `USE_LIBRESPOT` off.
static FORCE_NO_LIBRESPOT: AtomicBool = AtomicBool::new(false);
/// The account plan from the last check, also never persisted: 0 unknown (not checked, or
/// Spotify didn't say), 1 free, 2 Premium.
static PLAN: AtomicU8 = AtomicU8::new(0);

#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub log_level: String,
    pub admin_password_hash: String,
    /// Pre-shared secret that authorises togedemon push. Auto-generated on first run.
    pub togedemon_token: String,
    /// Max on-disk track cache size in megabytes.
    pub cache_max_mb: u64,
    /// Opus bitrate cached tracks are encoded at; YouTube's own Opus is kept as it is.
    pub audio_bitrate: String,
    pub normalization: Normalization,
    /// Stream a 112k copy of tracks cached above High quality (>144k).
    pub low_data_streaming: bool,
    /// Public `/player` and `/stream`. Off = cache-only (download what you listen to).
    pub player_enabled: bool,
    /// Optional togematch-api that picks each track's YouTube video.
    pub togematch_api_url: String,
    /// Config file only.
    pub cover_art: CoverArt,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spotify_auth: Option<SpotifyAuth>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct SpotifyAuth {
    pub access_token: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    pub expires_at_unix: i64,
}

/// ReplayGain 2.0 (−14 LUFS / −1 dBTP) via rsgain, written into the Opus header gain.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Normalization {
    #[default]
    Off,
    /// A normalized RAM copy per streamed track; cached files stay untouched.
    Stream,
    /// Tags on every newly cached file.
    Cache,
}

/// The size of the cover embedded in cached tracks.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum CoverArt {
    /// The largest size Spotify's metadata lists, 640 px.
    #[default]
    #[serde(rename = "640")]
    Listed,
    /// As uploaded, often 1400 to 3000 px, else the listed one.
    #[serde(rename = "original")]
    Original,
}

/// Offered in the dashboard; also the allowlist, since the value reaches an ffmpeg arg.
pub const AUDIO_BITRATES: &[&str] = &["112k", "144k", "192k", "256k"];
const DEFAULT_BITRATE: &str = "144k";

impl Default for Config {
    fn default() -> Self {
        Self {
            log_level: "info".into(),
            admin_password_hash: String::new(),
            togedemon_token: String::new(),
            cache_max_mb: 2048,
            audio_bitrate: DEFAULT_BITRATE.into(),
            normalization: Normalization::Off,
            low_data_streaming: false,
            player_enabled: false,
            togematch_api_url: String::new(),
            cover_art: CoverArt::Listed,
            spotify_auth: None,
        }
    }
}

impl Config {
    #[must_use]
    pub fn is_authenticated(&self) -> bool {
        self.spotify_auth.is_some()
    }

    #[must_use]
    pub fn has_admin_password(&self) -> bool {
        !self.admin_password_hash.is_empty()
    }
}

pub fn init() {
    // A file that exists but will not load must never fall back to defaults: the next
    // save would overwrite the admin hash and tokens it failed to read.
    let loaded = match fs::read_to_string(PATH) {
        Ok(data) => serde_json::from_str(&data).map_err(|e| e.to_string()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
        Err(e) => Err(e.to_string()),
    };
    let mut cfg = loaded.unwrap_or_else(|err| {
        eprintln!("{PATH} exists but could not be loaded ({err}).");
        eprintln!("Fix its permissions or move it aside to start fresh.");
        std::process::exit(1);
    });

    if cfg.togedemon_token.is_empty() {
        cfg.togedemon_token = crate::random_hex(24);
        if let Err(e) = save(&cfg) {
            eprintln!("Could not write {PATH} ({e}); the togedemon token changes every start.");
        }
    }
    CONFIG.set(RwLock::new(cfg)).ok();
}

pub fn get() -> &'static RwLock<Config> {
    CONFIG.get().expect("config not initialized")
}

/// Apply `f` under the write lock, then persist.
pub fn update(f: impl FnOnce(&mut Config)) -> Result<()> {
    // One writer at a time, so an older snapshot never lands on disk after a newer one
    // (losing, say, a rotated refresh token).
    static WRITER: parking_lot::Mutex<()> = parking_lot::Mutex::new(());
    let _writer = WRITER.lock();
    let snapshot = {
        let mut cfg = get().write();
        f(&mut cfg);
        cfg.clone()
    };
    save(&snapshot)
}

fn save(cfg: &Config) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    fs::create_dir_all("configs")?;
    let data = serde_json::to_string_pretty(cfg)?;
    // Unique temp file, mode 0600 from creation, then atomic rename.
    let tmp = format!("{PATH}.tmp.{}", crate::random_hex(6));
    let res = (|| -> Result<()> {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)?;
        file.write_all(data.as_bytes())?;
        file.sync_all()?;
        fs::rename(&tmp, PATH)?;
        Ok(())
    })();
    if res.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    res
}

pub fn save_spotify_auth(
    access_token: &str,
    refresh_token: Option<&str>,
    expires_in_secs: u64,
) -> Result<()> {
    let expires_at_unix = crate::now_unix() + expires_in_secs.max(60) as i64;
    update(|cfg| {
        let refresh_token = refresh_token.map(String::from).or_else(|| {
            cfg.spotify_auth
                .as_mut()
                .and_then(|a| a.refresh_token.take())
        });
        cfg.spotify_auth = Some(SpotifyAuth {
            access_token: access_token.to_string(),
            refresh_token,
            expires_at_unix,
        });
    })
}

pub fn invalidate_spotify_access_token() {
    if let Some(auth) = &mut get().write().spotify_auth {
        auth.expires_at_unix = 0;
    }
}

#[must_use]
pub fn use_librespot() -> bool {
    USE_LIBRESPOT.load(Ordering::Relaxed)
}

pub fn set_use_librespot(on: bool) {
    USE_LIBRESPOT.store(
        on && !FORCE_NO_LIBRESPOT.load(Ordering::Relaxed),
        Ordering::Relaxed,
    );
}

/// Whether the account is Premium, if the last check could tell.
#[must_use]
pub fn premium() -> Option<bool> {
    match PLAN.load(Ordering::Relaxed) {
        1 => Some(false),
        2 => Some(true),
        _ => None,
    }
}

pub fn set_premium(premium: Option<bool>) {
    let plan = match premium {
        None => 0,
        Some(false) => 1,
        Some(true) => 2,
    };
    PLAN.store(plan, Ordering::Relaxed);
}

pub fn force_no_librespot() {
    FORCE_NO_LIBRESPOT.store(true, Ordering::Relaxed);
    USE_LIBRESPOT.store(false, Ordering::Relaxed);
}

#[must_use]
pub fn audio_bitrate() -> String {
    let bitrate = &get().read().audio_bitrate;
    if AUDIO_BITRATES.contains(&bitrate.as_str()) {
        bitrate.clone()
    } else {
        DEFAULT_BITRATE.into()
    }
}

#[must_use]
pub fn normalization() -> Normalization {
    get().read().normalization
}

#[must_use]
pub fn cover_art() -> CoverArt {
    get().read().cover_art
}

#[must_use]
pub fn low_data_streaming() -> bool {
    get().read().low_data_streaming
}

#[must_use]
pub fn togematch_api_url() -> Option<String> {
    let url = get()
        .read()
        .togematch_api_url
        .trim()
        .trim_end_matches('/')
        .to_string();
    (!url.is_empty()).then_some(url)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_fields_take_defaults() {
        let cfg: Config =
            serde_json::from_str(r#"{"player_enabled": true, "normalization": "stream"}"#).unwrap();
        assert!(cfg.player_enabled);
        assert_eq!(cfg.normalization, Normalization::Stream);
        assert_eq!(cfg.cache_max_mb, 2048);
        assert_eq!(cfg.audio_bitrate, DEFAULT_BITRATE);
        assert_eq!(cfg.cover_art, CoverArt::Listed);
    }

    #[test]
    fn cover_art_reads_640_or_original() {
        let original: Config = serde_json::from_str(r#"{"cover_art": "original"}"#).unwrap();
        assert_eq!(original.cover_art, CoverArt::Original);
        let listed: Config = serde_json::from_str(r#"{"cover_art": "640"}"#).unwrap();
        assert_eq!(listed.cover_art, CoverArt::Listed);
        assert!(serde_json::from_str::<Config>(r#"{"cover_art": "huge"}"#).is_err());
        let saved = serde_json::to_string(&Config::default()).unwrap();
        assert!(saved.contains(r#""cover_art":"640""#));
    }
}
