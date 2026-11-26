use librespot_audio::{AudioDecrypt, AudioFile};
use librespot_core::authentication::Credentials;
use librespot_core::config::SessionConfig;
use librespot_core::error::ErrorKind;
use librespot_core::spotify_uri::SpotifyUri;
use librespot_core::{FileId, Session, SpotifyId};
use librespot_metadata::Metadata;
use librespot_metadata::audio::AudioFileFormat;
use librespot_metadata::image::ImageSize;
use librespot_metadata::track::Track as LsTrack;
use parking_lot::Mutex;
use std::collections::HashSet;
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex as TokioMutex;
use tracing::{debug, info};

use super::AudioMetadata;
use crate::cache::Manager as CacheManager;
use crate::spotify::SpotifyClient;
use crate::types::Track;

/// After an auth failure librespot stays off this long instead of hammering the AP.
const AUTH_COOLDOWN: Duration = Duration::from_mins(5);
/// Minimum spacing between audio-key requests while Spotify's burst bucket has room.
const KEY_PACE_BURST: Duration = Duration::from_secs(2);
/// Spacing between audio-key requests once Spotify's burst bucket is depleted (~1 key / 15-18s).
const KEY_PACE_STEADY: Duration = Duration::from_secs(18);
/// Idle time after which Spotify's audio-key burst bucket has refilled.
const KEY_BUCKET_REFILL_IDLE: Duration = Duration::from_mins(2);
/// Backoff delays before retrying when Spotify's audio-key server throttles a request.
const KEY_THROTTLE_BACKOFFS: &[Duration] = &[
    Duration::from_secs(18),
    Duration::from_secs(25),
    Duration::from_secs(35),
];
/// Spotify prepends a fixed header of this size before the real audio container.
const SPOTIFY_HEADER: usize = 0xA7;

pub struct LibrespotFetcher {
    spotify: SpotifyClient,
    cache: Arc<CacheManager>,
    session: TokioMutex<Option<Session>>,
    cooldown_until: TokioMutex<Option<Instant>>,
    last_key_request: TokioMutex<Option<Instant>>,
    key_pace: Mutex<Duration>,
    /// Formats Spotify refused this session's account while granting another (FLAC without
    /// Lossless, 320 kbps on a 160 kbps plan). Each try costs a round trip and an error line.
    refused: Mutex<HashSet<AudioFileFormat>>,
    /// Formats Spotify has granted this session; a failure on one of these is an AP key
    /// throttle rather than a plan refusal, so lower formats aren't hammered in vain.
    granted: Mutex<HashSet<AudioFileFormat>>,
}

/// `Auth` drops the session and cools down; anything else just falls back to yt-dlp.
pub enum LibrespotError {
    Auth(String),
    Other(String),
}

use LibrespotError::{Auth, Other};

impl std::fmt::Display for LibrespotError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Auth(m) | Other(m) => f.write_str(m),
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum EncodedKind {
    Flac,
    OggVorbis,
    Other,
}

impl LibrespotFetcher {
    pub fn new(spotify: SpotifyClient, cache: Arc<CacheManager>) -> Self {
        Self {
            spotify,
            cache,
            session: TokioMutex::new(None),
            cooldown_until: TokioMutex::new(None),
            last_key_request: TokioMutex::new(None),
            key_pace: Mutex::new(KEY_PACE_BURST),
            refused: Mutex::new(HashSet::new()),
            granted: Mutex::new(HashSet::new()),
        }
    }

    /// Forget the session, e.g. once the Spotify account is disconnected.
    pub async fn reset(&self) {
        *self.session.lock().await = None;
        *self.cooldown_until.lock().await = None;
        *self.last_key_request.lock().await = None;
        *self.key_pace.lock() = KEY_PACE_BURST;
        self.refused.lock().clear();
        self.granted.lock().clear();
    }

    pub async fn fetch_track(
        &self,
        track: &Track,
        to_disk: bool,
    ) -> Result<PathBuf, LibrespotError> {
        self.with_cooldown(self.fetch_track_inner(track, to_disk))
            .await
    }

    pub async fn manual_download_track(
        &self,
        track: &Track,
        dir: &std::path::Path,
        quality: super::manual_download::ManualDownloadQuality,
        normalize: bool,
    ) -> Result<PathBuf, LibrespotError> {
        self.with_cooldown(self.manual_download_track_inner(track, dir, quality, normalize))
            .await
    }

