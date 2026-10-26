use axum::body::Bytes;
use parking_lot::Mutex;
use std::collections::{HashSet, VecDeque};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, warn};

use crate::config;

const MAX_BURST_BUFFER: usize = 128 * 1024;
const OGG_PAGE_HEADER_LEN: usize = 27;
const OPUS_RATE: u64 = 48_000;
/// File playback runs this far ahead of real time so listeners keep a buffer.
const BUFFER_AHEAD: Duration = Duration::from_secs(5);
/// In-memory streaming copies kept in /dev/shm (~4 MB each).
const MAX_TEMP_COPIES: usize = 3;
/// Pages a listener may fall behind by before it skips ahead, and all one that stops reading
/// can hold on to: about 40 s of togedemon's 20 ms pages, or a minute of a cached file's 1 s
/// ones rather than the whole track.
const LIVE_BACKLOG: usize = 2048;
const FILE_BACKLOG: usize = 64;

pub const CONTENT_TYPE: &str = "audio/ogg";

/// One playback generation. New listeners get the header pages plus a short burst of recent
/// pages, then the broadcast; replacing the pipe drops its sender and ends every body on it.
#[derive(Clone)]
pub struct Pipe {
    tx: broadcast::Sender<Bytes>,
    shutdown: CancellationToken,
    prefix: Arc<Mutex<Prefix>>,
}

#[derive(Default)]
struct Prefix {
    headers: Option<Bytes>,
    burst: VecDeque<Bytes>,
    burst_len: usize,
}

impl Pipe {
    fn new(backlog: usize) -> Self {
        Self {
            tx: broadcast::channel(backlog).0,
            shutdown: CancellationToken::new(),
            prefix: Arc::default(),
        }
    }

    // Producers store+send under `prefix` and `subscribe` snapshots under it, so each page
    // reaches a listener exactly once: in its prefix or off the broadcast.
    fn set_headers(&self, headers: Bytes) {
        let mut p = self.prefix.lock();
        p.headers = Some(headers.clone());
        let _ = self.tx.send(headers);
    }

    fn send(&self, page: Bytes) {
        let mut p = self.prefix.lock();
        p.burst_len += page.len();
        p.burst.push_back(page.clone());
        while p.burst_len > MAX_BURST_BUFFER
            && let Some(old) = p.burst.pop_front()
        {
            p.burst_len -= old.len();
        }
        let _ = self.tx.send(page);
    }

    fn subscribe(&self) -> (broadcast::Receiver<Bytes>, Vec<Bytes>) {
        let p = self.prefix.lock();
        let prefix = p.headers.iter().chain(&p.burst).cloned().collect();
        (self.tx.subscribe(), prefix)
    }
}

struct State {
    pipe: Pipe,
    listeners: usize,
    generation: u64,
    playing_file: bool,
}

struct Inner {
    state: Mutex<State>,
    prewarming: Mutex<HashSet<String>>,
}

#[derive(Clone)]
pub struct StreamManager {
    inner: Arc<Inner>,
    cache: Arc<crate::cache::Manager>,
}

impl StreamManager {
    #[must_use]
    pub fn new(cache: Arc<crate::cache::Manager>) -> Self {
        use std::os::unix::fs::MetadataExt;
        let dir = norm_dir();
        let _ = std::fs::remove_dir_all(&dir);
        create_norm_dir(&dir);
        // /dev/shm is shared: a dir squatted by another user could read or swap our copies.
        if let Ok(meta) = std::fs::symlink_metadata(&dir) {
            assert!(
                meta.is_dir() && meta.uid() == uid() && meta.mode() & 0o077 == 0,
                "{} is not a private directory owned by this user; remove it",
                dir.display()
            );
        }
        Self {
            inner: Arc::new(Inner {
                state: Mutex::new(State {
                    pipe: Pipe::new(LIVE_BACKLOG),
                    listeners: 0,
                    generation: 0,
                    playing_file: false,
                }),
                prewarming: Mutex::default(),
            }),
            cache,
        }
    }

    /// Build the in-memory streaming copy of a cached track in the background.
    pub fn prewarm(&self, file_path: &str) {
        let Some(variant) = self.plan_temp_artifact(file_path) else {
            return;
        };
        if variant.path(file_path).is_file()
            || !self.inner.prewarming.lock().insert(file_path.to_string())
        {
            return;
        }
        let inner = Arc::clone(&self.inner);
        let path = file_path.to_string();
        tokio::spawn(async move {
            ensure_stream_temp(&path, variant).await;
            inner.prewarming.lock().remove(&path);
        });
    }

