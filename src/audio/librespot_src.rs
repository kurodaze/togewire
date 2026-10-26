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
/// Spotify prepends a fixed header of this size before the real audio container.
const SPOTIFY_HEADER: usize = 0xA7;

pub struct LibrespotFetcher {
    spotify: SpotifyClient,
    cache: Arc<CacheManager>,
    session: TokioMutex<Option<Session>>,
    cooldown_until: TokioMutex<Option<Instant>>,
    /// Formats Spotify refused this session's account while granting another (FLAC without
    /// Lossless, 320 kbps on a 160 kbps plan). Each try costs a round trip and an error line.
    refused: Mutex<HashSet<AudioFileFormat>>,
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
            refused: Mutex::new(HashSet::new()),
        }
    }

    /// Forget the session, e.g. once the Spotify account is disconnected.
    pub async fn reset(&self) {
        *self.session.lock().await = None;
        *self.cooldown_until.lock().await = None;
    }

    pub async fn fetch_track(&self, track: &Track) -> Result<PathBuf, LibrespotError> {
        if self
            .cooldown_until
            .lock()
            .await
            .is_some_and(|until| Instant::now() < until)
        {
            return Err(Auth("librespot cooling down after auth failures".into()));
        }
        let res = self.fetch_track_inner(track).await;
        if let Err(Auth(_)) = res {
            *self.cooldown_until.lock().await = Some(Instant::now() + AUTH_COOLDOWN);
            *self.session.lock().await = None;
        }
        res
    }

    async fn fetch_track_inner(&self, track: &Track) -> Result<PathBuf, LibrespotError> {
        let session = self.ensure_session().await?;
        let (meta, candidates) = resolve_playable(&session, &track.id).await?;

        let mut last_err = Other("all candidate audio streams rejected".into());
        let mut acquired = None;
        let skipped = self.refused.lock().clone();
        let mut refused_here = Vec::new();
        for (spotify_id, file_id, format, kind) in candidates {
            if skipped.contains(&format) {
                continue;
            }
            let key = match session.audio_key().request(spotify_id, file_id).await {
                Ok(key) => key,
                Err(e) => {
                    debug!("librespot audio key rejected for {format:?} ({file_id:?}): {e}");
                    // a refusal, not a timeout or a dropped connection
                    if e.kind == ErrorKind::Unavailable {
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
                    acquired = Some((kind, key, audio));
                    break;
                }
                Err(e) => {
                    debug!("librespot AudioFile::open failed for {file_id:?}: {e}");
                    last_err = Other(format!("audio open: {e}"));
                }
            }
        }
        let (kind, key, audio) = acquired.ok_or(last_err)?;
        // only now, with another format granted, a refusal is the plan's, not the track's
        {
            let mut refused = self.refused.lock();
            for format in refused_here {
                if refused.insert(format) {
                    info!(
                        "Spotify refuses this account {format:?} audio, so it's skipped until the next session"
                    );
                }
            }
        }
        if let Ok(controller) = audio.get_stream_loader_controller() {
            controller.set_stream_mode();
        }

        let dest = self.cache.track_path(track, "ogg");
        // Must end in a real media extension - ffmpeg ignores compound ".ogg.tmp".
        let dest_tmp = dest.with_extension("tmp.ogg");
        let raw_tmp = dest.with_extension(match kind {
            EncodedKind::Flac => "raw.flac",
            EncodedKind::OggVorbis => "raw.ogg",
            EncodedKind::Other => "raw.bin",
        });

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

        // AudioFile reads block on the network, so decrypt on the blocking pool, straight
        // to disk: the container starts right after Spotify's header in the first page.
        let raw = raw_tmp.clone();
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

        let bitrate = crate::config::audio_bitrate();
        let encoded = match written {
            Ok(()) => {
                super::write_cache_opus(&raw_tmp, &dest_tmp, Some(&bitrate), &audio_meta).await
            }
            Err(e) => Err(e),
        };
        let _ = tokio::fs::remove_file(&raw_tmp).await;
        let normalized = encoded.map_err(Other)?;
        tokio::fs::rename(&dest_tmp, &dest)
            .await
            .map_err(|e| Other(e.to_string()))?;

        self.cache
            .put(&track.id, &dest.to_string_lossy(), &bitrate, normalized);
        Ok(dest)
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

/// Best format first; formats without a known container parse last as `Other`.
fn candidate_files(
    meta: &LsTrack,
) -> impl Iterator<Item = (AudioFileFormat, FileId, EncodedKind)> + '_ {
    use AudioFileFormat as F;
    const PRIORITIES: &[(AudioFileFormat, EncodedKind)] = &[
        (F::FLAC_FLAC_24BIT, EncodedKind::Flac),
        (F::FLAC_FLAC, EncodedKind::Flac),
        (F::OGG_VORBIS_320, EncodedKind::OggVorbis),
        (F::OGG_VORBIS_160, EncodedKind::OggVorbis),
        (F::OGG_VORBIS_96, EncodedKind::OggVorbis),
        (F::AAC_320, EncodedKind::Other),
        (F::MP3_320, EncodedKind::Other),
        (F::MP3_256, EncodedKind::Other),
        (F::MP3_160, EncodedKind::Other),
        (F::AAC_160, EncodedKind::Other),
        (F::MP4_128, EncodedKind::Other),
        (F::MP3_96, EncodedKind::Other),
        (F::AAC_48, EncodedKind::Other),
        (F::AAC_24, EncodedKind::Other),
    ];
    let ranked = PRIORITIES
        .iter()
        .filter_map(|(fmt, kind)| Some((*fmt, *meta.files.get(fmt)?, *kind)));
    let unranked = meta
        .files
        .iter()
        .filter(|(fmt, _)| !PRIORITIES.iter().any(|(p, _)| p == *fmt))
        .map(|(fmt, id)| (*fmt, *id, EncodedKind::Other));
    ranked.chain(unranked)
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