    async fn with_cooldown<T>(
        &self,
        fut: impl std::future::Future<Output = Result<T, LibrespotError>>,
    ) -> Result<T, LibrespotError> {
        if self
            .cooldown_until
            .lock()
            .await
            .is_some_and(|until| Instant::now() < until)
        {
            return Err(Auth("librespot cooling down after auth failures".into()));
        }
        let res = fut.await;
        if let Err(Auth(_)) = res {
            *self.cooldown_until.lock().await = Some(Instant::now() + AUTH_COOLDOWN);
            *self.session.lock().await = None;
        }
        res
    }

    async fn fetch_track_inner(
        &self,
        track: &Track,
        to_disk: bool,
    ) -> Result<PathBuf, LibrespotError> {
        let dest = if to_disk {
            self.cache.track_path(track, "ogg")
        } else {
            self.cache.mem_track_path(track, "ogg")
        };
        // Must end in a real media extension - ffmpeg ignores compound ".ogg.tmp".
        let dest_tmp = super::TempFile(dest.with_extension("tmp.ogg"));
        // Keep large decrypted FLACs out of /dev/shm (64 MB default in containers) when
        // preparing in memory; only the final compact Opus file lives in /dev/shm.
        let raw_base = if to_disk {
            dest.clone()
        } else {
            std::env::temp_dir().join(format!("togewire-ls-{}.ogg", crate::random_hex(6)))
        };
        let (_kind, raw_tmp, audio_meta) =
            self.acquire_and_decrypt(track, &raw_base, to_disk).await?;

        let bitrate = crate::config::audio_bitrate();
        let normalize = crate::config::normalization() == crate::config::Normalization::Cache;
        let normalized = super::write_cache_opus(
            &raw_tmp.0,
            &dest_tmp.0,
            Some(&bitrate),
            normalize,
            &audio_meta,
        )
        .await
        .map_err(Other)?;
        drop(raw_tmp);
        tokio::fs::rename(&dest_tmp.0, &dest)
            .await
            .map_err(|e| Other(e.to_string()))?;

        if to_disk {
            self.cache.put(
                &track.id,
                &dest.to_string_lossy(),
                &bitrate,
                normalized,
                "librespot",
                track.duration,
            );
        } else {
            self.cache.put_mem(
                &track.id,
                &dest.to_string_lossy(),
                &bitrate,
                normalized,
                track.duration,
            );
        }
        Ok(dest)
    }

    async fn manual_download_track_inner(
        &self,
        track: &Track,
        dir: &std::path::Path,
        quality: super::manual_download::ManualDownloadQuality,
        normalize: bool,
    ) -> Result<PathBuf, LibrespotError> {
        use super::manual_download::ManualDownloadQuality;

        let temp_base = dir.join(format!("ls-{}.ogg", crate::random_hex(6)));
        let (kind, raw_tmp, audio_meta) = self.acquire_and_decrypt(track, &temp_base, true).await?;
        let stem = crate::cache::file_stem(&Track {
            artist: audio_meta.artist.clone(),
            name: audio_meta.title.clone(),
            ..Default::default()
        });

        let (dest, dest_tmp, encoded) = match (quality, kind) {
            (ManualDownloadQuality::Original, EncodedKind::Flac) => {
                let dest = crate::cache::unique_path(dir, &stem, "flac");
                let dest_tmp = super::TempFile(dest.with_extension("tmp.flac"));
                let res =
                    super::write_tagged_flac(&raw_tmp.0, &dest_tmp.0, normalize, &audio_meta).await;
                (dest, dest_tmp, res)
            }
            (ManualDownloadQuality::Original, EncodedKind::OggVorbis) => {
                let dest = crate::cache::unique_path(dir, &stem, "ogg");
                let dest_tmp = super::TempFile(dest.with_extension("tmp.ogg"));
                let res =
                    super::write_cache_opus(&raw_tmp.0, &dest_tmp.0, None, normalize, &audio_meta)
                        .await;
                (dest, dest_tmp, res)
            }
            (ManualDownloadQuality::Original, EncodedKind::Other) => {
                let dest = crate::cache::unique_path(dir, &stem, "ogg");
                let dest_tmp = super::TempFile(dest.with_extension("tmp.ogg"));
                let res = super::write_cache_opus(
                    &raw_tmp.0,
                    &dest_tmp.0,
                    Some("192k"),
                    normalize,
                    &audio_meta,
                )
                .await;
                (dest, dest_tmp, res)
            }
            (ManualDownloadQuality::Opus(br), _) => {
                let dest = crate::cache::unique_path(dir, &stem, "ogg");
                let dest_tmp = super::TempFile(dest.with_extension("tmp.ogg"));
                let res = super::write_cache_opus(
                    &raw_tmp.0,
                    &dest_tmp.0,
                    Some(br),
                    normalize,
                    &audio_meta,
                )
                .await;
                (dest, dest_tmp, res)
            }
        };
        drop(raw_tmp);
        encoded.map_err(Other)?;
        tokio::fs::rename(&dest_tmp.0, &dest)
            .await
            .map_err(|e| Other(e.to_string()))?;
        Ok(dest)
    }

