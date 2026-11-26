use anyhow::{Result, anyhow, bail};
use std::fs::OpenOptions;
use std::io::{Read, Write};
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
use crate::spotify::SpotifyClient;
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
static ACTIVE_COOKIES: parking_lot::Mutex<Option<String>> = parking_lot::Mutex::new(None);
static CLI_COOKIES: parking_lot::Mutex<Option<String>> = parking_lot::Mutex::new(None);
static COOKIES_DEAD: AtomicBool = AtomicBool::new(false);
/// How long the cookies are used after YouTube asks to sign in.
const LOGGED_IN_FOR: Duration = Duration::from_secs(3600);
/// When YouTube last asked to sign in.
static ASKED: parking_lot::Mutex<Option<Instant>> = parking_lot::Mutex::new(None);

pub fn set_cli_cookies(path: Option<String>) {
    *CLI_COOKIES.lock() = path;
}

fn logged_in() -> bool {
    cookies_file().is_some() && ASKED.lock().is_some_and(|t| t.elapsed() < LOGGED_IN_FOR)
}

/// Finds a non-empty cookies file in `dir`, checking standard names first then any `*cookie*.txt` file.
fn find_cookies(dir: &str) -> Option<String> {
    let non_empty = |p: &Path| p.is_file() && p.metadata().is_ok_and(|m| m.len() > 0);
    for name in [
        "cookies.txt",
        "youtube-cookies.txt",
        "www.youtube.com_cookies.txt",
    ] {
        let p = Path::new(dir).join(name);
        if non_empty(&p) {
            return Some(p.to_string_lossy().into_owned());
        }
    }
    let rd = std::fs::read_dir(dir).ok()?;
    for entry in rd.flatten() {
        let name = entry.file_name().to_string_lossy().to_lowercase();
        let path = entry.path();
        if name.ends_with(".txt") && name.contains("cookie") && non_empty(&path) {
            return Some(path.to_string_lossy().into_owned());
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
    spotify: SpotifyClient,
    cache: Arc<CacheManager>,
}

impl YtdlpFetcher {
    pub fn new(spotify: SpotifyClient, cache: Arc<CacheManager>) -> Self {
        Self { spotify, cache }
    }

    async fn enrich_spotify_track(&self, track: &Track) -> Track {
        let mut enriched = track.clone();
        // Embed pages never supply track_number, so only treat its absence as incomplete when
        // paired with spclient (while still enriching playlists lacking cover/album/date).
        let needs_spclient_only =
            track.track_number.is_none() && crate::config::get().read().is_authenticated();
        if crate::spotify::parsing::is_probable_spotify_id(&track.id)
            && (track.cover_url.is_none()
                || track.album.is_empty()
                || track.release_date.is_none()
                || needs_spclient_only)
            && let Ok(mut resolved) = self.spotify.resolve_track(&track.id).await
        {
            resolved.fill_from(enriched);
            enriched = resolved;
        }
        enriched
    }

    pub async fn fetch_track(&self, track: &Track, to_disk: bool) -> Result<PathBuf> {
        let track = self.enrich_spotify_track(track).await;
        let js = ensure_tools().await?;
        let video_id = match ask_togematch_api(&track).await? {
            Some(id) => id,
            None => search_best(js, &track).await?,
        };
        debug!("yt-dlp matched video {video_id} for {}", track.id);

        let work_dir = super::TempDir(if to_disk {
            self.cache.track_path(&track, "ytwork")
        } else {
            std::env::temp_dir().join(format!("togewire-yt-{}.ytwork", crate::random_hex(6)))
        });
        self.download_to_cache(js, &track, &video_id, &work_dir.0, to_disk)
            .await
    }

    pub async fn manual_download_spotify_track(
        &self,
        track: &Track,
        dir: &Path,
        normalize: bool,
    ) -> Result<PathBuf> {
        let track = self.enrich_spotify_track(track).await;
        let js = ensure_tools().await?;
        let video_id = match ask_togematch_api(&track).await? {
            Some(id) => id,
            None => search_best(js, &track).await?,
        };
        debug!("yt-dlp matched video {video_id} for {}", track.id);

        let work_dir = super::TempDir(dir.join(format!("yt-{}.ytwork", crate::random_hex(6))));
        let downloaded = run_download_loop(js, &video_id, &work_dir.0, false).await?;
        let meta = track_to_audio_metadata(&track, true).await;
        self.finish_manual_ogg(&downloaded, &track, &meta, dir, normalize)
            .await
    }

    pub async fn manual_download_youtube_video(
        &self,
        video_id: &str,
        dir: &Path,
        normalize: bool,
    ) -> Result<PathBuf> {
        let js = ensure_tools().await?;
        let work_dir = super::TempDir(dir.join(format!("yt-{}.ytwork", crate::random_hex(6))));
        let downloaded = run_download_loop(js, video_id, &work_dir.0, true).await?;
        let (track, meta) = self.resolve_youtube_metadata(&work_dir.0, video_id).await;
        self.finish_manual_ogg(&downloaded, &track, &meta, dir, normalize)
            .await
    }

    async fn resolve_youtube_metadata(
        &self,
        work_dir: &Path,
        video_id: &str,
    ) -> (Track, AudioMetadata) {
        let data = tokio::fs::read_to_string(work_dir.join("dl.info.json"))
            .await
            .unwrap_or_default();
        let mut info = parse_youtube_info_json(&data, video_id);

        // 1. If togematch-api is configured, ask it first for a canonical catalogue match,
        // unless this upload is already an official "Provided to YouTube by" Art Track.
        let art_track = is_art_track(&info.candidate);
        let mut api_match = None;
        if art_track {
            debug!("Skipping togematch-api for {video_id}: already an official Art Track upload");
        } else if let Some(hit) =
            ask_togematch_api_for_youtube(&info.queries, &info.candidate, video_id).await
        {
            if let Some(ref sp_id) = hit.spotify_id
                && let Ok(sp_track) = self.spotify.resolve_track(sp_id).await
            {
                let meta = track_to_audio_metadata(&sp_track, true).await;
                return (sp_track, meta);
            }
            let canonical_q = format!("{} {}", hit.artist, hit.title);
            if !info.queries.contains(&canonical_q) {
                info.queries.insert(0, canonical_q);
            }
            api_match = Some(hit);
        }

        // 2. Try matching against Spotify for full metadata and 1:1 cover art (no Premium needed).
        if let Some(sp_track) = self
            .spotify
            .match_youtube_track(&info.queries, &info.candidate)
            .await
        {
            debug!(
                "Matched YouTube video {video_id} to Spotify track {} ({} - {})",
                sp_track.id, sp_track.artist, sp_track.name
            );
            let meta = track_to_audio_metadata(&sp_track, true).await;
            return (sp_track, meta);
        }

        // 3. Fallback when unpaired with Spotify: public Deezer, then iTunes Search for
        // 1:1 album cover & metadata.
        let catalogue = match ask_deezer_for_youtube(&info.queries, &info.candidate).await {
            Some(hit) => Some(hit),
            None => ask_itunes_for_youtube(&info.queries, &info.candidate).await,
        };
        if let Some(hit) = catalogue {
            debug!(
                "Matched YouTube video {video_id} via {} ({} - {})",
                hit.source, hit.artist, hit.title
            );
            let mut picture = None;
            if let Some(url) = &hit.cover_url
                && let Some(img) = super::download_image(url).await
            {
                let img = super::prepare_square_cover(img).await;
                debug!(
                    "Cover art for {video_id}: {} album cover ({})",
                    hit.source,
                    super::describe_image(&img)
                );
                picture = Some(img);
            }
            if picture.is_none()
                && let Some(url) = &info.thumb_url
                && let Some(raw_img) = super::download_image(url).await
            {
                let img = super::prepare_youtube_cover(raw_img).await;
                debug!(
                    "Cover art for {video_id}: {} cover unavailable, using YouTube thumbnail ({})",
                    hit.source,
                    super::describe_image(&img)
                );
                picture = Some(img);
            }
            // Official Art Tracks already carry label-supplied title/artist/album/date;
            // keep those and upgrade to the high-res square cover (plus track/disc numbers).
            let meta = if art_track {
                AudioMetadata {
                    album: if info.meta.album.is_empty() {
                        hit.album
                    } else {
                        info.meta.album
                    },
                    date: info.meta.date.or(hit.date),
                    track_number: hit.track_number,
                    disc_number: hit.disc_number,
                    picture,
                    ..info.meta
                }
            } else {
                AudioMetadata {
                    title: hit.title,
                    artist: hit.artist,
                    album: if hit.album.is_empty() {
                        info.meta.album
                    } else {
                        hit.album
                    },
                    album_artist: String::new(),
                    date: info.meta.date.or(hit.date),
                    track_number: hit.track_number,
                    disc_number: hit.disc_number,
                    picture,
                }
            };
            let track = Track {
                id: format!("yt:{video_id}"),
                artist: meta.artist.clone(),
                name: meta.title.clone(),
                ..Default::default()
            };
            return (track, meta);
        }

        // 4. Pure YouTube upload (e.g. bootleg/remix): apply togematch-api title/artist if matched,
        // and trim/center-crop the YouTube thumbnail to a clean 1:1 square JPEG.
        if let Some(hit) = api_match {
            info.meta.title = hit.title;
            if !hit.artist.is_empty() {
                info.meta.artist = hit.artist;
            }
        }
        if let Some(url) = info.thumb_url
            && let Some(raw_img) = super::download_image(&url).await
        {
            let img = super::prepare_youtube_cover(raw_img).await;
            debug!(
                "Cover art for {video_id}: no catalogue match, using YouTube thumbnail ({})",
                super::describe_image(&img)
            );
            info.meta.picture = Some(img);
        }
        let track = Track {
            id: format!("yt:{video_id}"),
            artist: info.meta.artist.clone(),
            name: info.meta.title.clone(),
            ..Default::default()
        };
        (track, info.meta)
    }

    async fn finish_manual_ogg(
        &self,
        downloaded: &Path,
        track: &Track,
        meta: &AudioMetadata,
        dir: &Path,
        normalize: bool,
    ) -> Result<PathBuf> {
        let bitrate = if super::is_ogg_copyable(downloaded).await {
            None
        } else {
            Some(crate::config::audio_bitrate())
        };
        let stem = crate::cache::file_stem(track);
        let dest = crate::cache::unique_path(dir, &stem, "ogg");
        let dest_tmp = super::TempFile(dest.with_extension("tmp.ogg"));
        super::write_cache_opus(downloaded, &dest_tmp.0, bitrate.as_deref(), normalize, meta)
            .await
            .map_err(anyhow::Error::msg)?;
        tokio::fs::rename(&dest_tmp.0, &dest).await?;
        Ok(dest)
    }

    pub async fn resolve_youtube_playlist(
        &self,
        url: &str,
        max_items: usize,
    ) -> Result<(String, Vec<(String, String)>)> {
        let js = ensure_tools().await?;
        let raw = run_dump_json(js, true, Some(max_items), &[url.to_string()]).await?;
        let (title, items) = parse_youtube_playlist(&raw);
        if items.is_empty() {
            bail!("No playable videos found in YouTube playlist");
        }
        Ok((title, items))
    }

    async fn download_to_cache(
        &self,
        js: &str,
        track: &Track,
        video_id: &str,
        work_dir: &Path,
        to_disk: bool,
    ) -> Result<PathBuf> {
        let downloaded = run_download_loop(js, video_id, work_dir, false).await?;
        debug!("yt-dlp downloaded {}", downloaded.display());

        let dest = if to_disk {
            self.cache.track_path(track, "ogg")
        } else {
            self.cache.mem_track_path(track, "ogg")
        };
        // Must end in a real media extension - ffmpeg ignores compound ".ogg.tmp".
        let dest_tmp = super::TempFile(dest.with_extension("tmp.ogg"));
        // YouTube titles, channels and thumbnails are discarded: Spotify's metadata only.
        let meta = track_to_audio_metadata(track, to_disk).await;
        // YouTube's Opus (format 251, ~160k) is remuxed as-is; anything else is encoded.
        let bitrate = if super::is_opus(&downloaded).await {
            None
        } else {
            Some(crate::config::audio_bitrate())
        };
        let normalize = crate::config::normalization() == crate::config::Normalization::Cache;
        let normalized = super::write_cache_opus(
            &downloaded,
            &dest_tmp.0,
            bitrate.as_deref(),
            normalize,
            &meta,
        )
        .await
        .map_err(anyhow::Error::msg)?;
        tokio::fs::rename(&dest_tmp.0, &dest).await?;
        let recorded_bitrate = bitrate.as_deref().unwrap_or("160k");
        if to_disk {
            self.cache.put(
                &track.id,
                &dest.to_string_lossy(),
                recorded_bitrate,
                normalized,
                "ytdlp",
                track.duration,
            );
        } else {
            self.cache.put_mem(
                &track.id,
                &dest.to_string_lossy(),
                recorded_bitrate,
                normalized,
                track.duration,
            );
        }
        Ok(dest)
    }
}

async fn run_download_loop(
    js: &str,
    video_id: &str,
    work_dir: &Path,
    write_info_json: bool,
) -> Result<PathBuf> {
    let url = format!("https://www.youtube.com/watch?v={video_id}");
    let out_tmpl = work_dir.join("dl.%(ext)s");
    let (mut retried, mut updated) = (false, false);
    loop {
        let logged_in_now = logged_in();
        let _ = tokio::fs::remove_dir_all(work_dir).await;
        tokio::fs::create_dir_all(work_dir).await?;
        let output = download_audio(js, &out_tmpl, &url, write_info_json).await?;
        if output.status.success() {
            return find_downloaded(work_dir).await;
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
    }
}

async fn track_to_audio_metadata(track: &Track, fetch_cover: bool) -> AudioMetadata {
    AudioMetadata {
        title: track.name.clone(),
        artist: track.artist.clone(),
        album: track.album.clone(),
        album_artist: track.album_artist.clone().unwrap_or_default(),
        date: track.release_year(),
        track_number: track.track_number,
        disc_number: track.disc_number,
        picture: if fetch_cover {
            super::fetch_cover(track).await
        } else {
            None
        },
    }
}

struct YoutubeVideoInfo {
    meta: AudioMetadata,
    thumb_url: Option<String>,
    candidate: Candidate,
    queries: Vec<String>,
}

fn parse_youtube_info_json(data: &str, video_id: &str) -> YoutubeVideoInfo {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(data) else {
        return YoutubeVideoInfo {
            meta: AudioMetadata {
                title: video_id.to_string(),
                ..Default::default()
            },
            thumb_url: None,
            candidate: Candidate {
                id: video_id.to_string(),
                title: video_id.to_string(),
                ..Default::default()
            },
            queries: Vec::new(),
        };
    };
    let str_field = |k: &str| {
        v.get(k)
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(ToOwned::to_owned)
    };
    let raw_title = str_field("title").unwrap_or_else(|| video_id.to_string());
    let raw_uploader = str_field("uploader")
        .or_else(|| str_field("channel"))
        .unwrap_or_default();
    let structured_track = str_field("track");
    let structured_artists = v
        .get("artists")
        .and_then(serde_json::Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join(", ")
        })
        .filter(|s| !s.is_empty())
        .or_else(|| str_field("artist"))
        .or_else(|| str_field("creator"));

    let (title, artist) = match (structured_track, structured_artists) {
        (Some(tr), Some(ar)) => (tr, ar),
        (Some(tr), None) => {
            let ch = raw_uploader
                .strip_suffix(" - Topic")
                .unwrap_or(&raw_uploader)
                .trim()
                .to_string();
            (tr, ch)
        }
        (None, Some(ar)) => (raw_title.clone(), ar),
        (None, None) => {
            if let Some((ar, tr)) = split_artist_title(&raw_title) {
                (tr, ar)
            } else {
                let ch = raw_uploader
                    .strip_suffix(" - Topic")
                    .unwrap_or(&raw_uploader)
                    .trim()
                    .to_string();
                (raw_title.clone(), ch)
            }
        }
    };

    let album = str_field("album").unwrap_or_default();
    let date = v
        .get("release_year")
        .and_then(|y| {
            y.as_i64()
                .map(|n| n.to_string())
                .or_else(|| y.as_str().map(ToOwned::to_owned))
        })
        .or_else(|| str_field("release_date"))
        .or_else(|| str_field("upload_date"))
        .and_then(|d| d.get(..4).map(ToOwned::to_owned));
    let thumb_url = str_field("thumbnail");
    let duration = (v
        .get("duration")
        .and_then(serde_json::Value::as_f64)
        .unwrap_or(0.0)
        * 1000.0) as i64;
    let description = str_field("description").unwrap_or_default();

    let mut queries = Vec::new();
    let mut push_query = |q: String| {
        let q = q.trim().to_string();
        if !q.is_empty() && q != video_id && !queries.contains(&q) {
            queries.push(q);
        }
    };
    let primary = if artist.is_empty() || title.to_lowercase().contains(&artist.to_lowercase()) {
        title.clone()
    } else {
        format!("{artist} {title}")
    };
    push_query(primary.clone());
    // Fallback without parenthetical/bracketed tags (e.g. "(Frenchcore)", "[Official Audio]")
    // for stricter search APIs like Deezer while keeping the exact query first.
    push_query(strip_bracketed(&primary));
    push_query(raw_title.clone());
    push_query(strip_bracketed(&raw_title));

    let candidate_uploader = if raw_uploader.is_empty() {
        artist.clone()
    } else {
        raw_uploader
    };
    YoutubeVideoInfo {
        meta: AudioMetadata {
            title,
            artist,
            album: album.clone(),
            album_artist: String::new(),
            date,
            track_number: None,
            disc_number: None,
            picture: None,
        },
        thumb_url,
        candidate: Candidate {
            id: video_id.to_string(),
            title: raw_title,
            uploader: candidate_uploader,
            description,
            album,
            duration,
        },
        queries,
    }
}

fn split_artist_title(raw: &str) -> Option<(String, String)> {
    for sep in [" - ", " \u{2013} ", " \u{2014} "] {
        if let Some((left, right)) = raw.split_once(sep) {
            let (left, right) = (left.trim(), right.trim());
            if !left.is_empty() && !right.is_empty() {
                return Some((left.to_string(), right.to_string()));
            }
        }
    }
    None
}

/// True when the upload's description carries YouTube's official label Art Track credit
/// (`Provided to YouTube by ... <title> · <artist>`).
fn is_art_track(candidate: &Candidate) -> bool {
    let desc = candidate.description.to_lowercase();
    let Some(start) = desc.find("provided to youtube by") else {
        return false;
    };
    let title = candidate.title.trim().to_lowercase();
    !title.is_empty()
        && desc[start..]
            .split_once(" · ")
            .is_some_and(|(credit, _)| credit.trim_end().ends_with(&title))
}

/// Remove parenthetical/bracketed suffixes (`(...)`, `[...]`, `【...】`) and collapse whitespace.
fn strip_bracketed(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut depth = 0u32;
    for ch in raw.chars() {
        match ch {
            '(' | '[' | '【' => depth += 1,
            ')' | ']' | '】' if depth > 0 => depth -= 1,
            _ if depth == 0 => out.push(ch),
            _ => {}
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn parse_youtube_playlist(stdout: &[u8]) -> (String, Vec<(String, String)>) {
    let mut playlist_title = None;
    let mut items = Vec::new();
    for line in String::from_utf8_lossy(stdout).lines() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let f = |k: &str| {
            v.get(k)
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(ToOwned::to_owned)
        };
        if playlist_title.is_none() {
            playlist_title = f("playlist_title")
                .or_else(|| f("playlist"))
                .or_else(|| f("album"));
        }
        if let Some(id) = f("id").filter(|id| super::manual_download::is_youtube_video_id(id)) {
            let title = f("title").unwrap_or_else(|| id.clone());
            items.push((id, title));
        }
    }
    (
        playlist_title.unwrap_or_else(|| "YouTube Playlist".into()),
        items,
    )
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
    let mut queries = Vec::new();
    if crate::spotify::parsing::is_probable_spotify_id(&track.id) {
        queries.push(format!("spotify:track:{}", track.id));
    }
    if !track.name.is_empty() {
        let text_q = if track.artist.is_empty() || track.artist == "Unknown Artist" {
            track.name.clone()
        } else {
            format!("{} {}", track.artist, track.name)
        };
        queries.push(text_q);
    }
    for query in queries {
        let Some(body) = query_togematch_api(&base, &query).await else {
            if API_DOWN_UNTIL
                .lock()
                .is_some_and(|until| Instant::now() < until)
            {
                return Ok(None);
            }
            continue;
        };
        let Some(hit) = body["tracks"].get(0) else {
            continue;
        };
        let Some(id) = extract_youtube_id_from_url(hit["youtube"].as_str()) else {
            if query.starts_with("spotify:") {
                return Err(NoMatch.into());
            }
            continue;
        };
        debug!(
            "togematch-api matched {} to {id} ({})",
            track.id, hit["source"]
        );
        return Ok(Some(id));
    }
    Ok(None)
}

struct ApiCatalogueHit {
    title: String,
    artist: String,
    spotify_id: Option<String>,
}

async fn ask_togematch_api_for_youtube(
    queries: &[String],
    candidate: &Candidate,
    video_id: &str,
) -> Option<ApiCatalogueHit> {
    let base = crate::config::togematch_api()?;
    if API_DOWN_UNTIL
        .lock()
        .is_some_and(|until| Instant::now() < until)
    {
        return None;
    }
    for query in queries {
        let Some(body) = query_togematch_api(&base, query).await else {
            if API_DOWN_UNTIL
                .lock()
                .is_some_and(|until| Instant::now() < until)
            {
                return None;
            }
            continue;
        };
        let Some(hit) = body["tracks"].get(0) else {
            continue;
        };
        let source = hit["source"].as_str().unwrap_or_default();
        if !matches!(source, "match" | "unofficial" | "manual") {
            continue;
        }
        let title = hit["title"].as_str().unwrap_or_default().trim().to_string();
        let artist = hit["artist"]
            .as_str()
            .unwrap_or_default()
            .trim()
            .to_string();
        let duration = hit["duration"].as_i64().unwrap_or(0);
        if title.is_empty() {
            continue;
        }
        let matched_vid = extract_youtube_id_from_url(hit["youtube"].as_str());
        let wanted = togematch::Track {
            name: title.clone(),
            artist: artist.clone(),
            album: String::new(),
            duration,
            explicit: false,
        };
        if matched_vid.as_deref() == Some(video_id)
            || togematch::score(&wanted, candidate).is_some()
        {
            let spotify_id = hit["query"]
                .as_str()
                .and_then(|q| q.strip_prefix("spotify:track:"))
                .filter(|id| crate::spotify::parsing::is_probable_spotify_id(id))
                .map(ToOwned::to_owned);
            return Some(ApiCatalogueHit {
                title,
                artist,
                spotify_id,
            });
        }
    }
    None
}

async fn query_togematch_api(base: &str, query: &str) -> Option<serde_json::Value> {
    let answer = crate::http()
        .get(format!("{base}/match"))
        .query(&[("q", query)])
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
    match answer {
        Ok(response) => match response.json().await {
            Ok(body) => Some(body),
            Err(e) => {
                info!("togematch-api at {base} didn't answer ({e}), searching YouTube");
                None
            }
        },
        Err(e) => {
            info!("togematch-api at {base} didn't answer ({e}), searching YouTube");
            None
        }
    }
}

fn extract_youtube_id_from_url(url: Option<&str>) -> Option<String> {
    url?.split_once("v=")
        .and_then(|(_, q)| q.split('&').next())
        .filter(|id| super::manual_download::is_youtube_video_id(id))
        .map(ToOwned::to_owned)
}

struct CatalogueHit {
    source: &'static str,
    title: String,
    artist: String,
    album: String,
    date: Option<String>,
    track_number: Option<i32>,
    disc_number: Option<i32>,
    cover_url: Option<String>,
}

async fn ask_deezer_for_youtube(queries: &[String], candidate: &Candidate) -> Option<CatalogueHit> {
    for query in queries {
        let Ok(resp) = crate::http()
            .get("https://api.deezer.com/search")
            .query(&[("q", query.as_str()), ("limit", "10")])
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
        else {
            continue;
        };
        let Ok(body) = resp.json::<serde_json::Value>().await else {
            continue;
        };
        let Some(items) = body.get("data").and_then(serde_json::Value::as_array) else {
            continue;
        };
        let mut best_hit: Option<(i64, CatalogueHit)> = None;
        for item in items {
            let Some(title) = item.get("title").and_then(serde_json::Value::as_str) else {
                continue;
            };
            let Some(artist) = item
                .get("artist")
                .and_then(|a| a.get("name"))
                .and_then(serde_json::Value::as_str)
            else {
                continue;
            };
            let album_obj = item.get("album");
            let album = album_obj
                .and_then(|a| a.get("title"))
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_string();
            let duration = item
                .get("duration")
                .and_then(serde_json::Value::as_i64)
                .unwrap_or(0)
                * 1000;
            let explicit = item
                .get("explicit_lyrics")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
            let wanted = togematch::Track {
                name: title.to_string(),
                artist: artist.to_string(),
                album: album.clone(),
                duration,
                explicit,
            };
            if let Some(score) = togematch::score(&wanted, candidate)
                && best_hit.as_ref().is_none_or(|(best, _)| score > *best)
            {
                let cover_url = album_obj
                    .and_then(|a| {
                        a.get("cover_xl")
                            .or_else(|| a.get("cover_big"))
                            .or_else(|| a.get("cover_medium"))
                    })
                    .and_then(serde_json::Value::as_str)
                    .filter(|s| !s.is_empty())
                    .map(ToOwned::to_owned);
                best_hit = Some((
                    score,
                    CatalogueHit {
                        source: "Deezer",
                        title: title.to_string(),
                        artist: artist.to_string(),
                        album,
                        date: None,
                        track_number: None,
                        disc_number: None,
                        cover_url,
                    },
                ));
            }
        }
        if let Some((_, hit)) = best_hit {
            return Some(hit);
        }
    }
    None
}

async fn ask_itunes_for_youtube(queries: &[String], candidate: &Candidate) -> Option<CatalogueHit> {
    let mode = crate::config::cover_art();
    for query in queries {
        let mut params = vec![
            ("term", query.as_str()),
            ("media", "music"),
            ("entity", "song"),
            ("limit", "10"),
        ];
        if let Some(country) = itunes_country(query) {
            params.push(("country", country));
        }
        let Ok(resp) = crate::http()
            .get("https://itunes.apple.com/search")
            .query(&params)
            .timeout(Duration::from_secs(8))
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
        else {
            continue;
        };
        let Ok(body) = resp.json::<serde_json::Value>().await else {
            continue;
        };
        if let Some(hit) = parse_itunes_best(&body, candidate, mode) {
            return Some(hit);
        }
    }
    None
}

fn itunes_country(query: &str) -> Option<&'static str> {
    if query
        .chars()
        .any(|c| ('\u{3040}'..='\u{30FF}').contains(&c))
    {
        Some("JP")
    } else if query.chars().any(|c| {
        ('\u{AC00}'..='\u{D7AF}').contains(&c)
            || ('\u{1100}'..='\u{11FF}').contains(&c)
            || ('\u{3130}'..='\u{318F}').contains(&c)
    }) {
        Some("KR")
    } else if query
        .chars()
        .any(|c| ('\u{4E00}'..='\u{9FFF}').contains(&c))
    {
        Some("JP")
    } else {
        None
    }
}

fn itunes_cover_url(url: &str, mode: crate::config::CoverArt) -> String {
    let target = match mode {
        crate::config::CoverArt::Listed => "640x640bb",
        crate::config::CoverArt::Size1280 => "1280x1280bb",
        crate::config::CoverArt::Original => "3000x3000bb",
    };
    if let Some((prefix, file)) = url.rsplit_once('/')
        && let Some((stem, ext)) = file.rsplit_once('.')
        && stem.ends_with("bb")
    {
        return format!("{prefix}/{target}.{ext}");
    }
    url.to_string()
}

fn parse_itunes_best(
    body: &serde_json::Value,
    candidate: &Candidate,
    mode: crate::config::CoverArt,
) -> Option<CatalogueHit> {
    let items = body.get("results").and_then(serde_json::Value::as_array)?;
    let mut best_hit: Option<(i64, CatalogueHit)> = None;
    for item in items {
        if item
            .get("kind")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|k| k != "song")
        {
            continue;
        }
        let Some(title) = item.get("trackName").and_then(serde_json::Value::as_str) else {
            continue;
        };
        let Some(artist) = item.get("artistName").and_then(serde_json::Value::as_str) else {
            continue;
        };
        let raw_album = item
            .get("collectionName")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        let album = raw_album
            .strip_suffix(" - Single")
            .or_else(|| raw_album.strip_suffix(" - EP"))
            .unwrap_or(raw_album)
            .to_string();
        let duration = item
            .get("trackTimeMillis")
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(0);
        let explicit = item
            .get("trackExplicitness")
            .and_then(serde_json::Value::as_str)
            == Some("explicit");
        let wanted = togematch::Track {
            name: title.to_string(),
            artist: artist.to_string(),
            album: album.clone(),
            duration,
            explicit,
        };
        if let Some(score) = togematch::score(&wanted, candidate)
            && best_hit.as_ref().is_none_or(|(best, _)| score > *best)
        {
            let cover_url = item
                .get("artworkUrl100")
                .or_else(|| item.get("artworkUrl60"))
                .and_then(serde_json::Value::as_str)
                .filter(|s| !s.is_empty())
                .map(|u| itunes_cover_url(u, mode));
            let date = item
                .get("releaseDate")
                .and_then(serde_json::Value::as_str)
                .and_then(|d| d.get(..4))
                .map(ToOwned::to_owned);
            let track_number = item
                .get("trackNumber")
                .and_then(serde_json::Value::as_i64)
                .and_then(|n| i32::try_from(n).ok())
                .filter(|&n| n > 0);
            let disc_number = item
                .get("discNumber")
                .and_then(serde_json::Value::as_i64)
                .and_then(|n| i32::try_from(n).ok())
                .filter(|&n| n > 0);
            best_hit = Some((
                score,
                CatalogueHit {
                    source: "iTunes",
                    title: title.to_string(),
                    artist: artist.to_string(),
                    album,
                    date,
                    track_number,
                    disc_number,
                    cover_url,
                },
            ));
        }
    }
    best_hit.map(|(_, hit)| hit)
}

/// Search results. A search that fails outright is an error, not "no match", which would stick.
async fn flat_dump(js: &str, targets: &[String]) -> Result<Vec<Candidate>> {
    let raw = run_dump_json(js, true, None, targets).await?;
    Ok(candidates(&raw))
}

async fn run_dump_json(
    js: &str,
    flat_playlist: bool,
    max_items: Option<usize>,
    targets: &[String],
) -> Result<Vec<u8>> {
    let mut updated = false;
    loop {
        let logged_in_now = logged_in();
        let (mut cmd, cookies) = ytdlp_cmd(js);
        if flat_playlist {
            cmd.arg("--flat-playlist");
        }
        cmd.args(["--dump-json", "--no-download"]);
        if let Some(limit) = max_items {
            cmd.args(["--playlist-end", &limit.to_string()]);
        }
        let out = tokio::time::timeout(
            SEARCH_TIMEOUT,
            cmd.arg("--")
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
        if !out.stdout.is_empty() || out.status.success() {
            return Ok(out.stdout);
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
    if UPDATES_OFF.load(Ordering::Relaxed) || !usable_bin(YTDLP_BIN) {
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

/// True when the candidate is a licensed Art Track (`Provided to YouTube by ... <title> ·`)
/// or an auto-generated `- Topic` channel upload (as opposed to an artist-channel music video).
fn is_licensed_upload(candidate: &Candidate) -> bool {
    candidate
        .uploader
        .trim()
        .to_lowercase()
        .ends_with("- topic")
        || is_art_track(candidate)
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
    let hit = match togematch::best(&wanted, &rows).cloned() {
        Some(hit) if is_licensed_upload(&hit) => Some(hit),
        other => {
            debug!("no licensed Art Track in yt-dlp search, checking YouTube Music...");
            let mut pool = music_dump(js, &wanted).await.unwrap_or_default();
            for c in &pool {
                if let Some(score) = togematch::score(&wanted, c) {
                    debug!(
                        "yt-dlp YouTube Music candidate {} score={score}: {}",
                        c.id, c.title
                    );
                }
            }
            if let Some(prev) = other {
                pool.push(prev);
            }
            togematch::best(&wanted, &pool).cloned()
        }
    };
    hit.or_else(|| togematch::top(&wanted, &rows).cloned())
        .map(|c| c.id)
        .ok_or_else(|| NoMatch.into())
}

async fn music_dump(js: &str, wanted: &togematch::Track) -> Result<Vec<Candidate>> {
    let target = togematch::music_search(wanted);
    let raw = run_dump_json(js, false, Some(togematch::MUSIC_SEARCH_RESULTS), &[target]).await?;
    Ok(candidates(&raw))
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

/// The finished file yt-dlp left in `dir`, skipping partial downloads and info json.
async fn find_downloaded(dir: &Path) -> Result<PathBuf> {
    let mut rd = tokio::fs::read_dir(dir).await?;
    while let Some(entry) = rd.next_entry().await? {
        let name = entry.file_name().to_string_lossy().to_lowercase();
        if entry.file_type().await?.is_file()
            && !name.ends_with(".part")
            && !name.ends_with(".ytdl")
            && !name.ends_with(".json")
        {
            return Ok(entry.path());
        }
    }
    bail!("yt-dlp produced no file")
}

async fn download_audio(
    js: &str,
    out_tmpl: &Path,
    url: &str,
    write_info_json: bool,
) -> Result<std::process::Output> {
    let (mut cmd, cookies) = ytdlp_cmd(js);
    cmd.args(["-f", "bestaudio/best", "--no-playlist", "--no-mtime"]);
    if write_info_json {
        cmd.arg("--write-info-json");
    }
    let res = tokio::time::timeout(
        DOWNLOAD_TIMEOUT,
        cmd.arg("-o")
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
        let dir = crate::shm_dir_path("togewire_cookies");
        crate::ensure_shm_dir(&dir);
        let copy = dir.join(format!("{}-{run}.cookies", crate::random_hex(6)));
        let read = {
            let _turn = COOKIES_TURN.lock();
            std::fs::read(file)?
        };
        let cookies = Cookies {
            file: file.to_string(),
            copy,
            read,
        };
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
        if !std::fs::read(&self.file).is_ok_and(|now| now == self.read) {
            return;
        }
        let dest = Path::new(&self.file);
        let tmp = dest.with_extension(format!("tmp.{}", crate::random_hex(6)));
        let mut opts = OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
        let atomic = opts
            .open(&tmp)
            .and_then(|mut f| f.write_all(&learned))
            .and_then(|()| std::fs::rename(&tmp, dest));
        if atomic.is_err() {
            let _ = std::fs::remove_file(&tmp);
            // Fallback for single-file Docker bind mounts where rename returns EBUSY/EXDEV.
            if let Err(e) = std::fs::write(dest, &learned) {
                warn!("cookies: couldn't save {}: {e}", self.file);
            }
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
    if !usable_bin(YTDLP_BIN) || (!*SYSTEM_DENO && !usable_bin(DENO_BIN)) {
        let mut last = UPDATES.lock().await;
        let arch = std::env::consts::ARCH;
        if !usable_bin(YTDLP_BIN) {
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
        if !*SYSTEM_DENO && !usable_bin(DENO_BIN) {
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

/// True if `path` exists and, when it is a dynamically linked ELF64 binary, its loader (`PT_INTERP`)
/// exists on this host (so a musl binary from a container is replaced on a glibc host and vice versa).
fn usable_bin(path: &str) -> bool {
    let Ok(file) = std::fs::File::open(path) else {
        return false;
    };
    if !file.metadata().is_ok_and(|m| m.is_file()) {
        return false;
    }
    let mut head = Vec::new();
    if file.take(4096).read_to_end(&mut head).is_err() {
        return false;
    }
    elf_interp(&head).is_none_or(|interp| Path::new(interp).exists())
}

/// The `PT_INTERP` path of a little-endian ELF64 binary if it lies within `head`.
fn elf_interp(head: &[u8]) -> Option<&str> {
    if head.len() < 64 || &head[0..4] != b"\x7fELF" || head[4] != 2 || head[5] != 1 {
        return None;
    }
    let u16_at = |i: usize| Some(u16::from_le_bytes(head.get(i..i + 2)?.try_into().ok()?));
    let u32_at = |i: usize| Some(u32::from_le_bytes(head.get(i..i + 4)?.try_into().ok()?));
    let u64_at = |i: usize| Some(u64::from_le_bytes(head.get(i..i + 8)?.try_into().ok()?));
    let phoff = usize::try_from(u64_at(32)?).ok()?;
    let phentsize = usize::from(u16_at(54)?);
    let phnum = usize::from(u16_at(56)?);
    if phentsize < 56 {
        return None;
    }
    for i in 0..phnum {
        let off = phoff.checked_add(i.checked_mul(phentsize)?)?;
        if u32_at(off)? == 3 {
            let start = usize::try_from(u64_at(off + 8)?).ok()?;
            let size = usize::try_from(u64_at(off + 32)?).ok()?;
            let bytes = head.get(start..start.checked_add(size)?)?;
            let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
            return std::str::from_utf8(&bytes[..end]).ok();
        }
    }
    None
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

    #[test]
    fn parse_youtube_info_splits_artist_title_when_unstructured() {
        let info = parse_youtube_info_json(
            r#"{
                "title": "snow strippers – under your spell (200BPM frenchcore bootleg) yubit",
                "uploader": "sveeme",
                "upload_date": "20240501",
                "duration": 162.18
            }"#,
            "abcdefghijk",
        );
        assert_eq!(info.meta.artist, "snow strippers");
        assert_eq!(
            info.meta.title,
            "under your spell (200BPM frenchcore bootleg) yubit"
        );
        assert_eq!(info.meta.date.as_deref(), Some("2024"));

        let structured = parse_youtube_info_json(
            r#"{
                "title": "Selfish",
                "track": "Selfish",
                "artists": ["AGONY", "4GET", "Faye"],
                "album": "Selfish",
                "release_year": 2026,
                "duration": 140.0
            }"#,
            "12345678901",
        );
        assert_eq!(structured.meta.artist, "AGONY, 4GET, Faye");
        assert_eq!(structured.meta.title, "Selfish");
        assert_eq!(structured.meta.album, "Selfish");
        assert_eq!(structured.meta.date.as_deref(), Some("2026"));

        // Genre/video tag in parentheses gets a bracket-stripped fallback query for Deezer.
        let tagged = parse_youtube_info_json(
            r#"{
                "title": "Nörva - Night Shift (Frenchcore)",
                "uploader": "Nörva",
                "duration": 150.0
            }"#,
            "vid12345678",
        );
        assert_eq!(
            tagged.queries,
            vec![
                "Nörva Night Shift (Frenchcore)",
                "Nörva Night Shift",
                "Nörva - Night Shift (Frenchcore)",
                "Nörva - Night Shift",
            ]
        );
        assert!(!is_art_track(&tagged.candidate));
        assert!(!is_licensed_upload(&tagged.candidate));

        let topic = Candidate {
            id: "vid87654321".into(),
            title: "Night Shift".into(),
            uploader: "norva - Topic".into(),
            ..Default::default()
        };
        assert!(is_licensed_upload(&topic));

        let art_track = parse_youtube_info_json(
            r#"{
                "title": "頭ん中DEAD END",
                "track": "頭ん中DEAD END",
                "artists": ["RetBear (unknown Vo:10fu)"],
                "uploader": "レトベア Official / RetBear",
                "album": "頭ん中DEAD END",
                "duration": 194.0,
                "description": "Provided to YouTube by JVCKENWOOD Victor Entertainment Corp.\n\n頭ん中DEAD END · RetBear (unknown Vo:10fu)\n\n頭ん中DEAD END\n\nAuto-generated by YouTube."
            }"#,
            "unLtq0BpEQc",
        );
        assert!(is_art_track(&art_track.candidate));

        // A re-upload copying the credit line under a modified title isn't treated as an Art Track.
        let copycat = parse_youtube_info_json(
            r#"{
                "title": "頭ん中DEAD END (Slowed + Reverb)",
                "uploader": "someone",
                "description": "Provided to YouTube by JVCKENWOOD Victor Entertainment Corp.\n\n頭ん中DEAD END · RetBear (unknown Vo:10fu)"
            }"#,
            "zzzzzzzzzzz",
        );
        assert!(!is_art_track(&copycat.candidate));
    }

    #[test]
    fn itunes_fallback_picks_high_res_cover_and_skips_other_masters() {
        assert_eq!(itunes_country("RetBear 頭ん中DEAD END"), Some("JP"));
        assert_eq!(itunes_country("아이유 밤편지"), Some("KR"));
        assert_eq!(itunes_country("norva night shift"), None);

        let art =
            "https://is1-ssl.mzstatic.com/image/thumb/Music122/v4/e9/VEATP-40044.jpg/100x100bb.jpg";
        assert_eq!(
            itunes_cover_url(art, crate::config::CoverArt::Listed),
            "https://is1-ssl.mzstatic.com/image/thumb/Music122/v4/e9/VEATP-40044.jpg/640x640bb.jpg"
        );
        assert_eq!(
            itunes_cover_url(art, crate::config::CoverArt::Size1280),
            "https://is1-ssl.mzstatic.com/image/thumb/Music122/v4/e9/VEATP-40044.jpg/1280x1280bb.jpg"
        );
        assert_eq!(
            itunes_cover_url(art, crate::config::CoverArt::Original),
            "https://is1-ssl.mzstatic.com/image/thumb/Music122/v4/e9/VEATP-40044.jpg/3000x3000bb.jpg"
        );

        let candidate = Candidate {
            id: "unLtq0BpEQc".into(),
            title: "頭ん中DEAD END".into(),
            uploader: "レトベア Official / RetBear".into(),
            description: "Provided to YouTube by JVCKENWOOD Victor Entertainment Corp.\n\n頭ん中DEAD END · RetBear (unknown Vo:10fu)\n\n頭ん中DEAD END".into(),
            album: "頭ん中DEAD END".into(),
            duration: 194_000,
        };
        let body = serde_json::json!({
            "results": [
                {
                    "kind": "song",
                    "artistName": "RetBear",
                    "trackName": "頭ん中DEAD END (off vocal)",
                    "collectionName": "頭ん中DEAD END - Single",
                    "trackTimeMillis": 193823,
                    "artworkUrl100": "https://is1-ssl.mzstatic.com/image/thumb/offvocal/100x100bb.jpg"
                },
                {
                    "kind": "song",
                    "artistName": "RetBear (unknown Vo:10fu)",
                    "trackName": "頭ん中DEAD END",
                    "collectionName": "頭ん中DEAD END - Single",
                    "trackTimeMillis": 193823,
                    "releaseDate": "2022-07-10T07:00:00Z",
                    "trackNumber": 1,
                    "discNumber": 1,
                    "artworkUrl100": art
                }
            ]
        });
        let hit = parse_itunes_best(&body, &candidate, crate::config::CoverArt::Original).unwrap();
        assert_eq!(hit.title, "頭ん中DEAD END");
        assert_eq!(hit.album, "頭ん中DEAD END");
        assert_eq!(hit.date.as_deref(), Some("2022"));
        assert_eq!((hit.track_number, hit.disc_number), (Some(1), Some(1)));
        assert_eq!(
            hit.cover_url.as_deref(),
            Some(
                "https://is1-ssl.mzstatic.com/image/thumb/Music122/v4/e9/VEATP-40044.jpg/3000x3000bb.jpg"
            )
        );

        // Only off-vocal present must return None (must not fall back to instrumental).
        let only_off_vocal = serde_json::json!({
            "results": [body["results"][0].clone()]
        });
        assert!(
            parse_itunes_best(&only_off_vocal, &candidate, crate::config::CoverArt::Listed)
                .is_none()
        );
    }

    #[test]
    fn elf_interp_reads_loader_path() {
        let mut elf = vec![0u8; 256];
        elf[0..6].copy_from_slice(b"\x7fELF\x02\x01");
        elf[32..40].copy_from_slice(&64u64.to_le_bytes());
        elf[54..56].copy_from_slice(&56u16.to_le_bytes());
        elf[56..58].copy_from_slice(&1u16.to_le_bytes());
        elf[64..68].copy_from_slice(&3u32.to_le_bytes());
        let interp = b"/lib/ld-musl-x86_64.so.1\0";
        elf[64 + 8..64 + 16].copy_from_slice(&128u64.to_le_bytes());
        elf[64 + 32..64 + 40].copy_from_slice(&(interp.len() as u64).to_le_bytes());
        elf[128..128 + interp.len()].copy_from_slice(interp);
        assert_eq!(elf_interp(&elf), Some("/lib/ld-musl-x86_64.so.1"));
        assert_eq!(elf_interp(b"#!/bin/sh\n"), None);
    }
}
