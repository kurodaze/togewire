use anyhow::{Result, anyhow, bail};
use std::fs::OpenOptions;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tokio::process::Command;
use tokio::sync::{Mutex, OnceCell};
use tracing::{debug, info, warn};

use super::AudioMetadata;
use crate::cache::Manager as CacheManager;
use crate::types::Track;
use togematch::Candidate;

const DOWNLOAD_TIMEOUT: Duration = Duration::from_mins(2);
const SEARCH_TIMEOUT: Duration = Duration::from_secs(90);
const YTDLP_BIN: &str = "data/bin/yt-dlp";
const DENO_BIN: &str = "data/bin/deno";
/// How often yt-dlp checks for a new release of itself, besides at startup. YouTube changes
/// often, and an old yt-dlp stops working.
const UPDATE_EVERY: Duration = Duration::from_hours(6);
/// A failure that asks for an update checks again at most this often.
const UPDATE_GAP: Duration = Duration::from_mins(10);
const UPDATE_TIMEOUT: Duration = Duration::from_mins(2);
const API_DOWN_FOR: Duration = Duration::from_mins(1);
/// yt-dlp's own words when a newer version may fix a failure.
const OUTDATED: &str = "Confirm you are on the latest version";
/// yt-dlp's reasons it can't update itself, like a copy from a package manager.
const CANT_UPDATE: &[&str] = &[
    "Use that to update",
    "Re-download the latest release",
    "cannot be updated",
    "Use git to pull",
    "Build the executable again",
    "Unable to write to",
];
/// When yt-dlp last checked for a new release, and last installed one.
static UPDATES: Mutex<(Option<Instant>, Option<Instant>)> = Mutex::const_new((None, None));
/// Set once yt-dlp has said it can't update itself.
static UPDATES_OFF: AtomicBool = AtomicBool::new(false);
/// Skips an unreachable togematch-api briefly so lookups don't each wait on it.
static API_DOWN_UNTIL: parking_lot::Mutex<Option<Instant>> = parking_lot::Mutex::new(None);
/// yt-dlp rewrites its cookies file when it's done, so a run starting meanwhile could read it
/// half written. Each run gets its own copy, and copies go in and out one at a time.
static COOKIES_TURN: parking_lot::Mutex<()> = parking_lot::Mutex::new(());
pub static ACTIVE_COOKIES: parking_lot::Mutex<Option<String>> = parking_lot::Mutex::new(None);
static CLI_COOKIES: parking_lot::Mutex<Option<String>> = parking_lot::Mutex::new(None);
static COOKIES_DEAD: AtomicBool = AtomicBool::new(false);
/// How long the cookies are used after YouTube asks to sign in.
const LOGGED_IN_FOR: Duration = Duration::from_secs(3600);
/// When YouTube last asked to sign in.
static ASKED: parking_lot::Mutex<Option<Instant>> = parking_lot::Mutex::new(None);

pub fn set_cli_cookies(path: Option<String>) {
    *CLI_COOKIES.lock() = path;
}

pub fn logged_in() -> bool {
    cookies_file().is_some() && ASKED.lock().is_some_and(|t| t.elapsed() < LOGGED_IN_FOR)
}

/// Finds a cookies file in `dir`, checking standard names first then any `*cookie*.txt` file.
pub fn find_cookies(dir: &str) -> Option<String> {
    for name in [
        "cookies.txt",
        "youtube-cookies.txt",
        "www.youtube.com_cookies.txt",
    ] {
        let p = Path::new(dir).join(name);
        if p.is_file() {
            return Some(p.to_string_lossy().into_owned());
        }
    }
    let rd = std::fs::read_dir(dir).ok()?;
    for entry in rd.flatten() {
        let name = entry.file_name().to_string_lossy().to_lowercase();
        if name.ends_with(".txt") && name.contains("cookie") && entry.path().is_file() {
            return Some(entry.path().to_string_lossy().into_owned());
        }
    }
    None
}