    async fn wait_for_key_slot(&self) {
        let mut last = self.last_key_request.lock().await;
        if let Some(prev) = *last {
            let elapsed = prev.elapsed();
            if elapsed >= KEY_BUCKET_REFILL_IDLE {
                *self.key_pace.lock() = KEY_PACE_BURST;
            } else {
                let pace = *self.key_pace.lock();
                if elapsed < pace {
                    tokio::time::sleep(pace - elapsed).await;
                }
            }
        }
        *last = Some(Instant::now());
    }

    async fn acquire_and_decrypt(
        &self,
        track: &Track,
        temp_base: &std::path::Path,
        fetch_cover: bool,
    ) -> Result<(EncodedKind, super::TempFile, AudioMetadata), LibrespotError> {
        let mut attempt = 0usize;
        let (session, meta, kind, key, audio) = loop {
            let session = self.ensure_session().await?;
            let (meta, candidates) = resolve_playable(&session, &track.id).await?;

            let mut last_err = Other("all candidate audio streams rejected".into());
            let mut acquired = None;
            let skipped = self.refused.lock().clone();
            let known_good = self.granted.lock().clone();
            let mut refused_here = Vec::new();
            let mut key_throttled = false;

            for (spotify_id, file_id, format, kind) in candidates {
                if skipped.contains(&format) {
                    continue;
                }
                self.wait_for_key_slot().await;
                let key = match session.audio_key().request(spotify_id, file_id).await {
                    Ok(key) => key,
                    Err(e) => {
                        debug!("librespot audio key rejected for {format:?} ({file_id:?}): {e}");
                        if e.kind == ErrorKind::Unavailable {
                            if known_good.contains(&format) {
                                // Spotify already granted this format earlier, so a refusal now is
                                // an account audio-key rate-limit, not a plan tier mismatch.
                                key_throttled = true;
                                last_err = Other(e.to_string());
                                break;
                            }
                            refused_here.push(format);
                        }
                        last_err = Other(e.to_string());
                        continue;
                    }
                };
                match AudioFile::open(&session, file_id, 160 * 1024 / 8).await {
                    Ok(audio) => {
                        debug!(
                            "librespot acquired {kind:?} ({file_id:?}) for {}",
                            meta.name
                        );
                        acquired = Some((format, kind, key, audio));
                        break;
                    }
                    Err(e) => {
                        debug!("librespot AudioFile::open failed for {file_id:?}: {e}");
                        last_err = Other(format!("audio open: {e}"));
                    }
                }
            }

            if let Some((format, kind, key, audio)) = acquired {
                // Only with another format granted is a refusal the plan's, not a throttle.
                let mut refused = self.refused.lock();
                for fmt in refused_here {
                    if refused.insert(fmt) {
                        info!(
                            "Spotify refuses this account {fmt:?} audio, so it's skipped until the next session"
                        );
                    }
                }
                self.granted.lock().insert(format);
                break (session, meta, kind, key, audio);
            }

            if (key_throttled || !refused_here.is_empty())
                && let Some(&backoff) = KEY_THROTTLE_BACKOFFS.get(attempt)
            {
                attempt += 1;
                *self.key_pace.lock() = KEY_PACE_STEADY;
                info!(
                    "librespot audio key rate-limited on {}, waiting {}s and pacing subsequent tracks to {}s",
                    track.name,
                    backoff.as_secs(),
                    KEY_PACE_STEADY.as_secs()
                );
                tokio::time::sleep(backoff).await;
                continue;
            }
            return Err(last_err);
        };
        if let Ok(controller) = audio.get_stream_loader_controller() {
            controller.set_stream_mode();
        }

        let raw_tmp = super::TempFile(temp_base.with_extension(match kind {
            EncodedKind::Flac => "raw.flac",
            EncodedKind::OggVorbis => "raw.ogg",
            EncodedKind::Other => "raw.bin",
        }));

        // Spotify's own metadata wins; the connect-state track fills the gaps.
        let artists: Vec<&str> = meta.artists.iter().map(|a| a.name.as_str()).collect();
        let year = meta.album.date.year();
        let mut audio_meta = AudioMetadata {
            title: or_fallback(&meta.name, &track.name),
            artist: or_fallback(&artists.join(", "), &track.artist),
            album: or_fallback(&meta.album.name, &track.album),
            album_artist: or_fallback(
                &meta
                    .album
                    .artists
                    .iter()
                    .map(|a| a.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", "),
                track.album_artist.as_deref().unwrap_or_default(),
            ),
            date: (year > 0)
                .then(|| year.to_string())
                .or_else(|| track.release_year()),
            track_number: (meta.number > 0)
                .then_some(meta.number)
                .or(track.track_number),
            disc_number: (meta.disc_number > 0)
                .then_some(meta.disc_number)
                .or(track.disc_number),
            picture: None,
        };
        if fetch_cover {
            if let Some(cover) = pick_best_cover(&meta.album.covers) {
                if let Ok(id) = cover.to_base16() {
                    audio_meta.picture = crate::audio::original_cover(&id).await;
                }
                if audio_meta.picture.is_none() {
                    let image = session.spclient().get_image(&cover).await;
                    audio_meta.picture = image
                        .inspect_err(|e| debug!("spclient get_image failed: {e}"))
                        .ok()
                        .map(|b| b.to_vec());
                }
            }
            if audio_meta.picture.is_none() {
                audio_meta.picture = crate::audio::fetch_cover(track).await;
            }
        }

        // AudioFile reads block on the network, so decrypt on the blocking pool, straight
        // to disk: the container starts right after Spotify's header in the first page.
        let raw = raw_tmp.0.clone();
        let written = tokio::task::spawn_blocking(move || -> io::Result<()> {
            let mut decrypted = AudioDecrypt::new(Some(key), audio);
            let mut head = Vec::new();
            decrypted.by_ref().take(64 * 1024).read_to_end(&mut head)?;
            let start = match kind {
                EncodedKind::Flac => find_spotify_flac_start(&head).unwrap_or(0),
                EncodedKind::OggVorbis => find_spotify_ogg_start(&head)
                    .ok_or_else(|| io::Error::other("no Vorbis Ogg stream in decrypted audio"))?,
                EncodedKind::Other => 0,
            };
            let mut out = std::fs::File::create(&raw)?;
            out.write_all(&head[start..])?;
            io::copy(&mut decrypted, &mut out)?;
            Ok(())
        })
        .await
        .map_err(|e| e.to_string())
        .and_then(|r| r.map_err(|e| format!("decrypt: {e}")));

        if let Err(e) = written {
            return Err(Other(e));
        }
        Ok((kind, raw_tmp, audio_meta))
    }