    /// Which in-memory streaming copy a cached track needs, if any.
    fn plan_temp_artifact(&self, file_path: &str) -> Option<TempArtifact> {
        let (above_144k, normalized) = self.cache.stream_flags(file_path);
        let normalize = config::normalization() == config::Normalization::Stream && !normalized;
        if config::low_data_streaming() && above_144k {
            // One 112k copy covers both features: it is encoded from the normalized copy.
            Some(TempArtifact::LowData {
                normalized: normalize,
            })
        } else {
            normalize.then_some(TempArtifact::Normalized)
        }
    }

    #[must_use]
    pub fn generation(&self) -> u64 {
        self.inner.state.lock().generation
    }

    #[must_use]
    pub fn listener_count(&self) -> usize {
        self.inner.state.lock().listeners
    }

    /// True while a cached file (not togedemon) is feeding the stream.
    #[must_use]
    pub fn has_track(&self) -> bool {
        self.inner.state.lock().playing_file
    }

    pub fn add_client(
        &self,
        max_listeners: usize,
    ) -> Option<(broadcast::Receiver<Bytes>, Vec<Bytes>)> {
        let mut state = self.inner.state.lock();
        if state.listeners >= max_listeners {
            return None;
        }
        state.listeners += 1;
        debug!("Stream: client attached (total: {})", state.listeners);
        Some(state.pipe.subscribe())
    }

    pub fn remove_client(&self) {
        let mut state = self.inner.state.lock();
        state.listeners = state.listeners.saturating_sub(1);
        debug!("Stream: client detached (total: {})", state.listeners);
    }

    /// Retire the current pipe (ending its producer and listener bodies) for a fresh one.
    fn swap_pipe(&self, playing_file: bool) -> Pipe {
        let mut state = self.inner.state.lock();
        state.generation += 1;
        state.pipe.shutdown.cancel();
        state.pipe = Pipe::new(if playing_file {
            FILE_BACKLOG
        } else {
            LIVE_BACKLOG
        });
        state.playing_file = playing_file;
        state.pipe.clone()
    }

    pub fn stop(&self) {
        self.swap_pipe(false);
    }

    pub fn shutdown(&self) {
        self.stop();
        let _ = std::fs::remove_dir_all(norm_dir());
    }

    /// Switch to live togedemon input; feed the returned pipe with `pump_ogg`.
    #[must_use]
    pub fn play_live(&self) -> Pipe {
        self.swap_pipe(false)
    }

    /// Play a cached Ogg file from `seek_ms`.
    pub fn play_track(&self, file_path: &str, seek_ms: i64) {
        let pipe = self.swap_pipe(true);
        let artifact = self.plan_temp_artifact(file_path);
        let inner = Arc::clone(&self.inner);
        let path = file_path.to_string();
        tokio::spawn(async move {
            let source = match artifact {
                Some(variant) => prepare_stream_temp(&inner, &path, variant)
                    .await
                    .unwrap_or_else(|| PathBuf::from(&path)),
                None => PathBuf::from(&path),
            };
            let res = match tokio::fs::File::open(&source).await {
                Ok(file) => {
                    let reader = tokio::io::BufReader::with_capacity(64 * 1024, file);
                    pump_ogg(reader, seek_ms, true, &pipe).await
                }
                Err(e) => Err(e),
            };
            if let Err(e) = res {
                error!("Stream: {}: {e}", source.display());
            }
        });
    }
}

struct OggPage {
    data: Bytes,
    granule: u64,
}

async fn read_ogg_page<R: AsyncRead + Unpin>(r: &mut R) -> io::Result<Option<OggPage>> {
    let mut page = vec![0u8; OGG_PAGE_HEADER_LEN];
    match r.read_exact(&mut page).await {
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        res => {
            res?;
        }
    }
    if !page.starts_with(b"OggS") {
        return Err(io::Error::other("missing OggS"));
    }
    let segments = page[26] as usize;
    page.resize(OGG_PAGE_HEADER_LEN + segments, 0);
    r.read_exact(&mut page[OGG_PAGE_HEADER_LEN..]).await?;
    let body: usize = page[OGG_PAGE_HEADER_LEN..]
        .iter()
        .map(|&b| b as usize)
        .sum();
    let start = page.len();
    page.resize(start + body, 0);
    r.read_exact(&mut page[start..]).await?;
    let granule = u64::from_le_bytes(page[6..14].try_into().unwrap_or_default());
    Ok(Some(OggPage {
        data: page.into(),
        granule,
    }))
}

/// A comment-free OpusTags page, standing in for the file's own: cached tracks carry
/// 200-300 KB of cover art there that every join would otherwise download. `seq` is the
/// replaced last tags page's, so audio pages still follow in sequence.
fn empty_tags_page(serial: &[u8], seq: [u8; 4]) -> Vec<u8> {
    const PACKET: &[u8] = b"OpusTags\x08\0\0\0togewire\0\0\0\0";
    let mut page = b"OggS\0\0".to_vec();
    page.extend_from_slice(&0u64.to_le_bytes());
    page.extend_from_slice(serial);
    page.extend_from_slice(&seq);
    page.extend_from_slice(&[0, 0, 0, 0, 1, PACKET.len() as u8]);
    page.extend_from_slice(PACKET);
    let crc = ogg_crc(&page);
    page[22..26].copy_from_slice(&crc.to_le_bytes());
    page
}