pub fn cookies_file() -> Option<String> {
    if let Some(file) = CLI_COOKIES.lock().clone() {
        return Some(file);
    }
    find_cookies("data")
}

pub struct YtdlpFetcher {
    cache: Arc<CacheManager>,
}

impl YtdlpFetcher {
    pub fn new(cache: Arc<CacheManager>) -> Self {
        Self { cache }
    }

    pub fn cookies_file(&self) -> Option<String> {
        cookies_file()
    }

    pub fn logged_in(&self) -> bool {
        logged_in()
    }

    pub async fn fetch_track(&self, track: &Track) -> Result<PathBuf> {
        let js = ensure_tools().await?;
        let video_id = match ask_togematch_api(track).await? {
            Some(id) => id,
            None => search_best(js, track).await?,
        };
        debug!("yt-dlp matched video {video_id} for {}", track.id);

        let work_dir = self.cache.track_path(track, "ytwork");
        let res = self
            .download_to_cache(js, track, &video_id, &work_dir)
            .await;
        let _ = tokio::fs::remove_dir_all(&work_dir).await;
        res
    }

    async fn download_to_cache(
        &self,
        js: &str,
        track: &Track,
        video_id: &str,
        work_dir: &Path,
    ) -> Result<PathBuf> {
        let url = format!("https://www.youtube.com/watch?v={video_id}");
        let out_tmpl = work_dir.join("dl.%(ext)s");
        let (mut retried, mut updated) = (false, false);
        let downloaded = loop {
            let logged_in_now = logged_in();
            let _ = tokio::fs::remove_dir_all(work_dir).await;
            tokio::fs::create_dir_all(work_dir).await?;
            let output = download_audio(js, &out_tmpl, &url).await?;
            if output.status.success() {
                break find_downloaded(work_dir).await?;
            }
            let err = String::from_utf8_lossy(&output.stderr);
            if !logged_in_now
                && cookies_file().is_some()
                && (err.contains("Sign in to confirm you") || err.contains("confirm your age"))
            {
                info!("YouTube asked to sign in, so lookups use the cookies for the next hour");
                *ASKED.lock() = Some(Instant::now());
                continue;
            }
            if !updated && err.contains(OUTDATED) && update(UPDATE_GAP).await {
                info!("yt-dlp: trying the download again with its new version");
                updated = true;
                continue;
            }
            // YouTube 403s intermittently; one fresh attempt usually clears it.
            if !retried && err.contains("403") {
                debug!("yt-dlp 403 on {video_id}, retrying once");
                retried = true;
                continue;
            }
            bail!("yt-dlp download failed: {}", crate::tail_chars(&err, 400));
        };
        debug!("yt-dlp downloaded {}", downloaded.display());

        let dest = self.cache.track_path(track, "ogg");
        // Must end in a real media extension - ffmpeg ignores compound ".ogg.tmp".
        let dest_tmp = dest.with_extension("tmp.ogg");
        // YouTube titles, channels and thumbnails are discarded: Spotify's metadata only.
        let meta = AudioMetadata {
            title: track.name.clone(),
            artist: track.artist.clone(),
            album: track.album.clone(),
            album_artist: track.album_artist.clone().unwrap_or_default(),
            date: track.release_year(),
            track_number: track.track_number,
            disc_number: track.disc_number,
            picture: super::fetch_cover(track).await,
        };
        // YouTube's Opus (format 251, ~160k) is remuxed as-is; anything else is encoded.
        let bitrate = if super::is_opus(&downloaded).await {
            None
        } else {
            Some(crate::config::audio_bitrate())
        };
        let normalized = super::write_cache_opus(&downloaded, &dest_tmp, bitrate.as_deref(), &meta)
            .await
            .map_err(anyhow::Error::msg)?;
        tokio::fs::rename(&dest_tmp, &dest).await?;
        self.cache.put(
            &track.id,
            &dest.to_string_lossy(),
            bitrate.as_deref().unwrap_or("160k"),
            normalized,
        );
        Ok(dest)
    }
}