    async fn ensure_session(&self) -> Result<Session, LibrespotError> {
        let mut slot = self.session.lock().await;
        if let Some(session) = slot.as_ref().filter(|s| !s.is_invalid()) {
            return Ok(session.clone());
        }
        let token = self
            .spotify
            .access_token()
            .await
            .map_err(|e| Auth(format!("Spotify token error: {e}")))?;
        // librespot-core exits the process if the account is no longer Premium.
        if !self
            .spotify
            .account_is_premium()
            .await
            .map_err(|e| Other(format!("Spotify plan check: {e}")))?
        {
            crate::config::set_premium(Some(false));
            crate::config::set_use_librespot(false);
            return Err(Other("Spotify account is not Premium".into()));
        }
        let session = Session::new(SessionConfig::default(), None);
        session
            .connect(Credentials::with_access_token(token), false)
            .await
            .map_err(|e| Auth(format!("librespot connect: {e}")))?;
        info!("librespot session connected via OAuth token");
        // a new session may come with a better plan, or another account
        self.refused.lock().clear();
        self.granted.lock().clear();
        *slot = Some(session.clone());
        Ok(session)
    }
}

fn or_fallback(primary: &str, fallback: &str) -> String {
    String::from(if primary.is_empty() {
        fallback
    } else {
        primary
    })
}

type Candidate = (SpotifyId, FileId, AudioFileFormat, EncodedKind);