/// Ogg's CRC-32: polynomial 0x04c11db7, unreflected, zero init, no final xor.
fn ogg_crc(data: &[u8]) -> u32 {
    data.iter().fold(0, |mut crc, &b| {
        crc ^= u32::from(b) << 24;
        for _ in 0..8 {
            crc = (crc << 1)
                ^ if crc & 0x8000_0000 != 0 {
                    0x04c1_1db7
                } else {
                    0
                };
        }
        crc
    })
}

/// Feed Ogg pages from `r` into `pipe` until EOF or the pipe is retired: header pages
/// first (tags emptied), then from `seek_ms` on. Files are `pace`d to real time; live input already is.
pub(crate) async fn pump_ogg<R: AsyncRead + Unpin>(
    mut r: R,
    seek_ms: i64,
    pace: bool,
    pipe: &Pipe,
) -> io::Result<()> {
    let (mut head, mut tags_seq) = (None, 1u32.to_le_bytes());
    let mut page = loop {
        let page = read_ogg_page(&mut r)
            .await?
            .ok_or_else(|| io::Error::other("EOF in Ogg headers"))?;
        // Header pages carry granule 0 (or -1 while a packet continues).
        if page.granule != 0 && page.granule != u64::MAX {
            break page;
        }
        match head {
            None => head = Some(page.data),
            Some(_) => tags_seq.copy_from_slice(&page.data[18..22]),
        }
    };
    let head = head.ok_or_else(|| io::Error::other("no Ogg header pages"))?;
    let headers = [&head[..], &empty_tags_page(&head[14..18], tags_seq)].concat();
    // Everything streamed is Opus, whose granule clock is always 48 kHz.
    let base = seek_ms.max(0) as u64 * OPUS_RATE / 1000;
    while page.granule < base {
        let Some(next) = read_ogg_page(&mut r).await? else {
            return Ok(());
        };
        page = next;
    }
    debug!("Stream: ogg seek={seek_ms}ms");
    pipe.set_headers(headers.into());

    let started = Instant::now();
    loop {
        if pace && page.granule > base {
            let due = Duration::from_secs_f64((page.granule - base) as f64 / OPUS_RATE as f64)
                .saturating_sub(BUFFER_AHEAD);
            if let Some(wait) = due.checked_sub(started.elapsed()) {
                tokio::select! {
                    () = pipe.shutdown.cancelled() => return Ok(()),
                    () = tokio::time::sleep(wait) => {}
                }
            }
        }
        if pipe.shutdown.is_cancelled() {
            return Ok(());
        }
        pipe.send(page.data);
        page = tokio::select! {
            () = pipe.shutdown.cancelled() => return Ok(()),
            next = read_ogg_page(&mut r) => match next? {
                Some(next) => next,
                None => return Ok(()),
            },
        };
    }
}

fn create_norm_dir(dir: &Path) {
    use std::os::unix::fs::DirBuilderExt;
    let _ = std::fs::DirBuilder::new().mode(0o700).create(dir);
}

fn uid() -> u32 {
    // SAFETY: getuid has no preconditions and cannot fail.
    unsafe {
        unsafe extern "C" {
            fn getuid() -> u32;
        }
        getuid()
    }
}

/// Per-uid dir in /dev/shm (else the temp dir), so users sharing a host never collide.
fn norm_dir() -> PathBuf {
    let base = Path::new("/dev/shm");
    let base = if base.is_dir() {
        base.to_path_buf()
    } else {
        std::env::temp_dir()
    };
    base.join(format!("togewire_norm_{}", uid()))
}

/// The in-memory streaming copy variants kept in the norm dir.
#[derive(Clone, Copy)]
enum TempArtifact {
    /// Full-quality copy with ReplayGain applied (stream-only normalization).
    Normalized,
    /// 112k Opus transcode for low data mode, encoded from the normalized copy when
    /// `normalized`, so a single artifact serves both features.
    LowData { normalized: bool },
}

impl TempArtifact {
    fn path(self, file_path: &str) -> PathBuf {
        let prefix = match self {
            Self::Normalized => "",
            Self::LowData { normalized: false } => "ld_",
            Self::LowData { normalized: true } => "ldn_",
        };
        let name = Path::new(file_path)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("track.ogg");
        norm_dir().join(format!("{prefix}{name}"))
    }
}