/// Asks the optional togematch-api for this track's YouTube video.
async fn ask_togematch_api(track: &Track) -> Result<Option<String>> {
    let Some(base) = crate::config::togematch_api() else {
        return Ok(None);
    };
    if API_DOWN_UNTIL
        .lock()
        .is_some_and(|until| Instant::now() < until)
    {
        return Ok(None);
    }
    let query = format!("spotify:track:{}", track.id);
    let answer = crate::http()
        .get(format!("{base}/match"))
        .query(&[("q", &query)])
        // a first match can take a few searches
        .timeout(Duration::from_secs(90))
        .send()
        .await
        .inspect_err(|e| {
            if e.is_connect() {
                *API_DOWN_UNTIL.lock() = Some(Instant::now() + API_DOWN_FOR);
            }
        })
        .and_then(reqwest::Response::error_for_status);
    let body: serde_json::Value = match answer {
        Ok(response) => match response.json().await {
            Ok(body) => body,
            Err(e) => {
                info!("togematch-api at {base} didn't answer ({e}), searching YouTube");
                return Ok(None);
            }
        },
        Err(e) => {
            info!("togematch-api at {base} didn't answer ({e}), searching YouTube");
            return Ok(None);
        }
    };
    let Some(hit) = body["tracks"].get(0) else {
        return Ok(None);
    };
    let Some(id) = hit["youtube"]
        .as_str()
        .and_then(|u| u.split_once("v="))
        .and_then(|(_, q)| q.split('&').next())
        .filter(|id| {
            id.len() == 11
                && id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        })
    else {
        return Err(NoMatch.into());
    };
    debug!(
        "togematch-api matched {} to {id} ({})",
        track.id, hit["source"]
    );
    Ok(Some(id.to_string()))
}

/// Search results. A search that fails outright is an error, not "no match", which would stick.
async fn flat_dump(js: &str, targets: &[String]) -> Result<Vec<Candidate>> {
    let mut updated = false;
    loop {
        let logged_in_now = logged_in();
        let (mut cmd, cookies) = ytdlp_cmd(js);
        let out = tokio::time::timeout(
            SEARCH_TIMEOUT,
            cmd.args(["--flat-playlist", "--dump-json", "--no-download", "--"])
                .args(targets)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(true)
                .output(),
        )
        .await
        .map_err(|_| anyhow!("yt-dlp search timed out"))??;
        if let Some(ref c) = cookies {
            c.save();
        }
        let err = String::from_utf8_lossy(&out.stderr);
        check_cookies(&err);
        let rows = candidates(&out.stdout);
        if !rows.is_empty() || out.status.success() {
            return Ok(rows);
        }
        if !logged_in_now
            && cookies_file().is_some()
            && (err.contains("Sign in to confirm you") || err.contains("confirm your age"))
        {
            info!("YouTube asked to sign in, so lookups use the cookies for the next hour");
            *ASKED.lock() = Some(Instant::now());
            continue;
        }
        if !updated && err.contains(OUTDATED) && update(UPDATE_GAP).await {
            info!("yt-dlp: trying the search again with its new version");
            updated = true;
            continue;
        }
        bail!("yt-dlp search failed: {}", crate::tail_chars(&err, 400));
    }
}

