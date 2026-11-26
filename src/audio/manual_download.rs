use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::fs::File;
use std::io::{self, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use super::TrackPreparer;
use crate::cache::{CACHE_DIR, sanitize_filename};
use crate::config;
use crate::spotify::SpotifyClient;
use crate::spotify::parsing::is_probable_spotify_id;

/// Maximum tracks processed in one manual album or playlist job.
const MAX_MANUAL_DOWNLOAD_TRACKS: usize = 500;
/// Maximum queued, active, and finished manual download entries retained at once.
const MAX_MANUAL_JOBS: usize = 20;
/// Unclaimed finished browser downloads are removed after this long.
const READY_EXPIRY: Duration = Duration::from_hours(48);
/// Failed job status stays visible to polls/tabs for this long unless dismissed or replaced.
const ERROR_EXPIRY: Duration = Duration::from_mins(5);
/// Standard ZIP32 32-bit boundary; sizes or offsets at/above this emit ZIP64 records.
const ZIP64_THRESHOLD: u64 = u32::MAX as u64;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ManualDownloadSource {
    Librespot,
    Ytdlp,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ManualDownloadQuality {
    Original,
    Opus(&'static str),
}

impl ManualDownloadQuality {
    pub fn parse(raw: &str) -> Result<Self, &'static str> {
        match raw.trim() {
            "max" | "original" => Ok(Self::Original),
            "112k" => Ok(Self::Opus("112k")),
            "144k" => Ok(Self::Opus("144k")),
            "192k" => Ok(Self::Opus("192k")),
            _ => Err("Invalid audio quality"),
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Original => "Max (FLAC / 320k Vorbis)",
            Self::Opus("112k") => "112 kbps Opus",
            Self::Opus("144k") => "144 kbps Opus",
            Self::Opus("192k") => "192 kbps Opus",
            Self::Opus(_) => "Opus",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ManualDownloadTarget {
    SpotifyTrack(String),
    SpotifyAlbum(String),
    SpotifyPlaylist(String),
    YoutubeVideo(String),
    YoutubePlaylist(String),
}

impl ManualDownloadTarget {
    pub fn is_youtube(&self) -> bool {
        matches!(self, Self::YoutubeVideo(_) | Self::YoutubePlaylist(_))
    }
}

pub fn is_youtube_video_id(id: &str) -> bool {
    id.len() == 11
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn is_youtube_list_id(id: &str) -> bool {
    (2..=128).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Parse a Spotify or YouTube track, album or playlist URL / URI.
pub fn parse_manual_download_target(raw: &str) -> Result<ManualDownloadTarget, &'static str> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("Enter a Spotify or YouTube link");
    }

    // Spotify URIs: spotify:track:<id>, spotify:album:<id>, spotify:playlist:<id>,
    // or spotify:user:<user>:playlist:<id>.
    if let Some(rest) = trimmed.strip_prefix("spotify:") {
        for (prefix, ctor) in [
            (
                "track:",
                ManualDownloadTarget::SpotifyTrack as fn(String) -> ManualDownloadTarget,
            ),
            ("album:", ManualDownloadTarget::SpotifyAlbum),
            ("playlist:", ManualDownloadTarget::SpotifyPlaylist),
        ] {
            if let Some(id) = rest
                .strip_prefix(prefix)
                .filter(|id| is_probable_spotify_id(id))
            {
                return Ok(ctor(id.to_string()));
            }
        }
        if let Some((_, id)) = rest.rsplit_once(":playlist:")
            && is_probable_spotify_id(id)
        {
            return Ok(ManualDownloadTarget::SpotifyPlaylist(id.to_string()));
        }
        return Err("Unsupported Spotify URI (use a track, album, or playlist)");
    }

    let without_scheme = trimmed
        .strip_prefix("https://")
        .or_else(|| trimmed.strip_prefix("http://"))
        .unwrap_or(trimmed);
    let without_frag = without_scheme.split('#').next().unwrap_or(without_scheme);
    let (host_and_path, query) = without_frag
        .split_once('?')
        .map_or((without_frag, ""), |(h, q)| (h, q));
    let Some((host, path)) = host_and_path.split_once('/') else {
        return Err("Enter a valid Spotify or YouTube link");
    };
    let host = host.to_ascii_lowercase();

    if host == "open.spotify.com" || host == "play.spotify.com" {
        let segments: Vec<&str> = path.split('/').collect();
        for pair in segments.windows(2) {
            let [kind, id] = pair else { continue };
            if !is_probable_spotify_id(id) {
                continue;
            }
            match *kind {
                "track" => return Ok(ManualDownloadTarget::SpotifyTrack((*id).to_string())),
                "album" => return Ok(ManualDownloadTarget::SpotifyAlbum((*id).to_string())),
                "playlist" => return Ok(ManualDownloadTarget::SpotifyPlaylist((*id).to_string())),
                _ => {}
            }
        }
        return Err("Unsupported Spotify link (use a track, album, or playlist)");
    }

    if host == "youtu.be" {
        let id = path.split('/').next().unwrap_or_default();
        if is_youtube_video_id(id) {
            return Ok(ManualDownloadTarget::YoutubeVideo(id.to_string()));
        }
        return Err("Invalid YouTube video link");
    }

    if matches!(
        host.as_str(),
        "youtube.com" | "www.youtube.com" | "m.youtube.com" | "music.youtube.com"
    ) {
        for prefix in ["shorts/", "embed/", "live/"] {
            if let Some(rest) = path.strip_prefix(prefix) {
                let id = rest.split('/').next().unwrap_or_default();
                if is_youtube_video_id(id) {
                    return Ok(ManualDownloadTarget::YoutubeVideo(id.to_string()));
                }
            }
        }
        if let Some(rest) = path.strip_prefix("browse/") {
            let id = rest.split('/').next().unwrap_or_default();
            if is_youtube_list_id(id) {
                return Ok(ManualDownloadTarget::YoutubePlaylist(format!(
                    "https://music.youtube.com/browse/{id}"
                )));
            }
        }
        let query_param = |key: &str| {
            query.split('&').find_map(|pair| {
                let (k, v) = pair.split_once('=')?;
                (k == key).then_some(v)
            })
        };
        if path == "watch" {
            if let Some(id) = query_param("v").filter(|id| is_youtube_video_id(id)) {
                return Ok(ManualDownloadTarget::YoutubeVideo(id.to_string()));
            }
            if let Some(list) = query_param("list").filter(|id| is_youtube_list_id(id)) {
                return Ok(ManualDownloadTarget::YoutubePlaylist(format!(
                    "https://www.youtube.com/playlist?list={list}"
                )));
            }
        }
        if path == "playlist"
            && let Some(list) = query_param("list").filter(|id| is_youtube_list_id(id))
        {
            return Ok(ManualDownloadTarget::YoutubePlaylist(format!(
                "https://www.youtube.com/playlist?list={list}"
            )));
        }
        return Err("Unsupported YouTube link (use a video, album, or playlist)");
    }

    Err("Enter a Spotify or YouTube link")
}

pub struct ReadyManualDownload {
    pub file_path: PathBuf,
    pub download_name: String,
    pub content_type: &'static str,
}

enum JobPhase {
    Queued,
    Running {
        done: usize,
        total: usize,
        current: String,
    },
    Ready {
        file_path: PathBuf,
        download_name: String,
        content_type: &'static str,
        skipped: usize,
    },
    Error(String),
}

struct JobState {
    id: String,
    work_dir: PathBuf,
    cancel: CancellationToken,
    stop: CancellationToken,
    pause_tx: watch::Sender<bool>,
    input: String,
    title: Option<String>,
    details: String,
    phase: JobPhase,
}

#[derive(Default)]
pub struct ManualDownloadManager {
    jobs: Arc<Mutex<Vec<JobState>>>,
    queue_lock: Arc<tokio::sync::Mutex<()>>,
}

fn job_to_json(job: &JobState) -> serde_json::Value {
    let is_paused = *job.pause_tx.borrow();
    let is_stopping = job.stop.is_cancelled();
    match &job.phase {
        JobPhase::Queued => json!({
            "id": job.id,
            "status": "queued",
            "input": job.input,
            "title": job.title,
            "details": job.details,
        }),
        JobPhase::Running {
            done,
            total,
            current,
        } => json!({
            "id": job.id,
            "status": if is_paused && !is_stopping { "paused" } else { "running" },
            "stopping": is_stopping,
            "input": job.input,
            "title": job.title,
            "details": job.details,
            "done": done,
            "total": total,
            "current": current,
        }),
        JobPhase::Ready {
            download_name,
            skipped,
            ..
        } => json!({
            "id": job.id,
            "status": "ready",
            "input": job.input,
            "title": job.title,
            "details": job.details,
            "filename": download_name,
            "skipped": skipped,
        }),
        JobPhase::Error(msg) => json!({
            "id": job.id,
            "status": "error",
            "input": job.input,
            "title": job.title,
            "details": job.details,
            "error": msg,
        }),
    }
}

impl ManualDownloadManager {
    #[allow(clippy::too_many_arguments)]
    pub fn start(
        &self,
        spotify: SpotifyClient,
        preparer: Arc<TrackPreparer>,
        shutdown: CancellationToken,
        url: &str,
        requested_source: Option<ManualDownloadSource>,
        quality_raw: &str,
        normalize: bool,
    ) -> Result<String, String> {
        let target = parse_manual_download_target(url).map_err(String::from)?;
        let quality = ManualDownloadQuality::parse(quality_raw).map_err(String::from)?;
        let can_use_librespot = config::get().read().is_authenticated()
            && config::premium() == Some(true)
            && config::use_librespot();

        let source = if target.is_youtube() {
            ManualDownloadSource::Ytdlp
        } else {
            match requested_source {
                Some(ManualDownloadSource::Librespot) if !can_use_librespot => {
                    return Err("Spotify Premium is required for Spotify downloads".into());
                }
                Some(s) => s,
                None if can_use_librespot => ManualDownloadSource::Librespot,
                None => ManualDownloadSource::Ytdlp,
            }
        };

        let norm_suffix = if normalize { " · Normalized" } else { "" };
        let details = match source {
            ManualDownloadSource::Librespot => {
                format!("Spotify · {}{norm_suffix}", quality.label())
            }
            ManualDownloadSource::Ytdlp => {
                format!("YouTube · Best (native Opus){norm_suffix}")
            }
        };

        let id = crate::random_hex(8);
        let work_dir = PathBuf::from(CACHE_DIR).join(format!("manual-{id}.ytwork"));
        let cancel = shutdown.child_token();
        let stop = cancel.child_token();
        let (pause_tx, pause_rx) = watch::channel(false);
        let input = url.trim().to_string();

        {
            let mut jobs = self.jobs.lock();
            if jobs.len() >= MAX_MANUAL_JOBS {
                return Err(format!(
                    "Download queue is full (max {MAX_MANUAL_JOBS}); dismiss finished items first"
                ));
            }
            jobs.push(JobState {
                id: id.clone(),
                work_dir: work_dir.clone(),
                cancel: cancel.clone(),
                stop: stop.clone(),
                pause_tx,
                input,
                title: None,
                details,
                phase: JobPhase::Queued,
            });
        }

        let jobs_arc = Arc::clone(&self.jobs);
        let queue_lock = Arc::clone(&self.queue_lock);
        let job_id = id.clone();
        tokio::spawn(async move {
            run_manual_download_job(
                jobs_arc, queue_lock, job_id, work_dir, cancel, stop, pause_rx, spotify, preparer,
                target, source, quality, normalize,
            )
            .await;
        });

        Ok(id)
    }

    pub fn pause(&self, id: &str) -> bool {
        let jobs = self.jobs.lock();
        let Some(job) = jobs.iter().find(|j| j.id == id) else {
            return false;
        };
        if !job.stop.is_cancelled()
            && matches!(job.phase, JobPhase::Running { total, .. } if total > 1)
        {
            job.pause_tx.send_replace(true);
            true
        } else {
            false
        }
    }

    pub fn resume(&self, id: &str) -> bool {
        let jobs = self.jobs.lock();
        let Some(job) = jobs.iter().find(|j| j.id == id) else {
            return false;
        };
        if matches!(job.phase, JobPhase::Running { .. }) {
            job.pause_tx.send_replace(false);
            true
        } else {
            false
        }
    }

    /// Stop downloading remaining tracks in a multi-track job and package the tracks
    /// completed so far into the output ZIP archive.
    pub fn stop(&self, id: &str) -> bool {
        let jobs = self.jobs.lock();
        let Some(job) = jobs.iter().find(|j| j.id == id) else {
            return false;
        };
        if matches!(job.phase, JobPhase::Running { total, .. } if total > 1) {
            job.stop.cancel();
            job.pause_tx.send_replace(false);
            true
        } else {
            false
        }
    }

    pub fn cancel(&self, id: &str) -> bool {
        // Running jobs delete their own work_dir after unwinding (once child processes and
        // blocking work exit); Queued jobs never created one; Ready jobs are cleaned up here.
        let taken = {
            let mut jobs = self.jobs.lock();
            jobs.iter().position(|j| j.id == id).map(|idx| {
                let job = jobs.remove(idx);
                job.cancel.cancel();
                matches!(job.phase, JobPhase::Ready { .. }).then_some(job.work_dir)
            })
        };
        let Some(ready_dir) = taken else {
            return false;
        };
        if let Some(dir) = ready_dir {
            tokio::spawn(async move {
                let _ = tokio::fs::remove_dir_all(dir).await;
            });
        }
        true
    }

    pub fn list_status(&self) -> serde_json::Value {
        let jobs = self.jobs.lock();
        let items: Vec<serde_json::Value> = jobs.iter().map(job_to_json).collect();
        json!({ "jobs": items })
    }

    pub fn get_ready_file(&self, id: &str) -> Option<ReadyManualDownload> {
        let jobs = self.jobs.lock();
        let job = jobs.iter().find(|j| j.id == id)?;
        match &job.phase {
            JobPhase::Ready {
                file_path,
                download_name,
                content_type,
                ..
            } => Some(ReadyManualDownload {
                file_path: file_path.clone(),
                download_name: download_name.clone(),
                content_type,
            }),
            _ => None,
        }
    }
}

fn begin_job(jobs_arc: &Mutex<Vec<JobState>>, job_id: &str) -> bool {
    let mut jobs = jobs_arc.lock();
    if let Some(job) = jobs.iter_mut().find(|j| j.id == job_id) {
        job.phase = JobPhase::Running {
            done: 0,
            total: 0,
            current: String::new(),
        };
        true
    } else {
        false
    }
}

fn set_job_title(jobs_arc: &Mutex<Vec<JobState>>, job_id: &str, kind: &str, title: &str) {
    let title = title.trim();
    if title.is_empty() {
        return;
    }
    let mut jobs = jobs_arc.lock();
    if let Some(job) = jobs.iter_mut().find(|j| j.id == job_id) {
        job.title = Some(format!("{kind}: {title}"));
    }
}

fn update_progress(
    jobs_arc: &Mutex<Vec<JobState>>,
    job_id: &str,
    done: usize,
    total: usize,
    current: String,
) {
    let mut jobs = jobs_arc.lock();
    if let Some(job) = jobs.iter_mut().find(|j| j.id == job_id) {
        job.phase = JobPhase::Running {
            done,
            total,
            current,
        };
    }
}

fn finish_job(jobs_arc: &Mutex<Vec<JobState>>, job_id: &str, phase: JobPhase) -> bool {
    let mut jobs = jobs_arc.lock();
    if let Some(job) = jobs.iter_mut().find(|j| j.id == job_id) {
        job.phase = phase;
        true
    } else {
        false
    }
}

async fn wait_if_paused(pause_rx: &mut watch::Receiver<bool>, stop: &CancellationToken) {
    tokio::select! {
        biased;
        () = stop.cancelled() => {}
        _ = pause_rx.wait_for(|&paused| !paused) => {}
    }
}

fn schedule_job_expiry(
    jobs_arc: Arc<Mutex<Vec<JobState>>>,
    job_id: String,
    cancel: CancellationToken,
    after: Duration,
) {
    tokio::spawn(async move {
        tokio::select! {
            () = cancel.cancelled() => return,
            () = tokio::time::sleep(after) => {}
        }
        let dir = {
            let mut jobs = jobs_arc.lock();
            jobs.iter().position(|j| j.id == job_id).and_then(|idx| {
                let job = jobs.remove(idx);
                matches!(job.phase, JobPhase::Ready { .. }).then_some(job.work_dir)
            })
        };
        if let Some(dir) = dir {
            let _ = tokio::fs::remove_dir_all(dir).await;
        }
    });
}

#[allow(clippy::too_many_arguments)]
async fn run_manual_download_job(
    jobs_arc: Arc<Mutex<Vec<JobState>>>,
    queue_lock: Arc<tokio::sync::Mutex<()>>,
    job_id: String,
    work_dir: PathBuf,
    cancel: CancellationToken,
    stop: CancellationToken,
    mut pause_rx: watch::Receiver<bool>,
    spotify: SpotifyClient,
    preparer: Arc<TrackPreparer>,
    target: ManualDownloadTarget,
    source: ManualDownloadSource,
    quality: ManualDownloadQuality,
    normalize: bool,
) {
    // Wait in strict FIFO order for earlier queued jobs, exiting immediately if cancelled.
    let _turn = tokio::select! {
        biased;
        () = cancel.cancelled() => return,
        guard = queue_lock.lock() => guard,
    };
    if cancel.is_cancelled() || !begin_job(&jobs_arc, &job_id) {
        return;
    }
    if let Err(e) = tokio::fs::create_dir_all(&work_dir).await {
        if finish_job(
            &jobs_arc,
            &job_id,
            JobPhase::Error(format!("Could not create download directory: {e}")),
        ) {
            schedule_job_expiry(jobs_arc, job_id, cancel, ERROR_EXPIRY);
        }
        return;
    }

    let downloaded = {
        let run = download_manual_target(
            &jobs_arc,
            &job_id,
            &work_dir,
            &stop,
            &mut pause_rx,
            &spotify,
            &preparer,
            target,
            source,
            quality,
            normalize,
        );
        tokio::select! {
            () = cancel.cancelled() => None,
            res = run => Some(res),
        }
    };

    // Package outside `select!` so cancellation waits for the blocking ZIP writer (which
    // polls `cancel` every 64 KB) to unwind before `work_dir` is deleted.
    let outcome = match downloaded {
        None => None,
        Some(Err(err)) => Some(Err(err)),
        Some(Ok((title, files, skipped, always_zip))) => {
            if cancel.is_cancelled() {
                None
            } else {
                let res =
                    finalize_collection(&work_dir, &title, files, skipped, &cancel, always_zip)
                        .await;
                if cancel.is_cancelled() {
                    None
                } else {
                    Some(res)
                }
            }
        }
    };

    let Some(outcome) = outcome else {
        let _ = tokio::fs::remove_dir_all(&work_dir).await;
        return;
    };

    match outcome {
        Ok((file_path, download_name, content_type, skipped)) => {
            info!("Manual download ready: {download_name}");
            if finish_job(
                &jobs_arc,
                &job_id,
                JobPhase::Ready {
                    file_path,
                    download_name,
                    content_type,
                    skipped,
                },
            ) {
                // Clean up if the browser never fetches or dismisses the file.
                schedule_job_expiry(jobs_arc, job_id, cancel, READY_EXPIRY);
            } else {
                let _ = tokio::fs::remove_dir_all(&work_dir).await;
            }
        }
        Err(err) => {
            warn!("Manual download failed: {err}");
            let _ = tokio::fs::remove_dir_all(&work_dir).await;
            if finish_job(&jobs_arc, &job_id, JobPhase::Error(err)) {
                schedule_job_expiry(jobs_arc, job_id, cancel, ERROR_EXPIRY);
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn download_manual_target(
    job_arc: &Mutex<Vec<JobState>>,
    job_id: &str,
    work_dir: &Path,
    stop: &CancellationToken,
    pause_rx: &mut watch::Receiver<bool>,
    spotify: &SpotifyClient,
    preparer: &TrackPreparer,
    target: ManualDownloadTarget,
    source: ManualDownloadSource,
    quality: ManualDownloadQuality,
    normalize: bool,
) -> Result<(String, Vec<PathBuf>, usize, bool), String> {
    match target {
        ManualDownloadTarget::SpotifyTrack(id) => {
            let track = spotify
                .resolve_track(&id)
                .await
                .map_err(|e| e.to_string())?;
            let label = format!("{} - {}", track.artist, track.name);
            update_progress(job_arc, job_id, 0, 1, label);
            wait_if_paused(pause_rx, stop).await;
            let path = preparer
                .manual_download_spotify_track(&track, work_dir, source, quality, normalize)
                .await?;
            Ok((String::new(), vec![path], 0, false))
        }
        ManualDownloadTarget::YoutubeVideo(video_id) => {
            update_progress(job_arc, job_id, 0, 1, video_id.clone());
            wait_if_paused(pause_rx, stop).await;
            let path = preparer
                .manual_download_youtube_video(&video_id, work_dir, normalize)
                .await?;
            Ok((String::new(), vec![path], 0, false))
        }
        ManualDownloadTarget::SpotifyAlbum(id) => {
            let (title, tracks) = spotify
                .resolve_album(&id, MAX_MANUAL_DOWNLOAD_TRACKS)
                .await
                .map_err(|e| e.to_string())?;
            let (files, skipped) = download_collection_items(
                job_arc,
                job_id,
                stop,
                pause_rx,
                "Album",
                &title,
                tracks,
                |t| format!("{} - {}", t.artist, t.name),
                |track| async move {
                    preparer
                        .manual_download_spotify_track(&track, work_dir, source, quality, normalize)
                        .await
                },
            )
            .await?;
            Ok((title, files, skipped, stop.is_cancelled()))
        }
        ManualDownloadTarget::SpotifyPlaylist(id) => {
            let (title, tracks) = spotify
                .resolve_playlist(&id, MAX_MANUAL_DOWNLOAD_TRACKS)
                .await
                .map_err(|e| e.to_string())?;
            let (files, skipped) = download_collection_items(
                job_arc,
                job_id,
                stop,
                pause_rx,
                "Spotify playlist",
                &title,
                tracks,
                |t| format!("{} - {}", t.artist, t.name),
                |track| async move {
                    preparer
                        .manual_download_spotify_track(&track, work_dir, source, quality, normalize)
                        .await
                },
            )
            .await?;
            Ok((title, files, skipped, stop.is_cancelled()))
        }
        ManualDownloadTarget::YoutubePlaylist(url) => {
            let (title, items) = preparer
                .resolve_youtube_playlist(&url, MAX_MANUAL_DOWNLOAD_TRACKS)
                .await?;
            let (files, skipped) = download_collection_items(
                job_arc,
                job_id,
                stop,
                pause_rx,
                "YouTube playlist",
                &title,
                items,
                |(_, video_title)| video_title.clone(),
                |(video_id, _)| async move {
                    preparer
                        .manual_download_youtube_video(&video_id, work_dir, normalize)
                        .await
                },
            )
            .await?;
            Ok((title, files, skipped, stop.is_cancelled()))
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn download_collection_items<T, F, Fut>(
    job_arc: &Mutex<Vec<JobState>>,
    job_id: &str,
    stop: &CancellationToken,
    pause_rx: &mut watch::Receiver<bool>,
    kind: &str,
    title: &str,
    items: Vec<T>,
    mut label_of: impl FnMut(&T) -> String,
    mut download_one: F,
) -> Result<(Vec<PathBuf>, usize), String>
where
    F: FnMut(T) -> Fut,
    Fut: std::future::Future<Output = Result<PathBuf, String>>,
{
    let items: Vec<_> = items.into_iter().take(MAX_MANUAL_DOWNLOAD_TRACKS).collect();
    if items.is_empty() {
        return Err("No tracks found".into());
    }
    set_job_title(job_arc, job_id, kind, title);
    let total = items.len();
    let mut files = Vec::with_capacity(total);
    let mut skipped = 0usize;
    for (idx, item) in items.into_iter().enumerate() {
        if stop.is_cancelled() && !files.is_empty() {
            break;
        }
        let label = label_of(&item);
        update_progress(job_arc, job_id, idx, total, label.clone());
        wait_if_paused(pause_rx, stop).await;
        if stop.is_cancelled() && !files.is_empty() {
            break;
        }
        let dl = download_one(item);
        let res = if !files.is_empty() {
            tokio::select! {
                biased;
                () = stop.cancelled() => break,
                r = dl => r,
            }
        } else {
            dl.await
        };
        match res {
            Ok(path) => files.push(path),
            Err(e) => {
                warn!("Skipping {label}: {e}");
                skipped += 1;
            }
        }
    }
    Ok((files, skipped))
}

async fn finalize_collection(
    work_dir: &Path,
    title: &str,
    files: Vec<PathBuf>,
    skipped: usize,
    cancel: &CancellationToken,
    always_zip: bool,
) -> Result<(PathBuf, String, &'static str, usize), String> {
    if files.is_empty() {
        return Err("All tracks in the collection failed to download".into());
    }
    if files.len() == 1 && !always_zip {
        let path = files.into_iter().next().unwrap();
        let (name, mime) = single_file_meta(&path);
        return Ok((path, name, mime, skipped));
    }
    let safe_title = sanitize_filename(title, "download");
    let zip_name = format!("{safe_title}.zip");
    let zip_path = work_dir.join("archive.zip");
    let zip_out = zip_path.clone();
    let cancel_zip = cancel.clone();
    tokio::task::spawn_blocking(move || -> io::Result<()> {
        write_store_zip_with_threshold(&zip_out, &files, ZIP64_THRESHOLD, &cancel_zip)?;
        for f in &files {
            let _ = std::fs::remove_file(f);
        }
        Ok(())
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| format!("Failed to package zip archive: {e}"))?;
    Ok((zip_path, zip_name, "application/zip", skipped))
}

fn single_file_meta(path: &Path) -> (String, &'static str) {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "track.ogg".into());
    let mime = match path.extension().and_then(|e| e.to_str()) {
        Some("flac") => "audio/flac",
        _ => "audio/ogg",
    };
    (name, mime)
}

/// Write an uncompressed (`STORE`, method 0) ZIP / ZIP64 archive of `files` to `zip_path`.
pub(crate) fn write_store_zip(zip_path: &Path, files: &[PathBuf]) -> io::Result<()> {
    write_store_zip_with_threshold(zip_path, files, ZIP64_THRESHOLD, &CancellationToken::new())
}

/// Write an uncompressed (`STORE`, method 0) ZIP / ZIP64 archive with UTF-8 filenames
/// (flag bit 11) and modification timestamps (MS-DOS + `UT` 0x5455 Unix extra field).
fn write_store_zip_with_threshold(
    zip_path: &Path,
    files: &[PathBuf],
    zip64_threshold: u64,
    cancel: &CancellationToken,
) -> io::Result<()> {
    struct EntryMeta {
        name: Vec<u8>,
        crc: u32,
        size: u64,
        offset: u64,
        mtime: u16,
        mdate: u16,
        ut_extra: [u8; 9],
    }

    let mut out = BufWriter::with_capacity(256 * 1024, File::create(zip_path)?);
    let mut entries = Vec::with_capacity(files.len());
    let mut offset = 0u64;
    let mut buf = vec![0u8; 64 * 1024];

    for path in files {
        if cancel.is_cancelled() {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"));
        }
        let Some(name_os) = path.file_name() else {
            continue;
        };
        let name = name_os.to_string_lossy().into_owned().into_bytes();
        let (crc, size) = crc32_and_size(path, &mut buf, cancel)?;
        let unix_secs = std::fs::metadata(path)
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or_else(crate::now_unix, |d| d.as_secs() as i64);
        let (mtime, mdate) = unix_to_dos_datetime(unix_secs);
        let ts = (unix_secs.max(0) as u32).to_le_bytes();
        // Extended Timestamp (0x5455 "UT"): size=5, flags=1 (mtime), 4-byte Unix timestamp.
        let ut_extra = [0x55, 0x54, 0x05, 0x00, 0x01, ts[0], ts[1], ts[2], ts[3]];

        let size_zip64 = size >= zip64_threshold;
        let mut local_extra = Vec::with_capacity(ut_extra.len() + if size_zip64 { 20 } else { 0 });
        if size_zip64 {
            local_extra.extend_from_slice(&0x0001u16.to_le_bytes()); // ZIP64 extra ID
            local_extra.extend_from_slice(&16u16.to_le_bytes()); // data size: 2 x u64
            local_extra.extend_from_slice(&size.to_le_bytes()); // uncompressed size
            local_extra.extend_from_slice(&size.to_le_bytes()); // compressed size
        }
        local_extra.extend_from_slice(&ut_extra);

        let version_needed: u16 = if size_zip64 { 45 } else { 20 };
        let size32: u32 = if size_zip64 { u32::MAX } else { size as u32 };

        // Local file header (30 bytes + filename + extra). Bit 11 (0x0800) marks UTF-8.
        let mut local = Vec::with_capacity(30 + name.len() + local_extra.len());
        local.extend_from_slice(b"PK\x03\x04");
        local.extend_from_slice(&version_needed.to_le_bytes());
        local.extend_from_slice(&0x0800u16.to_le_bytes()); // flags: UTF-8
        local.extend_from_slice(&0u16.to_le_bytes()); // method: 0 (STORE)
        local.extend_from_slice(&mtime.to_le_bytes());
        local.extend_from_slice(&mdate.to_le_bytes());
        local.extend_from_slice(&crc.to_le_bytes());
        local.extend_from_slice(&size32.to_le_bytes());
        local.extend_from_slice(&size32.to_le_bytes());
        local.extend_from_slice(&(name.len() as u16).to_le_bytes());
        local.extend_from_slice(&(local_extra.len() as u16).to_le_bytes());
        local.extend_from_slice(&name);
        local.extend_from_slice(&local_extra);
        out.write_all(&local)?;

        let mut f = File::open(path)?;
        loop {
            if cancel.is_cancelled() {
                return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"));
            }
            let n = f.read(&mut buf)?;
            if n == 0 {
                break;
            }
            out.write_all(&buf[..n])?;
        }

        entries.push(EntryMeta {
            name,
            crc,
            size,
            offset,
            mtime,
            mdate,
            ut_extra,
        });
        offset = offset
            .checked_add(local.len() as u64)
            .and_then(|v| v.checked_add(size))
            .ok_or_else(|| io::Error::other("zip archive offset overflow"))?;
    }

    let cd_offset = offset;
    let mut cd_size = 0u64;
    for e in &entries {
        let size_zip64 = e.size >= zip64_threshold;
        let offset_zip64 = e.offset >= zip64_threshold;
        let mut zip64_fields = Vec::with_capacity(24);
        if size_zip64 {
            zip64_fields.extend_from_slice(&e.size.to_le_bytes());
            zip64_fields.extend_from_slice(&e.size.to_le_bytes());
        }
        if offset_zip64 {
            zip64_fields.extend_from_slice(&e.offset.to_le_bytes());
        }

        let mut cd_extra = Vec::with_capacity(
            e.ut_extra.len()
                + if zip64_fields.is_empty() {
                    0
                } else {
                    4 + zip64_fields.len()
                },
        );
        if !zip64_fields.is_empty() {
            cd_extra.extend_from_slice(&0x0001u16.to_le_bytes());
            cd_extra.extend_from_slice(&(zip64_fields.len() as u16).to_le_bytes());
            cd_extra.extend_from_slice(&zip64_fields);
        }
        cd_extra.extend_from_slice(&e.ut_extra);

        let version_needed: u16 = if zip64_fields.is_empty() { 20 } else { 45 };
        let size32: u32 = if size_zip64 { u32::MAX } else { e.size as u32 };
        let offset32: u32 = if offset_zip64 {
            u32::MAX
        } else {
            e.offset as u32
        };

        let mut cd = Vec::with_capacity(46 + e.name.len() + cd_extra.len());
        cd.extend_from_slice(b"PK\x01\x02");
        cd.extend_from_slice(&version_needed.to_le_bytes()); // version made by
        cd.extend_from_slice(&version_needed.to_le_bytes()); // version needed
        cd.extend_from_slice(&0x0800u16.to_le_bytes()); // flags: UTF-8
        cd.extend_from_slice(&0u16.to_le_bytes()); // method: STORE
        cd.extend_from_slice(&e.mtime.to_le_bytes());
        cd.extend_from_slice(&e.mdate.to_le_bytes());
        cd.extend_from_slice(&e.crc.to_le_bytes());
        cd.extend_from_slice(&size32.to_le_bytes());
        cd.extend_from_slice(&size32.to_le_bytes());
        cd.extend_from_slice(&(e.name.len() as u16).to_le_bytes());
        cd.extend_from_slice(&(cd_extra.len() as u16).to_le_bytes());
        cd.extend_from_slice(&0u16.to_le_bytes()); // comment len
        cd.extend_from_slice(&0u16.to_le_bytes()); // disk number
        cd.extend_from_slice(&0u16.to_le_bytes()); // internal attrs
        cd.extend_from_slice(&0u32.to_le_bytes()); // external attrs
        cd.extend_from_slice(&offset32.to_le_bytes());
        cd.extend_from_slice(&e.name);
        cd.extend_from_slice(&cd_extra);
        out.write_all(&cd)?;
        cd_size += cd.len() as u64;
    }

    let need_zip64_eocd = entries.len() >= usize::from(u16::MAX)
        || cd_size >= zip64_threshold
        || cd_offset >= zip64_threshold;
    if need_zip64_eocd {
        let zip64_eocd_offset = cd_offset + cd_size;
        let count64 = entries.len() as u64;

        let mut z64_eocd = [0u8; 56];
        z64_eocd[0..4].copy_from_slice(b"PK\x06\x06");
        z64_eocd[4..12].copy_from_slice(&44u64.to_le_bytes());
        z64_eocd[12..14].copy_from_slice(&45u16.to_le_bytes());
        z64_eocd[14..16].copy_from_slice(&45u16.to_le_bytes());
        z64_eocd[24..32].copy_from_slice(&count64.to_le_bytes());
        z64_eocd[32..40].copy_from_slice(&count64.to_le_bytes());
        z64_eocd[40..48].copy_from_slice(&cd_size.to_le_bytes());
        z64_eocd[48..56].copy_from_slice(&cd_offset.to_le_bytes());
        out.write_all(&z64_eocd)?;

        let mut locator = [0u8; 20];
        locator[0..4].copy_from_slice(b"PK\x06\x07");
        locator[8..16].copy_from_slice(&zip64_eocd_offset.to_le_bytes());
        locator[16..20].copy_from_slice(&1u32.to_le_bytes());
        out.write_all(&locator)?;
    }

    let count16 = u16::try_from(entries.len()).unwrap_or(u16::MAX);
    let cd_size32 = if cd_size >= zip64_threshold {
        u32::MAX
    } else {
        cd_size as u32
    };
    let cd_offset32 = if cd_offset >= zip64_threshold {
        u32::MAX
    } else {
        cd_offset as u32
    };
    let mut eocd = [0u8; 22];
    eocd[0..4].copy_from_slice(b"PK\x05\x06");
    eocd[8..10].copy_from_slice(&count16.to_le_bytes());
    eocd[10..12].copy_from_slice(&count16.to_le_bytes());
    eocd[12..16].copy_from_slice(&cd_size32.to_le_bytes());
    eocd[16..20].copy_from_slice(&cd_offset32.to_le_bytes());
    out.write_all(&eocd)?;
    out.flush()
}

fn unix_to_dos_datetime(unix_secs: i64) -> (u16, u16) {
    let days = unix_secs.div_euclid(86_400);
    let secs = unix_secs.rem_euclid(86_400) as u32;
    let hour = secs / 3600;
    let minute = (secs % 3600) / 60;
    let second = secs % 60;

    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = (z - era * 146_097) as u32;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = (y + i64::from(month <= 2)).clamp(1980, 2107) as u32;

    let mtime = ((hour << 11) | (minute << 5) | (second / 2)) as u16;
    let mdate = (((year - 1980) << 9) | (month << 5) | day) as u16;
    (mtime, mdate)
}

fn crc32_and_size(
    path: &Path,
    buf: &mut [u8],
    cancel: &CancellationToken,
) -> io::Result<(u32, u64)> {
    let mut f = File::open(path)?;
    let mut crc = flate2::Crc::new();
    let mut total = 0u64;
    loop {
        if cancel.is_cancelled() {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"));
        }
        let n = f.read(buf)?;
        if n == 0 {
            break;
        }
        total += n as u64;
        crc.update(&buf[..n]);
    }
    Ok((crc.sum(), total))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_spotify_and_youtube_targets() {
        assert_eq!(
            parse_manual_download_target(
                "https://open.spotify.com/track/6rqhFgbbKwnb9MLmUQDhG6?si=abc"
            ),
            Ok(ManualDownloadTarget::SpotifyTrack(
                "6rqhFgbbKwnb9MLmUQDhG6".into()
            ))
        );
        assert_eq!(
            parse_manual_download_target(
                "https://open.spotify.com/intl-ja/album/1A2GTWGtFfWp7KSQTwWOyo"
            ),
            Ok(ManualDownloadTarget::SpotifyAlbum(
                "1A2GTWGtFfWp7KSQTwWOyo".into()
            ))
        );
        assert_eq!(
            parse_manual_download_target(
                "https://open.spotify.com/playlist/37i9dQZF1DXcBWIGoYBM5M"
            ),
            Ok(ManualDownloadTarget::SpotifyPlaylist(
                "37i9dQZF1DXcBWIGoYBM5M".into()
            ))
        );
        assert_eq!(
            parse_manual_download_target("spotify:track:6rqhFgbbKwnb9MLmUQDhG6"),
            Ok(ManualDownloadTarget::SpotifyTrack(
                "6rqhFgbbKwnb9MLmUQDhG6".into()
            ))
        );
        assert_eq!(
            parse_manual_download_target("https://www.youtube.com/watch?v=dQw4w9WgXcQ"),
            Ok(ManualDownloadTarget::YoutubeVideo("dQw4w9WgXcQ".into()))
        );
        assert_eq!(
            parse_manual_download_target("https://youtu.be/dQw4w9WgXcQ?si=123"),
            Ok(ManualDownloadTarget::YoutubeVideo("dQw4w9WgXcQ".into()))
        );
        assert_eq!(
            parse_manual_download_target(
                "https://music.youtube.com/playlist?list=OLAK5uy_1234567890abcdef"
            ),
            Ok(ManualDownloadTarget::YoutubePlaylist(
                "https://www.youtube.com/playlist?list=OLAK5uy_1234567890abcdef".into()
            ))
        );
        assert!(parse_manual_download_target("https://example.com/song.mp3").is_err());
    }

    #[test]
    fn store_zip_writes_valid_archive() {
        let dir = std::env::temp_dir().join(format!("togewire-zip-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let f1 = dir.join("Artist - Track 1.ogg");
        let f2 = dir.join("結束バンド - Track 2.flac");
        std::fs::write(&f1, b"123456789").unwrap();
        std::fs::write(&f2, b"flac-data").unwrap();

        let zip = dir.join("out.zip");
        let cancel = CancellationToken::new();
        write_store_zip_with_threshold(&zip, &[f1.clone(), f2.clone()], ZIP64_THRESHOLD, &cancel)
            .unwrap();
        let bytes = std::fs::read(&zip).unwrap();
        assert!(bytes.starts_with(b"PK\x03\x04"));
        // MS-DOS mtime and mdate (bytes 10..14) must be populated (not 0 = 1980-01-01).
        assert_ne!(u16::from_le_bytes(bytes[10..12].try_into().unwrap()), 0);
        assert_ne!(u16::from_le_bytes(bytes[12..14].try_into().unwrap()), 0);
        // IEEE 802.3 CRC-32 of "123456789" is 0xCBF43926.
        assert_eq!(
            u32::from_le_bytes(bytes[14..18].try_into().unwrap()),
            0xCBF4_3926
        );
        // Extra field length (bytes 28..30) is 9 bytes for the 0x5455 "UT" Unix timestamp.
        assert_eq!(u16::from_le_bytes(bytes[28..30].try_into().unwrap()), 9);
        assert!(bytes.ends_with(&[0, 0]));

        // Forced ZIP64 threshold: walk EOCD -> ZIP64 locator -> ZIP64 EOCD -> CD -> local entries.
        let zip64 = dir.join("out64.zip");
        write_store_zip_with_threshold(&zip64, &[f1, f2], 8, &cancel).unwrap();
        let z64 = std::fs::read(&zip64).unwrap();
        let u16_at = |off: usize| u16::from_le_bytes(z64[off..off + 2].try_into().unwrap());
        let u32_at = |off: usize| u32::from_le_bytes(z64[off..off + 4].try_into().unwrap());
        let u64_at = |off: usize| u64::from_le_bytes(z64[off..off + 8].try_into().unwrap());

        let eocd = z64.len() - 22;
        assert_eq!(&z64[eocd..eocd + 4], b"PK\x05\x06");
        assert_eq!(u32_at(eocd + 12), u32::MAX);
        assert_eq!(u32_at(eocd + 16), u32::MAX);

        let loc = eocd - 20;
        assert_eq!(&z64[loc..loc + 4], b"PK\x06\x07");
        let z64_eocd = u64_at(loc + 8) as usize;
        assert_eq!(&z64[z64_eocd..z64_eocd + 4], b"PK\x06\x06");
        assert_eq!(u64_at(z64_eocd + 24), 2);
        let cd_size = u64_at(z64_eocd + 40) as usize;
        let cd_off = u64_at(z64_eocd + 48) as usize;
        assert_eq!(cd_off + cd_size, z64_eocd);

        let mut pos = cd_off;
        let mut extracted = Vec::new();
        while pos < cd_off + cd_size {
            assert_eq!(&z64[pos..pos + 4], b"PK\x01\x02");
            let size32 = u32_at(pos + 24);
            let name_len = u16_at(pos + 28) as usize;
            let extra_len = u16_at(pos + 30) as usize;
            let offset32 = u32_at(pos + 42);
            let name = std::str::from_utf8(&z64[pos + 46..pos + 46 + name_len]).unwrap();
            let extra = pos + 46 + name_len;
            assert_eq!(u16_at(extra), 0x0001);
            let mut zpos = extra + 4;
            let size = if size32 == u32::MAX {
                let s = u64_at(zpos) as usize;
                zpos += 16;
                s
            } else {
                size32 as usize
            };
            let local_off = if offset32 == u32::MAX {
                u64_at(zpos) as usize
            } else {
                offset32 as usize
            };
            assert_eq!(&z64[local_off..local_off + 4], b"PK\x03\x04");
            let l_name_len = u16_at(local_off + 26) as usize;
            let l_extra_len = u16_at(local_off + 28) as usize;
            let data_off = local_off + 30 + l_name_len + l_extra_len;
            extracted.push((name.to_string(), z64[data_off..data_off + size].to_vec()));
            pos += 46 + name_len + extra_len;
        }
        assert_eq!(pos, cd_off + cd_size);
        assert_eq!(
            extracted,
            vec![
                ("Artist - Track 1.ogg".into(), b"123456789".to_vec()),
                ("結束バンド - Track 2.flac".into(), b"flac-data".to_vec()),
            ]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn queue_tracks_multiple_jobs_and_cancels_independently() {
        let mgr = ManualDownloadManager::default();
        let make_job = |id: &str, phase: JobPhase| {
            let (pause_tx, _) = watch::channel(false);
            let cancel = CancellationToken::new();
            let stop = cancel.child_token();
            JobState {
                id: id.into(),
                work_dir: PathBuf::from(format!("/tmp/togewire-nonexistent-{id}")),
                cancel,
                stop,
                pause_tx,
                input: format!("https://youtu.be/{id}"),
                title: None,
                details: "YouTube · Best (native Opus)".into(),
                phase,
            }
        };
        {
            let mut jobs = mgr.jobs.lock();
            jobs.push(make_job(
                "j1",
                JobPhase::Running {
                    done: 1,
                    total: 5,
                    current: "Track 2".into(),
                },
            ));
            jobs.push(make_job("j2", JobPhase::Queued));
            jobs.push(make_job(
                "j3",
                JobPhase::Running {
                    done: 0,
                    total: 1,
                    current: "Single".into(),
                },
            ));
        }

        let status_of = |id: &str| {
            mgr.list_status()["jobs"]
                .as_array()
                .unwrap()
                .iter()
                .find(|j| j["id"].as_str() == Some(id))
                .cloned()
        };

        // Only multi-track running jobs can be paused or stopped early.
        assert!(mgr.pause("j1"));
        assert!(!mgr.pause("j2"));
        assert!(!mgr.pause("j3"));
        assert!(!mgr.stop("j2"));
        assert!(!mgr.stop("j3"));
        assert_eq!(status_of("j1").unwrap()["status"], "paused");
        assert!(mgr.resume("j1"));
        assert_eq!(status_of("j1").unwrap()["status"], "running");

        // Stopping a paused multi-track job unpauses it and marks it stopping.
        assert!(mgr.pause("j1"));
        assert!(mgr.stop("j1"));
        let j1_status = status_of("j1").unwrap();
        assert_eq!(j1_status["status"], "running");
        assert_eq!(j1_status["stopping"], true);
        assert!(!mgr.pause("j1"));

        // Cancelling a queued job leaves running jobs intact.
        assert!(mgr.cancel("j2"));
        assert!(status_of("j2").is_none());
        let list = mgr.list_status();
        let ids: Vec<_> = list["jobs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|j| j["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, vec!["j1", "j3"]);
    }
}
