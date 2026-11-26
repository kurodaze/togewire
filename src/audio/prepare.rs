use parking_lot::Mutex;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};

use super::librespot_src::LibrespotFetcher;
use super::ytdlp::YtdlpFetcher;
use crate::cache::Manager as CacheManager;
use crate::spotify::SpotifyClient;
use crate::types::Track;

/// Nothing matched, and the search is deterministic, so repeating it just burns calls.
const NO_SOURCE_COOLDOWN: Duration = Duration::from_mins(10);
/// A download or encode failure is usually transient (`YouTube` 403s intermittently), so
/// hold off only long enough to avoid hammering.
const RETRY_COOLDOWN: Duration = Duration::from_secs(30);

pub enum PrepareError {
    Busy,
    /// Nothing on `YouTube` matches this track.
    NoSource,
    Failed(String),
}

struct InFlightGuard<'a> {
    in_flight: &'a Mutex<HashMap<String, tokio::sync::watch::Receiver<()>>>,
    track_id: String,
    _tx: tokio::sync::watch::Sender<()>,
}

impl Drop for InFlightGuard<'_> {
    fn drop(&mut self) {
        self.in_flight.lock().remove(&self.track_id);
    }
}

pub struct TrackPreparer {
    pub cache: Arc<CacheManager>,
    librespot: LibrespotFetcher,
    ytdlp: YtdlpFetcher,
    in_flight: Mutex<HashMap<String, tokio::sync::watch::Receiver<()>>>,
    /// Track id to the instant it may be attempted again.
    failed: Mutex<HashMap<String, Instant>>,
    /// One librespot/yt-dlp download at a time.
    download_lock: tokio::sync::Mutex<()>,
}

impl TrackPreparer {
    pub fn new(spotify: SpotifyClient, cache: Arc<CacheManager>) -> Self {
        let librespot = LibrespotFetcher::new(spotify.clone(), Arc::clone(&cache));
        let ytdlp = YtdlpFetcher::new(spotify, Arc::clone(&cache));
        Self {
            cache,
            librespot,
            ytdlp,
            in_flight: Mutex::new(HashMap::new()),
            failed: Mutex::new(HashMap::new()),
            download_lock: tokio::sync::Mutex::new(()),
        }
    }

    /// Drop the librespot session, so a re-paired account gets a fresh one.
    pub async fn reset_librespot(&self) {
        self.librespot.reset().await;
    }

    pub fn get_cached_path(&self, track: &Track) -> Option<String> {
        self.cache.get_for_track(track)
    }

    pub fn is_preparing(&self, track: &Track) -> bool {
        let in_flight = self.in_flight.lock();
        in_flight.contains_key(&track.id)
            || track.alt_ids.iter().any(|id| in_flight.contains_key(id))
    }

    pub fn recently_failed(&self, track: &Track) -> bool {
        let mut failed = self.failed.lock();
        if failed.len() > 64 {
            let now = Instant::now();
            failed.retain(|_, until| now < *until);
        }
        failed
            .get(&track.id)
            .is_some_and(|until| Instant::now() < *until)
    }

    pub async fn prepare_track(&self, track: &Track) -> Result<PathBuf, PrepareError> {
        if let Some(path) = self.cache.get_for_track(track) {
            return Ok(PathBuf::from(path));
        }
        if self.recently_failed(track) {
            return Err(PrepareError::Busy);
        }

        enum PrepRole {
            Leader(tokio::sync::watch::Sender<()>),
            Follower(tokio::sync::watch::Receiver<()>),
        }

        let role = {
            let mut in_flight = self.in_flight.lock();
            if let Some(existing) = in_flight
                .get(&track.id)
                .or_else(|| track.alt_ids.iter().find_map(|id| in_flight.get(id)))
            {
                PrepRole::Follower(existing.clone())
            } else {
                let (tx, rx) = tokio::sync::watch::channel(());
                in_flight.insert(track.id.clone(), rx);
                PrepRole::Leader(tx)
            }
        };

        match role {
            PrepRole::Leader(tx) => self.execute_prepare(track, tx).await,
            PrepRole::Follower(mut rx) => {
                // Another task is already preparing - wait for completion instead of busy polling.
                let _ = tokio::time::timeout(Duration::from_secs(60), rx.changed()).await;
                if let Some(path) = self.cache.get_for_track(track) {
                    Ok(PathBuf::from(path))
                } else {
                    Err(PrepareError::Busy)
                }
            }
        }
    }