fn candidates(stdout: &[u8]) -> Vec<Candidate> {
    String::from_utf8_lossy(stdout)
        .lines()
        .filter_map(|line| {
            let v: serde_json::Value = serde_json::from_str(line).ok()?;
            let f = |k: &str| {
                v.get(k)
                    .and_then(|x| x.as_str())
                    .unwrap_or_default()
                    .to_string()
            };
            let id = f("id");
            let uploader = Some(f("uploader"))
                .filter(|u| !u.is_empty())
                .unwrap_or_else(|| f("channel"));
            let duration = (v
                .get("duration")
                .and_then(serde_json::Value::as_f64)
                .unwrap_or(0.0)
                * 1000.0) as i64;
            (!id.is_empty()).then(|| Candidate {
                id,
                title: f("title"),
                uploader,
                description: f("description"),
                album: f("album"),
                duration,
            })
        })
        .collect()
}

/// Checks for a new yt-dlp at startup and every few hours, while the server runs.
pub fn keep_updated() {
    tokio::spawn(async {
        loop {
            update(Duration::ZERO).await;
            tokio::time::sleep(UPDATE_EVERY).await;
        }
    });
}

/// Runs yt-dlp's own updater, unless it ran less than `gap` ago. True if a new version came
/// in, then or within `gap`. One not downloaded yet comes new on first use anyway.
async fn update(gap: Duration) -> bool {
    if UPDATES_OFF.load(Ordering::Relaxed) || !Path::new(YTDLP_BIN).is_file() {
        return false;
    }
    let mut last = UPDATES.lock().await;
    let recent = |at: Option<Instant>| at.is_some_and(|at| at.elapsed() < gap);
    if recent(last.0) {
        return recent(last.1);
    }
    last.0 = Some(Instant::now());
    let run = Command::new(YTDLP_BIN)
        .arg("-U")
        .kill_on_drop(true)
        .output();
    let out = match tokio::time::timeout(UPDATE_TIMEOUT, run).await {
        Ok(Ok(out)) => out,
        Ok(Err(e)) => {
            warn!("yt-dlp: couldn't check for a new version: {e}");
            return false;
        }
        Err(_) => {
            warn!("yt-dlp: checking for a new version took too long");
            return false;
        }
    };
    let stdout = String::from_utf8_lossy(&out.stdout);
    if let Some(line) = stdout.lines().find(|l| l.starts_with("Updated yt-dlp to")) {
        info!("{line}");
        last.1 = Some(Instant::now());
        return true;
    }
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let why = stderr
            .lines()
            .rfind(|l| l.starts_with("ERROR:"))
            .unwrap_or("yt-dlp failed");
        if CANT_UPDATE.iter().any(|reason| why.contains(reason)) {
            UPDATES_OFF.store(true, Ordering::Relaxed);
            warn!(
                "yt-dlp can't update itself, so delete {YTDLP_BIN} to get a new one. It says: {why}"
            );
        } else {
            warn!("yt-dlp: couldn't update, trying again later: {why}");
        }
    }
    false
}

async fn search_best(js: &str, track: &Track) -> Result<String> {
    let wanted = togematch::Track {
        name: track.name.clone(),
        artist: track.artist.clone(),
        album: track.album.clone(),
        duration: track.duration,
        explicit: track.explicit,
    };
    let targets = togematch::queries(&wanted);
    info!("yt-dlp search: {}", targets[0]);
    let rows = flat_dump(js, &targets).await?;
    for c in &rows {
        if let Some(score) = togematch::score(&wanted, c) {
            debug!("yt-dlp candidate {} score={score}: {}", c.id, c.title);
        }
    }
    togematch::best(&wanted, &rows)
        .map(|c| c.id.clone())
        .ok_or_else(|| NoMatch.into())
}

/// Nothing on `YouTube` is this track. Unlike a failed download, searching again
/// in a minute will not change that.
#[derive(Debug)]
pub struct NoMatch;

impl std::fmt::Display for NoMatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("no suitable YouTube match")
    }
}

impl std::error::Error for NoMatch {}