async fn resolve_playable(
    session: &Session,
    track_id: &str,
) -> Result<(LsTrack, Vec<Candidate>), LibrespotError> {
    let uri = SpotifyUri::from_uri(&format!("spotify:track:{track_id}"))
        .map_err(|e| Other(format!("bad track uri: {e}")))?;
    let meta = LsTrack::get(session, &uri).await.map_err(map_meta_err)?;
    debug!(
        "librespot metadata: {} ({} files, {} alternatives, explicit={})",
        meta.name,
        meta.files.len(),
        meta.alternatives.len(),
        meta.is_explicit
    );
    let SpotifyUri::Track { id: primary_id } = uri else {
        return Err(Other("not a track uri".into()));
    };

    let mut out: Vec<Candidate> = candidate_files(&meta)
        .map(|(format, file, kind)| (primary_id, file, format, kind))
        .collect();
    // Linked-from / alternate takes often carry the real file list when the
    // primary metadata entry is empty (common for remixes / region variants).
    for alt in meta.alternatives.iter() {
        if let SpotifyUri::Track { id } = alt
            && let Ok(alt_meta) = LsTrack::get(session, alt).await
        {
            debug!(
                "librespot alternative {id:?}: {} files",
                alt_meta.files.len()
            );
            out.extend(
                candidate_files(&alt_meta).map(|(format, file, kind)| (*id, file, format, kind)),
            );
        }
    }
    if out.is_empty() {
        return Err(Other(
            "no playable audio files on track or alternatives".into(),
        ));
    }
    Ok((meta, out))
}

/// High-quality formats only, best first: FLAC (Lossless / Platinum), 320 kbps (Standard
/// Premium), or 160 kbps (Premium Lite). Low-bitrate mobile data-saver streams (96k / 24k)
/// are never used.
fn candidate_files(
    meta: &LsTrack,
) -> impl Iterator<Item = (AudioFileFormat, FileId, EncodedKind)> + '_ {
    use AudioFileFormat as F;
    const PRIORITIES: &[(AudioFileFormat, EncodedKind)] = &[
        (F::FLAC_FLAC_24BIT, EncodedKind::Flac),
        (F::FLAC_FLAC, EncodedKind::Flac),
        (F::OGG_VORBIS_320, EncodedKind::OggVorbis),
        (F::AAC_320, EncodedKind::Other),
        (F::MP3_320, EncodedKind::Other),
        (F::MP3_256, EncodedKind::Other),
        (F::OGG_VORBIS_160, EncodedKind::OggVorbis),
        (F::AAC_160, EncodedKind::Other),
        (F::MP3_160, EncodedKind::Other),
    ];
    PRIORITIES
        .iter()
        .filter_map(|(fmt, kind)| Some((*fmt, *meta.files.get(fmt)?, *kind)))
}

fn pick_best_cover(covers: &librespot_metadata::image::Images) -> Option<FileId> {
    [
        ImageSize::XLARGE,
        ImageSize::LARGE,
        ImageSize::DEFAULT,
        ImageSize::SMALL,
    ]
    .iter()
    .find_map(|size| covers.iter().find(|i| i.size == *size))
    .or_else(|| covers.first())
    .map(|i| i.id)
}

fn map_meta_err(e: librespot_core::Error) -> LibrespotError {
    let msg = e.to_string();
    let lower = msg.to_lowercase();
    if ["auth", "login", "permission", "unauthorized"]
        .iter()
        .any(|w| lower.contains(w))
    {
        Auth(msg)
    } else {
        Other(msg)
    }
}

fn find_spotify_ogg_start(data: &[u8]) -> Option<usize> {
    if data.get(SPOTIFY_HEADER..SPOTIFY_HEADER + 4) == Some(b"OggS") {
        return Some(SPOTIFY_HEADER);
    }
    let vorbis_id = data.windows(7).position(|w| w == b"\x01vorbis")?;
    data[..vorbis_id].windows(4).rposition(|w| w == b"OggS")
}

fn find_spotify_flac_start(data: &[u8]) -> Option<usize> {
    let magic = |w: &[u8]| w == b"fLaC" || w == b"OggS";
    if data
        .get(SPOTIFY_HEADER..SPOTIFY_HEADER + 4)
        .is_some_and(magic)
    {
        return Some(SPOTIFY_HEADER);
    }
    data.windows(4).position(magic)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_start_skips_spotify_header() {
        let mut buf = vec![0u8; SPOTIFY_HEADER];
        buf.extend_from_slice(b"fLaC\x00\x00\x00\x22");
        assert_eq!(find_spotify_flac_start(&buf), Some(SPOTIFY_HEADER));
        assert_eq!(find_spotify_flac_start(b"fLaC\x00\x00\x00\x22"), Some(0));
        assert_eq!(find_spotify_flac_start(b"some random audio bytes"), None);

        let mut ogg = vec![0u8; 10];
        ogg.extend_from_slice(b"OggS....\x01vorbis");
        assert_eq!(find_spotify_ogg_start(&ogg), Some(10));
    }
}