    async fn execute_prepare(
        &self,
        track: &Track,
        tx: tokio::sync::watch::Sender<()>,
    ) -> Result<PathBuf, PrepareError> {
        let _guard = InFlightGuard {
            in_flight: &self.in_flight,
            track_id: track.id.clone(),
            _tx: tx,
        };

        let result = {
            let _download = self.download_lock.lock().await;
            if let Some(path) = self.cache.get_for_track(track) {
                Ok(PathBuf::from(path))
            } else if self.recently_failed(track) {
                debug!("skipping recent failure: {} - {}", track.artist, track.name);
                Err(PrepareError::Busy)
            } else {
                self.prepare_inner(track).await
            }
        };

        match &result {
            Ok(_) => {
                self.failed.lock().remove(&track.id);
            }
            Err(PrepareError::NoSource) => {
                self.failed
                    .lock()
                    .insert(track.id.clone(), Instant::now() + NO_SOURCE_COOLDOWN);
            }
            Err(PrepareError::Failed(_)) => {
                self.failed
                    .lock()
                    .insert(track.id.clone(), Instant::now() + RETRY_COOLDOWN);
            }
            Err(PrepareError::Busy) => {}
        }

        result
    }

    async fn prepare_inner(&self, track: &Track) -> Result<PathBuf, PrepareError> {
        let to_disk = crate::config::cache_enabled();
        let verb = if to_disk {
            "Cached"
        } else {
            "Prepared in memory"
        };
        if !crate::config::use_librespot()
            || crate::config::audio_source() == crate::config::AudioSource::Youtube
        {
            return self.fetch_via_ytdlp(track, to_disk, verb).await;
        }

        match self.librespot.fetch_track(track, to_disk).await {
            Ok(path) => {
                info!(
                    "{verb} via librespot: {} - {} ({})",
                    track.artist,
                    track.name,
                    path.display()
                );
                return Ok(path);
            }
            Err(e) => {
                warn!("librespot failed ({e}), falling back to yt-dlp");
            }
        }

        self.fetch_via_ytdlp(track, to_disk, verb).await
    }

    async fn fetch_via_ytdlp(
        &self,
        track: &Track,
        to_disk: bool,
        verb: &str,
    ) -> Result<PathBuf, PrepareError> {
        match self.ytdlp.fetch_track(track, to_disk).await {
            Ok(path) => {
                info!(
                    "{verb} via yt-dlp: {} - {} ({})",
                    track.artist,
                    track.name,
                    path.display()
                );
                Ok(path)
            }
            Err(e) if e.downcast_ref::<super::ytdlp::NoMatch>().is_some() => {
                Err(PrepareError::NoSource)
            }
            Err(e) => Err(PrepareError::Failed(e.to_string())),
        }
    }

    pub async fn manual_download_spotify_track(
        &self,
        track: &Track,
        dir: &std::path::Path,
        source: super::manual_download::ManualDownloadSource,
        quality: super::manual_download::ManualDownloadQuality,
        normalize: bool,
    ) -> Result<PathBuf, String> {
        let _download = self.download_lock.lock().await;
        match source {
            super::manual_download::ManualDownloadSource::Librespot => self
                .librespot
                .manual_download_track(track, dir, quality, normalize)
                .await
                .map_err(|e| e.to_string()),
            super::manual_download::ManualDownloadSource::Ytdlp => self
                .ytdlp
                .manual_download_spotify_track(track, dir, normalize)
                .await
                .map_err(|e| e.to_string()),
        }
    }

    pub async fn manual_download_youtube_video(
        &self,
        video_id: &str,
        dir: &std::path::Path,
        normalize: bool,
    ) -> Result<PathBuf, String> {
        let _download = self.download_lock.lock().await;
        self.ytdlp
            .manual_download_youtube_video(video_id, dir, normalize)
            .await
            .map_err(|e| e.to_string())
    }

    pub async fn resolve_youtube_playlist(
        &self,
        url: &str,
        max_items: usize,
    ) -> Result<(String, Vec<(String, String)>), String> {
        let _download = self.download_lock.lock().await;
        self.ytdlp
            .resolve_youtube_playlist(url, max_items)
            .await
            .map_err(|e| e.to_string())
    }
}