/// The finished file yt-dlp left in `dir`, skipping partial downloads.
async fn find_downloaded(dir: &Path) -> Result<PathBuf> {
    let mut rd = tokio::fs::read_dir(dir).await?;
    while let Some(entry) = rd.next_entry().await? {
        let name = entry.file_name().to_string_lossy().to_lowercase();
        if entry.file_type().await?.is_file()
            && !name.ends_with(".part")
            && !name.ends_with(".ytdl")
        {
            return Ok(entry.path());
        }
    }
    bail!("yt-dlp produced no file")
}

async fn download_audio(js: &str, out_tmpl: &Path, url: &str) -> Result<std::process::Output> {
    let (mut cmd, cookies) = ytdlp_cmd(js);
    let res = tokio::time::timeout(
        DOWNLOAD_TIMEOUT,
        cmd.args(["-f", "bestaudio/best", "--no-playlist", "--no-mtime", "-o"])
            .arg(out_tmpl)
            .arg("--")
            .arg(url)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .map_err(|_| anyhow!("yt-dlp download timed out"))?;
    if let Some(ref c) = cookies {
        c.save();
    }
    if let Ok(ref out) = res {
        let err = String::from_utf8_lossy(&out.stderr);
        check_cookies(&err);
    }
    res.map_err(Into::into)
}

/// `YouTube` n-sig needs a JS runtime, so yt-dlp always runs with the vendored Deno.
fn ytdlp_cmd(js: &str) -> (Command, Option<Cookies>) {
    let mut cmd = Command::new(YTDLP_BIN);
    cmd.args(["--js-runtimes", js]).stdin(Stdio::null());
    let cookies = current_cookies();
    if let Some(ref c) = cookies {
        cmd.arg("--cookies").arg(&c.copy);
    }
    (cmd, cookies)
}

/// One run's copy of the cookies file, removed when dropped.
struct Cookies {
    file: String,
    copy: PathBuf,
    /// the file as copied
    read: Vec<u8>,
}

impl Cookies {
    fn copy(file: &str) -> std::io::Result<Self> {
        static RUNS: AtomicU64 = AtomicU64::new(0);
        let run = RUNS.fetch_add(1, Ordering::Relaxed);
        let copy =
            std::env::temp_dir().join(format!("togewire-{}-{run}.cookies", std::process::id()));
        let read = {
            let _turn = COOKIES_TURN.lock();
            std::fs::read(file)?
        };
        let cookies = Cookies {
            file: file.to_string(),
            copy,
            read,
        };
        // one left behind by a run that was killed
        let _ = std::fs::remove_file(&cookies.copy);
        // only for us: it holds a logged-in session
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        options.open(&cookies.copy)?.write_all(&cookies.read)?;
        Ok(cookies)
    }

    /// Hands what yt-dlp learned back to the file, unless it was replaced meanwhile, by hand or
    /// by another run: that copy is as new.
    fn save(&self) {
        let Ok(learned) = std::fs::read(&self.copy) else {
            return;
        };
        if learned == self.read {
            return;
        }
        let _turn = COOKIES_TURN.lock();
        if std::fs::read(&self.file).is_ok_and(|now| now == self.read)
            && let Err(e) = std::fs::write(&self.file, learned)
        {
            warn!("cookies: couldn't save {}: {e}", self.file);
        }
    }
}

impl Drop for Cookies {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.copy);
    }
}

fn current_cookies() -> Option<Cookies> {
    if !logged_in() {
        return None;
    }
    let cookies_path = cookies_file();
    {
        let mut active = ACTIVE_COOKIES.lock();
        if *active != cookies_path {
            if let Some(path) = &cookies_path {
                info!("using YouTube cookies from {path}");
            } else if active.is_some() {
                warn!("YouTube cookies file removed; continuing without cookies");
            }
            *active = cookies_path.clone();
        }
    }
    cookies_path.and_then(|file| match Cookies::copy(&file) {
        Ok(cookies) => Some(cookies),
        Err(e) => {
            warn!("cookies: couldn't load {file}: {e}, continuing without cookies");
            None
        }
    })
}