/// Keep only the newest few copies; an open stream keeps reading a pruned file.
fn prune_norm_cache(dir: &Path) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    let mut files: Vec<_> = rd
        .flatten()
        .filter(|e| !e.file_name().to_string_lossy().contains(".tmp."))
        .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
        .collect();
    files.sort();
    for (_, path) in files.iter().rev().skip(MAX_TEMP_COPIES) {
        let _ = std::fs::remove_file(path);
    }
}

/// Build (or reuse) a streaming copy in /dev/shm, so every replay and seek streams plain Ogg
/// pages. The low-data copy bakes the normalized copy's Opus header gain into its 112k
/// encode, since ffmpeg applies it while decoding.
async fn ensure_stream_temp(file_path: &str, variant: TempArtifact) -> Option<PathBuf> {
    let target = variant.path(file_path);
    if target.is_file() {
        return Some(target);
    }
    let dir = norm_dir();
    create_norm_dir(&dir);
    let tmp = dir.join(format!(
        "{}.tmp.{}",
        crate::random_hex(6),
        target.file_name()?.to_string_lossy()
    ));
    let started = Instant::now();
    let built = match variant {
        TempArtifact::Normalized => match tokio::fs::copy(file_path, &tmp).await {
            Ok(_) => crate::audio::apply_rsgain(&tmp).await,
            Err(e) => Err(format!("copy: {e}")),
        },
        TempArtifact::LowData { normalized } => {
            let normalized_copy = if normalized {
                Box::pin(ensure_stream_temp(file_path, TempArtifact::Normalized)).await
            } else {
                None
            };
            let source = normalized_copy.as_deref().unwrap_or(Path::new(file_path));
            let mut cmd = Command::new("ffmpeg");
            cmd.args(["-y", "-hide_banner", "-loglevel", "error", "-i"])
                .arg(source)
                .args(["-vn", "-c:a", "libopus", "-b:a", "112k", "-vbr", "on"])
                .args(["-application", "audio", "-f", "ogg"])
                .arg(&tmp);
            let res = crate::audio::run(&mut cmd, "ffmpeg").await;
            // Only this encode reads the full-size normalized copy; free its RAM slot.
            if let Some(copy) = normalized_copy {
                let _ = std::fs::remove_file(copy);
            }
            res
        }
    }
    .and_then(|()| std::fs::rename(&tmp, &target).map_err(|e| format!("rename: {e}")));
    if let Err(e) = built {
        warn!("Stream copy {} failed: {e}", target.display());
        let _ = std::fs::remove_file(&tmp);
        return None;
    }
    debug!(
        "Stream copy ready: {} in {}ms",
        target.display(),
        started.elapsed().as_millis()
    );
    prune_norm_cache(&dir);
    Some(target)
}

async fn prepare_stream_temp(
    inner: &Inner,
    file_path: &str,
    variant: TempArtifact,
) -> Option<PathBuf> {
    // 100ms poll (max 6s) for a concurrent prewarm of the same file rather than
    // racing it with a second build.
    for _ in 0..60 {
        if !inner.prewarming.lock().contains(file_path) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    ensure_stream_temp(file_path, variant).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(granule: u64, payload: &[u8]) -> Vec<u8> {
        let mut p = vec![0u8; OGG_PAGE_HEADER_LEN];
        p[0..4].copy_from_slice(b"OggS");
        p[6..14].copy_from_slice(&granule.to_le_bytes());
        p[26] = 1; // one segment
        p.push(payload.len() as u8);
        p.extend_from_slice(payload);
        p
    }

    #[tokio::test]
    async fn pump_sends_headers_then_pages_from_the_seek_point() {
        let mut ogg = page(0, b"OpusHead");
        ogg.extend(page(0, b"OpusTags with cover art"));
        ogg.extend(page(48_000, b"one second"));
        ogg.extend(page(96_000, b"two seconds"));

        let pipe = Pipe::new(FILE_BACKLOG);
        let (mut rx, prefix) = pipe.subscribe();
        assert!(prefix.is_empty());
        pump_ogg(&ogg[..], 1500, false, &pipe).await.unwrap();

        let headers = rx.recv().await.unwrap();
        assert!(headers.windows(8).any(|w| w == b"OpusHead"));
        assert!(headers.windows(8).any(|w| w == b"OpusTags"));
        assert!(
            !headers.windows(5).any(|w| w == b"cover"),
            "tags are emptied"
        );
        let audio = rx.recv().await.unwrap();
        assert!(
            audio.ends_with(b"two seconds"),
            "1s page is before the seek"
        );

        // A late joiner gets the same two pages as its prefix.
        let (_, prefix) = pipe.subscribe();
        assert_eq!(prefix, vec![headers, audio]);

        assert_eq!(ogg_crc(b"123456789"), 0x89a1_897f);
        assert!(
            pump_ogg(&b"junk"[..], 0, false, &Pipe::new(FILE_BACKLOG))
                .await
                .is_err()
        );
    }
}