fn check_cookies(stderr: &str) {
    if (stderr.contains("cookies are no longer valid")
        || stderr.contains("Your cookies are expired"))
        && !COOKIES_DEAD.swap(true, Ordering::Relaxed)
    {
        let file = cookies_file().unwrap_or_default();
        warn!(
            "cookies: YouTube no longer accepts the ones in {file}. Export new ones, see the README"
        );
    }
}

/// yt-dlp (and Deno unless one is on PATH) in data/bin, downloaded and SHA-256 verified on
/// first use (delete one to force a refresh). Returns the `--js-runtimes` spec.
async fn ensure_tools() -> Result<&'static str> {
    static SYSTEM_DENO: std::sync::LazyLock<bool> = std::sync::LazyLock::new(|| {
        // A packaged Deno (the container's) wins: Deno only publishes glibc builds.
        std::env::var_os("PATH")
            .is_some_and(|p| std::env::split_paths(&p).any(|d| d.join("deno").is_file()))
    });
    static JS_RUNTIME: OnceCell<String> = OnceCell::const_new();
    if !Path::new(YTDLP_BIN).is_file() || (!*SYSTEM_DENO && !Path::new(DENO_BIN).is_file()) {
        let mut last = UPDATES.lock().await;
        let arch = std::env::consts::ARCH;
        if !Path::new(YTDLP_BIN).is_file() {
            // Each build runs only on its own libc (gcompat is not enough). The system
            // ldd names it, as in detect-libc; a stray musl loader (musl-tools) does not.
            let musl = std::fs::read_to_string("/usr/bin/ldd").is_ok_and(|s| s.contains("musl"));
            let libc = if musl { "musllinux" } else { "linux" };
            let name = if arch == "aarch64" {
                format!("yt-dlp_{libc}_aarch64")
            } else {
                format!("yt-dlp_{libc}")
            };
            let base = "https://github.com/yt-dlp/yt-dlp/releases/latest/download";
            let sums = format!("{base}/SHA2-256SUMS");
            let bin = download_verified(&format!("{base}/{name}"), &sums, &name).await?;
            install_bin(YTDLP_BIN, &bin).await?;
            UPDATES_OFF.store(false, Ordering::Relaxed);
            last.0 = Some(Instant::now());
        }
        if !*SYSTEM_DENO && !Path::new(DENO_BIN).is_file() {
            let name = format!("deno-{arch}-unknown-linux-gnu.zip");
            let url = format!("https://github.com/denoland/deno/releases/latest/download/{name}");
            let zip = download_verified(&url, &format!("{url}.sha256sum"), &name).await?;
            install_bin(DENO_BIN, &unzip_single(&zip)?).await?;
        }
    }
    let spec = JS_RUNTIME
        .get_or_try_init(|| async {
            let spec = if *SYSTEM_DENO {
                "deno".to_string()
            } else {
                let path = tokio::fs::canonicalize(DENO_BIN).await?;
                format!("deno:{}", path.display())
            };
            info!("yt-dlp EJS runtime {spec}");
            Ok::<_, anyhow::Error>(spec)
        })
        .await?;
    Ok(spec)
}

/// Write an executable via a temp file, so an interrupted install never looks complete.
async fn install_bin(path: &str, bytes: &[u8]) -> Result<()> {
    let tmp = Path::new(path).with_extension("tmp");
    tokio::fs::write(&tmp, bytes).await?;
    tokio::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755)).await?;
    tokio::fs::rename(&tmp, path).await?;
    Ok(())
}

/// The single deflated entry of a Deno release zip (integrity is the zip's SHA-256).
fn unzip_single(zip: &[u8]) -> Result<Vec<u8>> {
    use std::io::Read;
    let le16 = |i: usize| {
        zip.get(i..i + 2)
            .map_or(0, |b| usize::from(b[0]) | usize::from(b[1]) << 8)
    };
    if !zip.starts_with(b"PK\x03\x04") || le16(8) != 8 {
        bail!("Deno release is not a deflated zip");
    }
    // Local header: 30 fixed bytes, then the file name and extra field.
    let data = zip.get(30 + le16(26) + le16(28)..).unwrap_or_default();
    let mut out = Vec::new();
    flate2::read::DeflateDecoder::new(data).read_to_end(&mut out)?;
    Ok(out)
}

/// Fetch a release asset and check it against the published SHA-256.
async fn download_verified(url: &str, sums_url: &str, name: &str) -> Result<Vec<u8>> {
    use sha2::{Digest, Sha256};
    info!("downloading {url}");
    tokio::fs::create_dir_all("data/bin").await?;
    // Tens of MB: a slow link needs longer than the client's 10s default.
    let get = |u: &str| crate::http().get(u).timeout(Duration::from_mins(10)).send();
    let sums = get(sums_url).await?.error_for_status()?.text().await?;
    let expected =
        parse_sha256sums(&sums, name).ok_or_else(|| anyhow!("checksum missing for {name}"))?;
    let bytes = get(url).await?.error_for_status()?.bytes().await?;
    let actual = crate::hex(&Sha256::digest(&bytes));
    if actual != expected {
        bail!("{name} checksum mismatch (expected {expected}, got {actual})");
    }
    Ok(bytes.to_vec())
}

/// Hash for `filename` from a sums file: GNU `hash  name` lines (name may carry a path or
/// a `*` binary marker), or a single-asset file holding just the hash.
fn parse_sha256sums(text: &str, filename: &str) -> Option<String> {
    text.lines().find_map(|line| {
        let mut parts = line.split_whitespace();
        let hash = parts.next()?;
        let named = parts.next().map(|n| n.trim_start_matches('*'));
        let ok = hash.len() == 64
            && hash.bytes().all(|b| b.is_ascii_hexdigit())
            && named.is_none_or(|n| n == filename || n.ends_with(&format!("/{filename}")));
        ok.then(|| hash.to_ascii_lowercase())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unzip_single_inflates_the_only_entry() {
        use std::io::Write;
        let mut deflate = flate2::write::DeflateEncoder::new(Vec::new(), Default::default());
        deflate.write_all(b"deno binary").unwrap();
        let mut zip = b"PK\x03\x04\x14\0\0\0\x08\0".to_vec(); // version, flags, deflate
        zip.resize(26, 0);
        zip.extend(b"\x04\0\0\0deno"); // name and extra lengths, name
        zip.extend(deflate.finish().unwrap());
        assert_eq!(unzip_single(&zip).unwrap(), b"deno binary");
        assert!(unzip_single(b"not a zip").is_err());
    }

    #[test]
    fn parse_sha256sums_gnu_format() {
        let text = "\
6bbb3d314cde4febe36e5fa1d55462e29c974f63444e707871834f6d8cc210ae  yt-dlp_linux
b6ce97646773070d7a7ffd6bbbdcaecb47c48483909c54c915bf08a7a9b5e0b1  yt-dlp_linux_aarch64
";
        assert_eq!(
            parse_sha256sums(text, "yt-dlp_linux").as_deref(),
            Some("6bbb3d314cde4febe36e5fa1d55462e29c974f63444e707871834f6d8cc210ae")
        );
        assert!(parse_sha256sums(text, "yt-dlp.exe").is_none());
        assert!(parse_sha256sums("not-a-hash  yt-dlp_linux", "yt-dlp_linux").is_none());
        // Deno publishes one hash per asset, sometimes bare.
        let bare = "6BBB3D314CDE4FEBE36E5FA1D55462E29C974F63444E707871834F6D8CC210AE\n";
        assert_eq!(
            parse_sha256sums(bare, "deno.zip").as_deref(),
            Some("6bbb3d314cde4febe36e5fa1d55462e29c974f63444e707871834f6d8cc210ae")
        );
    }
}
