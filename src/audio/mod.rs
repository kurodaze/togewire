pub mod librespot_src;
pub mod manual_download;
pub mod prepare;
pub mod ytdlp;

pub use prepare::{PrepareError, TrackPreparer};

use base64::Engine;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};
use tokio::process::Command;
use tracing::{debug, warn};

use crate::types::Track;
use crate::{config, http};

/// A cache encode or remux still running after this is killed.
pub(crate) const ENCODE_TIMEOUT: Duration = Duration::from_mins(2);
const PROBE_TIMEOUT: Duration = Duration::from_secs(15);
const COVER_TIMEOUT: Duration = Duration::from_secs(30);
/// Hard cap on downloaded cover art to prevent unbounded RAM growth from misconfigured servers.
const MAX_COVER_BYTES: usize = 20 * 1024 * 1024;

/// Removes a temporary file on drop, including when an async operation is cancelled.
pub(crate) struct TempFile(pub(crate) PathBuf);

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Removes a temporary directory on drop, including when an async operation is cancelled.
pub(crate) struct TempDir(pub(crate) PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Spotify metadata embedded into every cached file.
#[derive(Default)]
pub struct AudioMetadata {
    pub title: String,
    pub artist: String,
    pub album: String,
    pub album_artist: String,
    pub date: Option<String>,
    pub track_number: Option<i32>,
    pub disc_number: Option<i32>,
    pub picture: Option<Vec<u8>>,
}

/// Run a subprocess to completion with a timeout (killed on drop/timeout).
pub(crate) async fn run_timeout(
    cmd: &mut Command,
    what: &str,
    limit: Duration,
) -> Result<(), String> {
    tokio::time::timeout(limit, run(cmd, what))
        .await
        .unwrap_or_else(|_| Err(format!("{what} timed out")))
}

/// Run a subprocess to completion (killed if the future is dropped); a non-zero exit is
/// an error carrying the tail of its stderr.
async fn run(cmd: &mut Command, what: &str) -> Result<(), String> {
    let out = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .output()
        .await
        .map_err(|e| format!("{what}: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&out.stderr);
        Err(format!("{what}: {}", crate::tail_chars(&stderr, 300)))
    }
}

fn add_metadata_args(cmd: &mut Command, meta: &AudioMetadata) {
    let number = |n: Option<i32>| n.filter(|&n| n > 0).map(|n| n.to_string());
    let tags = [
        ("title", Some(meta.title.clone())),
        (
            "artist",
            Some(meta.artist.clone()).filter(|a| a != "Unknown Artist"),
        ),
        ("album", Some(meta.album.clone())),
        ("album_artist", Some(meta.album_artist.clone())),
        ("date", meta.date.clone()),
        ("track", number(meta.track_number)),
        ("disc", number(meta.disc_number)),
    ];
    for (key, value) in tags {
        if let Some(value) = value.filter(|v| !v.is_empty()) {
            cmd.arg("-metadata").arg(format!("{key}={value}"));
        }
    }
}

/// Write `input` as Ogg/Opus (or stream-copied Ogg Vorbis/Opus when `bitrate` is `None`)
/// tagged with metadata. Returns whether ReplayGain tags were applied.
pub(crate) async fn write_cache_opus(
    input: &Path,
    output: &Path,
    bitrate: Option<&str>,
    normalize: bool,
    meta: &AudioMetadata,
) -> Result<bool, String> {
    let started = Instant::now();
    let mut cmd = Command::new("ffmpeg");
    cmd.args(["-y", "-hide_banner", "-loglevel", "error", "-i"])
        .arg(input);

    // Cover art rides in as METADATA_BLOCK_PICTURE via an ffmetadata side input.
    // Always strip container and stream metadata from the input so YouTube/source tags
    // cannot leak into or override our Vorbis comments.
    let pic_file = TempFile(output.with_extension(format!("meta.{}.tmp", crate::random_hex(6))));
    if let Some(pic) = meta.picture.as_deref().filter(|p| !p.is_empty())
        && tokio::fs::write(&pic_file.0, flac_picture_ffmetadata(pic))
            .await
            .is_ok()
    {
        cmd.arg("-i").arg(&pic_file.0).args(["-map_metadata", "1"]);
    } else {
        cmd.args(["-map_metadata", "-1"]);
    }
    cmd.args(["-map_metadata:s", "-1"]);

    add_metadata_args(&mut cmd, meta);

    cmd.args(["-map", "0:a", "-vn"]);
    match bitrate {
        Some(br) => cmd.args([
            "-c:a",
            "libopus",
            "-b:a",
            br,
            "-vbr",
            "on",
            "-application",
            "audio",
        ]),
        None => cmd.args(["-c:a", "copy"]),
    };
    cmd.args(["-f", "ogg"]).arg(output);

    let res = run_timeout(&mut cmd, "ffmpeg", ENCODE_TIMEOUT).await;
    drop(pic_file);
    if let Err(e) = res {
        let _ = tokio::fs::remove_file(output).await;
        return Err(e);
    }
    debug!(
        "ogg {} done in {}ms: {}",
        bitrate.unwrap_or("remux"),
        started.elapsed().as_millis(),
        output.display()
    );

    Ok(normalize
        && apply_rsgain(output)
            .await
            .inspect_err(|e| warn!("{e}"))
            .is_ok())
}

/// Losslessly remux `input` FLAC into `output` with metadata and attached front cover art.
pub(crate) async fn write_tagged_flac(
    input: &Path,
    output: &Path,
    normalize: bool,
    meta: &AudioMetadata,
) -> Result<bool, String> {
    let started = Instant::now();
    let mut cmd = Command::new("ffmpeg");
    cmd.args(["-y", "-hide_banner", "-loglevel", "error", "-i"])
        .arg(input);

    let pic_ext = if meta
        .picture
        .as_deref()
        .is_some_and(|p| p.starts_with(b"\x89PNG"))
    {
        "png"
    } else {
        "jpg"
    };
    let pic_file =
        TempFile(output.with_extension(format!("cov.{}.{pic_ext}", crate::random_hex(6))));
    let has_pic = if let Some(pic) = meta.picture.as_deref().filter(|p| !p.is_empty())
        && tokio::fs::write(&pic_file.0, pic).await.is_ok()
    {
        cmd.arg("-i").arg(&pic_file.0);
        true
    } else {
        false
    };

    cmd.args(["-map_metadata", "-1"]);
    add_metadata_args(&mut cmd, meta);

    if has_pic {
        cmd.args([
            "-map",
            "0:a",
            "-map",
            "1:v",
            "-c",
            "copy",
            "-disposition:v:0",
            "attached_pic",
            "-metadata:s:v:0",
            "comment=Cover (front)",
        ]);
    } else {
        cmd.args(["-map", "0:a", "-vn", "-c:a", "copy"]);
    }
    cmd.args(["-f", "flac"]).arg(output);

    let res = run_timeout(&mut cmd, "ffmpeg", ENCODE_TIMEOUT).await;
    drop(pic_file);
    if let Err(e) = res {
        let _ = tokio::fs::remove_file(output).await;
        return Err(e);
    }
    debug!(
        "flac remux done in {}ms: {}",
        started.elapsed().as_millis(),
        output.display()
    );

    Ok(normalize
        && apply_rsgain(output)
            .await
            .inspect_err(|e| warn!("{e}"))
            .is_ok())
}

/// First audio stream codec name from `ffprobe`.
async fn audio_codec(path: &Path) -> Option<Vec<u8>> {
    let out = tokio::time::timeout(
        PROBE_TIMEOUT,
        Command::new("ffprobe")
            .args(["-v", "error", "-select_streams", "a:0", "-show_entries"])
            .args([
                "stream=codec_name",
                "-of",
                "default=noprint_wrappers=1:nokey=1",
            ])
            .arg(path)
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .ok()?
    .ok()?;
    Some(out.stdout.trim_ascii().to_vec())
}

/// True when the file's first audio stream is Opus.
pub(crate) async fn is_opus(path: &Path) -> bool {
    audio_codec(path).await.as_deref() == Some(b"opus")
}

/// True when the file's first audio stream can be stream-copied directly into an `.ogg` container.
pub(crate) async fn is_ogg_copyable(path: &Path) -> bool {
    matches!(
        audio_codec(path).await.as_deref(),
        Some(b"opus" | b"vorbis")
    )
}

/// Normalize to ReplayGain 2.0 (−14 LUFS / −1 dBTP) by writing the track gain into the
/// Opus header output gain, which decoders apply; no re-encode, fully reversible.
pub(crate) async fn apply_rsgain(path: &Path) -> Result<(), String> {
    let started = Instant::now();
    let mut cmd = Command::new("rsgain");
    cmd.args([
        "custom", "-s", "i", "-l", "-14", "-t", "-m", "-1.0", "-c", "a", "-o", "t",
    ])
    .arg(path);
    run_timeout(&mut cmd, "rsgain", ENCODE_TIMEOUT).await?;
    debug!(
        "rsgain applied to {} in {}ms",
        path.display(),
        started.elapsed().as_millis()
    );
    Ok(())
}

/// Extract embedded cover art (`METADATA_BLOCK_PICTURE`) from an Ogg Opus or Ogg Vorbis file.
/// Returns `(mime_type, image_bytes)` for validated JPEG, PNG, or WebP images.
pub fn extract_ogg_cover(path: &Path) -> Option<(&'static str, Vec<u8>)> {
    use std::io::{BufReader, Read};

    const MAX_COMMENT_PACKET: usize = MAX_COVER_BYTES / 3 * 4 + 1024 * 1024;
    let file = std::fs::File::open(path).ok()?;
    let mut reader = BufReader::new(file);

    let mut packet_idx = 0u8;
    let mut comment_packet = Vec::new();
    let mut header = [0u8; 27];
    let mut segments = [0u8; 255];

    while packet_idx < 2 {
        reader.read_exact(&mut header).ok()?;
        if &header[0..4] != b"OggS" {
            return None;
        }
        let page_segments = usize::from(header[26]);
        reader.read_exact(&mut segments[..page_segments]).ok()?;

        for &seg_len in &segments[..page_segments] {
            let len = usize::from(seg_len);
            if packet_idx == 0 {
                if len > 0 {
                    std::io::copy(&mut reader.by_ref().take(len as u64), &mut std::io::sink())
                        .ok()?;
                }
            } else if packet_idx == 1 {
                if comment_packet.len().saturating_add(len) > MAX_COMMENT_PACKET {
                    return None;
                }
                let start = comment_packet.len();
                comment_packet.resize(start + len, 0);
                reader.read_exact(&mut comment_packet[start..]).ok()?;
            } else if len > 0 {
                std::io::copy(&mut reader.by_ref().take(len as u64), &mut std::io::sink()).ok()?;
            }
            if seg_len < 255 {
                packet_idx = packet_idx.saturating_add(1);
                if packet_idx >= 2 {
                    break;
                }
            }
        }
    }

    parse_vorbis_comment_cover(&comment_packet)
}

fn parse_vorbis_comment_cover(packet: &[u8]) -> Option<(&'static str, Vec<u8>)> {
    let mut rest = packet
        .strip_prefix(b"OpusTags")
        .or_else(|| packet.strip_prefix(b"\x03vorbis"))?;

    let read_u32_le = |buf: &mut &[u8]| -> Option<usize> {
        if buf.len() < 4 {
            return None;
        }
        let n = u32::from_le_bytes(buf[..4].try_into().ok()?) as usize;
        *buf = &buf[4..];
        Some(n)
    };

    let vendor_len = read_u32_le(&mut rest)?;
    if rest.len() < vendor_len {
        return None;
    }
    rest = &rest[vendor_len..];

    let comment_count = read_u32_le(&mut rest)?;
    let prefix = b"METADATA_BLOCK_PICTURE=";
    let mut fallback: Option<(&'static str, Vec<u8>)> = None;

    for _ in 0..comment_count {
        let len = read_u32_le(&mut rest)?;
        if rest.len() < len {
            return None;
        }
        let comment = &rest[..len];
        rest = &rest[len..];

        if comment.len() <= prefix.len() || !comment[..prefix.len()].eq_ignore_ascii_case(prefix) {
            continue;
        }
        let b64 = &comment[prefix.len()..];
        let Ok(block) = base64::engine::general_purpose::STANDARD.decode(b64) else {
            continue;
        };
        if let Some((pic_type, mime, image)) = parse_flac_picture_block(&block) {
            if pic_type == 3 {
                return Some((mime, image));
            }
            if fallback.is_none() {
                fallback = Some((mime, image));
            }
        }
    }
    fallback
}

fn parse_flac_picture_block(block: &[u8]) -> Option<(u32, &'static str, Vec<u8>)> {
    let mut rest = block;
    let read_u32_be = |buf: &mut &[u8]| -> Option<u32> {
        if buf.len() < 4 {
            return None;
        }
        let n = u32::from_be_bytes(buf[..4].try_into().ok()?);
        *buf = &buf[4..];
        Some(n)
    };

    let pic_type = read_u32_be(&mut rest)?;
    let mime_len = read_u32_be(&mut rest)? as usize;
    if rest.len() < mime_len {
        return None;
    }
    rest = &rest[mime_len..];

    let desc_len = read_u32_be(&mut rest)? as usize;
    // description + width, height, color depth, indexed colors (16 bytes)
    let skip = desc_len.checked_add(16)?;
    if rest.len() < skip {
        return None;
    }
    rest = &rest[skip..];

    let data_len = read_u32_be(&mut rest)? as usize;
    if data_len == 0 || data_len > MAX_COVER_BYTES || rest.len() < data_len {
        return None;
    }
    let image = &rest[..data_len];
    let mime = sniff_cover_mime(image)?;
    Some((pic_type, mime, image.to_vec()))
}

fn sniff_cover_mime(data: &[u8]) -> Option<&'static str> {
    if data.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if data.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("image/jpeg")
    } else if data.len() >= 12 && &data[..4] == b"RIFF" && &data[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

/// ffmetadata file carrying a Base64 FLAC PICTURE block (front cover).
fn flac_picture_ffmetadata(image: &[u8]) -> String {
    let mime = if image.starts_with(b"\x89PNG") {
        "image/png"
    } else {
        "image/jpeg"
    };
    let (width, height) = image_dimensions(image).unwrap_or((0, 0));
    let mut block = Vec::with_capacity(32 + mime.len() + image.len());
    for n in [3, mime.len() as u32] {
        block.extend_from_slice(&n.to_be_bytes()); // picture type 3 = front cover
    }
    block.extend_from_slice(mime.as_bytes());
    // description length, width, height, colour depth, indexed colours, data length
    for n in [0, width, height, 24, 0, image.len() as u32] {
        block.extend_from_slice(&n.to_be_bytes());
    }
    block.extend_from_slice(image);
    let prefix = ";FFMETADATA1\nMETADATA_BLOCK_PICTURE=";
    let mut out = String::with_capacity(prefix.len() + block.len().div_ceil(3) * 4 + 4);
    out.push_str(prefix);
    base64::engine::general_purpose::STANDARD.encode_string(&block, &mut out);
    let pad = out.bytes().rev().take_while(|&b| b == b'=').count();
    out.truncate(out.len() - pad);
    for _ in 0..pad {
        out.push_str("\\=");
    }
    out.push('\n');
    out
}

fn image_dimensions(data: &[u8]) -> Option<(u32, u32)> {
    if data.starts_with(b"\x89PNG\r\n\x1a\n") && data.len() >= 24 {
        let width = u32::from_be_bytes(data[16..20].try_into().ok()?);
        let height = u32::from_be_bytes(data[20..24].try_into().ok()?);
        return Some((width, height));
    }
    if data.starts_with(&[0xFF, 0xD8]) {
        // Walk JPEG segments to the first SOF0-2 frame header.
        let mut i = 2;
        while i + 8 < data.len() && data[i] == 0xFF {
            while data.get(i + 1) == Some(&0xFF) {
                i += 1;
            }
            if i + 8 >= data.len() {
                break;
            }
            match data[i + 1] {
                0xC0..=0xC2 => {
                    let height = u16::from_be_bytes([data[i + 5], data[i + 6]]) as u32;
                    let width = u16::from_be_bytes([data[i + 7], data[i + 8]]) as u32;
                    return Some((width, height));
                }
                0xDA | 0xD9 => break,
                _ => {
                    let len = u16::from_be_bytes([data[i + 2], data[i + 3]]) as usize;
                    if len < 2 {
                        break;
                    }
                    i += 2 + len;
                }
            }
        }
    }
    None
}

/// Spotify image ids are a size prefix and the image's hash. Its image server also keeps
/// every cover at its uploaded size under this prefix, next to the sizes metadata lists.
const ORIGINAL_COVER: &str = "ab67616d000082c1";
const LARGE_COVER: &str = "ab67616d0000b273";
const SMALL_LISTED_COVERS: &[&str] = &["ab67616d00001e02", "ab67616d00004851"];
const LISTED_COVERS: &[&str] = &[LARGE_COVER, "ab67616d00001e02", "ab67616d00004851"];

/// Album art from the track's Spotify cover URL, else (missing or failing) Spotify's
/// public oEmbed.
pub async fn fetch_cover(track: &Track) -> Option<Vec<u8>> {
    if let Some(url) = &track.cover_url
        && let Some(image) = fetch_spotify_cover_url(url).await
    {
        debug!(
            "cover for {} from Spotify ({})",
            track.id,
            describe_image(&image)
        );
        return Some(image);
    }
    let oembed = format!(
        "https://open.spotify.com/oembed?url=spotify:track:{}",
        track.id
    );
    let json: serde_json::Value = http().get(oembed).send().await.ok()?.json().await.ok()?;
    let thumbnail = json.get("thumbnail_url")?.as_str()?;
    let image = fetch_spotify_cover_url(thumbnail).await;
    match &image {
        Some(img) => debug!(
            "cover for {} from Spotify oEmbed ({})",
            track.id,
            describe_image(img)
        ),
        None => warn!("no cover art found for {}", track.id),
    }
    image
}

async fn fetch_spotify_cover_url(url: &str) -> Option<Vec<u8>> {
    if let Some(image) = original_cover(url).await {
        return Some(image);
    }
    if let Some(large) = large_cover_url(url)
        && let Some(image) = download_image(&large).await
    {
        return Some(image);
    }
    download_image(url).await
}

/// The cover an image id or URL names at its uploaded size (or downscaled to 1280 px),
/// when the config asks for it.
pub async fn original_cover(id_or_url: &str) -> Option<Vec<u8>> {
    let mode = config::cover_art();
    if mode == config::CoverArt::Listed {
        return None;
    }
    let image = download_image(&original_cover_url(id_or_url)?).await?;
    if mode == config::CoverArt::Size1280 {
        return Some(downscale_cover(image, 1280).await);
    }
    Some(image)
}

/// Respect `config::cover_art()` sizing on an already-square cover (e.g. Deezer) without
/// black-bar cropping or unnecessary re-encoding.
pub(crate) async fn prepare_square_cover(image: Vec<u8>) -> Vec<u8> {
    match config::cover_art() {
        config::CoverArt::Listed => downscale_cover(image, 640).await,
        config::CoverArt::Size1280 => downscale_cover(image, 1280).await,
        config::CoverArt::Original => image,
    }
}

async fn downscale_cover(image: Vec<u8>, max_side: u32) -> Vec<u8> {
    if let Some((w, h)) = image_dimensions(&image)
        && w <= max_side
        && h <= max_side
    {
        return image;
    }
    let is_png = image.starts_with(b"\x89PNG");
    let ext = if is_png { "png" } else { "jpg" };
    let id = crate::random_hex(6);
    let tmp_in = TempFile(std::env::temp_dir().join(format!("togewire-cov-{id}-in.{ext}")));
    let tmp_out = TempFile(std::env::temp_dir().join(format!("togewire-cov-{id}-out.{ext}")));
    let vf = format!(
        "scale='min({max_side},iw)':'min({max_side},ih)':force_original_aspect_ratio=decrease"
    );
    let res = async {
        tokio::fs::write(&tmp_in.0, &image).await.ok()?;
        let mut cmd = Command::new("ffmpeg");
        cmd.args(["-y", "-hide_banner", "-loglevel", "error", "-i"])
            .arg(&tmp_in.0)
            .args(["-vf", &vf, "-frames:v", "1"]);
        if !is_png {
            cmd.args(["-q:v", "2"]);
        }
        cmd.arg(&tmp_out.0);
        run_timeout(&mut cmd, "ffmpeg cover scale", COVER_TIMEOUT)
            .await
            .ok()?;
        tokio::fs::read(&tmp_out.0).await.ok()
    }
    .await;
    drop(tmp_in);
    drop(tmp_out);
    res.filter(|b| !b.is_empty()).unwrap_or(image)
}

fn original_cover_url(id_or_url: &str) -> Option<String> {
    let id = id_or_url.rsplit(['/', ':']).next()?;
    let hash = LISTED_COVERS.iter().find_map(|p| id.strip_prefix(p))?;
    Some(format!("https://i.scdn.co/image/{ORIGINAL_COVER}{hash}"))
}

fn large_cover_url(id_or_url: &str) -> Option<String> {
    let id = id_or_url.rsplit(['/', ':']).next()?;
    let hash = SMALL_LISTED_COVERS
        .iter()
        .find_map(|p| id.strip_prefix(p))?;
    Some(format!("https://i.scdn.co/image/{LARGE_COVER}{hash}"))
}

/// Attempts per cover image: CDNs (Apple's in particular) intermittently reset connections.
const COVER_ATTEMPTS: u32 = 3;

/// Downloads an image, retrying connection failures, timeouts and 5xx/429 responses.
pub(crate) async fn download_image(url: &str) -> Option<Vec<u8>> {
    let started = Instant::now();
    let mut last_err = String::new();
    for attempt in 1..=COVER_ATTEMPTS {
        if attempt > 1 {
            tokio::time::sleep(Duration::from_millis(300 * u64::from(attempt - 1))).await;
        }
        let mut resp = match http().get(url).timeout(COVER_TIMEOUT).send().await {
            Ok(resp) => resp,
            Err(e) => {
                last_err = e.to_string();
                debug!("cover fetch attempt {attempt}/{COVER_ATTEMPTS} failed for {url}: {e}");
                continue;
            }
        };
        let status = resp.status();
        if !status.is_success() {
            last_err = format!("HTTP {status}");
            debug!("cover fetch attempt {attempt}/{COVER_ATTEMPTS} for {url}: HTTP {status}");
            if status.is_server_error() || status == reqwest::StatusCode::TOO_MANY_REQUESTS {
                continue;
            }
            // 4xx is an answer, not a hiccup (e.g. a Spotify cover with no original size).
            debug!("cover fetch gave up on {url}: HTTP {status}");
            return None;
        }
        if resp
            .content_length()
            .is_some_and(|n| n > MAX_COVER_BYTES as u64)
        {
            warn!("cover fetch rejected oversized image for {url}");
            return None;
        }
        let mut bytes = Vec::new();
        let mut read_err = None;
        loop {
            match resp.chunk().await {
                Ok(Some(chunk)) => {
                    if bytes.len() + chunk.len() > MAX_COVER_BYTES {
                        warn!("cover fetch exceeded {MAX_COVER_BYTES} bytes for {url}");
                        return None;
                    }
                    bytes.extend_from_slice(&chunk);
                }
                Ok(None) => break,
                Err(e) => {
                    read_err = Some(e);
                    break;
                }
            }
        }
        if let Some(e) = read_err {
            last_err = e.to_string();
            debug!("cover fetch attempt {attempt}/{COVER_ATTEMPTS} body failed for {url}: {e}");
            continue;
        }
        if !bytes.is_empty() {
            let dims = image_dimensions(&bytes)
                .map_or_else(|| "unknown size".into(), |(w, h)| format!("{w}x{h}"));
            debug!(
                "cover fetched {url} ({dims}, {} KB) in {}ms",
                bytes.len() / 1024,
                started.elapsed().as_millis()
            );
            return Some(bytes);
        }
        last_err = "empty body".into();
        debug!("cover fetch attempt {attempt}/{COVER_ATTEMPTS} for {url}: empty body");
    }
    warn!("cover fetch failed for {url}: {last_err}");
    None
}

/// `WxH` of an image for logs.
pub(crate) fn describe_image(image: &[u8]) -> String {
    image_dimensions(image).map_or_else(
        || format!("{} KB", image.len() / 1024),
        |(w, h)| format!("{w}x{h}, {} KB", image.len() / 1024),
    )
}

/// Convert a fallback YouTube video thumbnail (often 16:9 widescreen with black pillarbox or
/// watermark bars, or in WebP format) into a clean 1:1 square JPEG cover.
pub(crate) async fn prepare_youtube_cover(image: Vec<u8>) -> Vec<u8> {
    let id = crate::random_hex(6);
    let tmp_in = TempFile(std::env::temp_dir().join(format!("togewire-ytcov-{id}-in.img")));
    let tmp_out = TempFile(std::env::temp_dir().join(format!("togewire-ytcov-{id}-out.jpg")));
    let res = async {
        tokio::fs::write(&tmp_in.0, &image).await.ok()?;
        let probe = tokio::time::timeout(
            PROBE_TIMEOUT,
            Command::new("ffprobe")
                .args([
                    "-v",
                    "error",
                    "-select_streams",
                    "v:0",
                    "-show_entries",
                    "stream=width,height",
                    "-of",
                    "csv=p=0",
                ])
                .arg(&tmp_in.0)
                .stdin(Stdio::null())
                .kill_on_drop(true)
                .output(),
        )
        .await
        .ok()?
        .ok()?;
        let dims = String::from_utf8_lossy(&probe.stdout);
        let (w_str, h_str) = dims.trim().split_once(',')?;
        let (w, h): (usize, usize) = (w_str.parse().ok()?, h_str.parse().ok()?);
        if w == 0 || h == 0 {
            return None;
        }
        // Already square (e.g. YouTube Music art track): keep framing intact rather than
        // trimming dark edges into the artwork.
        let (side, cx, cy) = if w == h {
            (w, 0, 0)
        } else {
            let gray_out = tokio::time::timeout(
                COVER_TIMEOUT,
                Command::new("ffmpeg")
                    .args(["-v", "error", "-i"])
                    .arg(&tmp_in.0)
                    .args(["-f", "rawvideo", "-pix_fmt", "gray", "-frames:v", "1", "-"])
                    .stdin(Stdio::null())
                    .kill_on_drop(true)
                    .output(),
            )
            .await
            .ok()?
            .ok()?;
            let raw = &gray_out.stdout;
            if w.checked_mul(h).is_some_and(|n| raw.len() >= n) {
                square_crop_box(raw, w, h)
            } else {
                let side = w.min(h);
                (side, (w - side) / 2, (h - side) / 2)
            }
        };
        let max_side = match config::cover_art() {
            config::CoverArt::Listed => 640,
            config::CoverArt::Size1280 => 1280,
            config::CoverArt::Original => side,
        };
        let vf = if side > max_side {
            format!("crop={side}:{side}:{cx}:{cy},scale={max_side}:{max_side}")
        } else {
            format!("crop={side}:{side}:{cx}:{cy}")
        };
        let mut cmd = Command::new("ffmpeg");
        cmd.args(["-y", "-hide_banner", "-loglevel", "error", "-i"])
            .arg(&tmp_in.0)
            .args(["-vf", &vf, "-frames:v", "1", "-q:v", "2"])
            .arg(&tmp_out.0);
        run_timeout(&mut cmd, "ffmpeg youtube cover crop", COVER_TIMEOUT)
            .await
            .ok()?;
        tokio::fs::read(&tmp_out.0).await.ok()
    }
    .await;
    drop(tmp_in);
    drop(tmp_out);
    res.filter(|b| !b.is_empty()).unwrap_or(image)
}

/// Detect symmetric outer black pillarbox/letterbox bars on a grayscale frame `(w, h)` and
/// return `(side, x, y)` for a centered 1:1 square inside the trimmed content area.
fn square_crop_box(raw: &[u8], w: usize, h: usize) -> (usize, usize, usize) {
    let is_black_bar = |mut vals: Vec<u8>| {
        if vals.is_empty() {
            return false;
        }
        vals.sort_unstable();
        let n = vals.len();
        vals[n * 90 / 100] <= 8 && vals[n * 98 / 100] <= 20
    };
    let max_h = w / 3;
    let mut left_bar = 0;
    while left_bar < max_h && is_black_bar((0..h).map(|y| raw[y * w + left_bar]).collect()) {
        left_bar += 1;
    }
    let mut right_bar = 0;
    while right_bar < max_h
        && is_black_bar((0..h).map(|y| raw[y * w + (w - 1 - right_bar)]).collect())
    {
        right_bar += 1;
    }
    // Trim symmetrically so a dark edge on only one side never shifts the crop off-center.
    let mut h_trim = left_bar.min(right_bar);
    if h_trim > 0 && h_trim < max_h {
        h_trim += 1;
    }
    let left = h_trim;
    let right = w.saturating_sub(1 + h_trim);

    let max_v = h / 4;
    let mut top_bar = 0;
    while top_bar < max_v && is_black_bar(raw[top_bar * w + left..=top_bar * w + right].to_vec()) {
        top_bar += 1;
    }
    let mut bottom_bar = 0;
    while bottom_bar < max_v
        && is_black_bar(
            raw[(h - 1 - bottom_bar) * w + left..=(h - 1 - bottom_bar) * w + right].to_vec(),
        )
    {
        bottom_bar += 1;
    }
    let mut v_trim = top_bar.min(bottom_bar);
    if v_trim > 0 && v_trim < max_v {
        v_trim += 1;
    }
    let top = v_trim;
    let bottom = h.saturating_sub(1 + v_trim);

    let bw = right.saturating_sub(left) + 1;
    let bh = bottom.saturating_sub(top) + 1;
    let side = bw.min(bh).max(1);
    let cx = left + (bw - side) / 2;
    let cy = top + (bh - side) / 2;
    (side, cx, cy)
}

#[cfg(test)]
mod tests {
    use super::{
        extract_ogg_cover, flac_picture_ffmetadata, large_cover_url, original_cover_url,
        square_crop_box,
    };

    fn ogg_page(payload: &[u8], segments: &[u8]) -> Vec<u8> {
        let mut page = Vec::with_capacity(27 + segments.len() + payload.len());
        page.extend_from_slice(b"OggS");
        page.extend_from_slice(&[0; 22]);
        page.push(segments.len() as u8);
        page.extend_from_slice(segments);
        page.extend_from_slice(payload);
        page
    }

    #[test]
    fn extract_ogg_cover_reads_multi_page_opus_tags_picture() {
        // Minimal PNG signature + IHDR dimensions + payload large enough to span multiple Ogg pages.
        let mut png = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR\0\0\0\x10\0\0\0\x10".to_vec();
        png.resize(1200, 0xAB);

        let ffmeta = flac_picture_ffmetadata(&png);
        let b64 = ffmeta
            .lines()
            .find_map(|l| l.strip_prefix("METADATA_BLOCK_PICTURE="))
            .unwrap()
            .replace("\\=", "=");
        let comment = format!("METADATA_BLOCK_PICTURE={b64}");

        let mut tags = b"OpusTags".to_vec();
        let vendor = b"togewire-test";
        tags.extend_from_slice(&(vendor.len() as u32).to_le_bytes());
        tags.extend_from_slice(vendor);
        tags.extend_from_slice(&2u32.to_le_bytes());
        let title = b"TITLE=Demo";
        tags.extend_from_slice(&(title.len() as u32).to_le_bytes());
        tags.extend_from_slice(title);
        tags.extend_from_slice(&(comment.len() as u32).to_le_bytes());
        tags.extend_from_slice(comment.as_bytes());

        // Packet 0: OpusHead on page 0.
        let head = b"OpusHead\x01\x02\0\0\x80\xbb\0\0\0\0\0";
        let mut ogg = ogg_page(head, &[head.len() as u8]);
        // Packet 1: OpusTags split across two Ogg pages (255-byte segments + final short segment).
        let split = 510.min(tags.len());
        ogg.extend_from_slice(&ogg_page(&tags[..split], &[255, 255]));
        let rest = &tags[split..];
        let mut segs = vec![255u8; rest.len() / 255];
        segs.push((rest.len() % 255) as u8);
        ogg.extend_from_slice(&ogg_page(rest, &segs));

        let path =
            std::env::temp_dir().join(format!("togewire-oggcover-{}.ogg", std::process::id()));
        std::fs::write(&path, &ogg).unwrap();
        let extracted = extract_ogg_cover(&path);
        let _ = std::fs::remove_file(&path);

        assert_eq!(extracted, Some(("image/png", png)));
    }

    #[test]
    fn original_cover_swaps_the_size_prefix() {
        let original =
            Some("https://i.scdn.co/image/ab67616d000082c18664a3fea53f0101be379f8c".to_string());
        let listed = "https://i.scdn.co/image/ab67616d0000b2738664a3fea53f0101be379f8c";
        assert_eq!(original_cover_url(listed), original);
        let thumbnail =
            "https://image-cdn-ak.spotifycdn.com/image/ab67616d00001e028664a3fea53f0101be379f8c";
        assert_eq!(original_cover_url(thumbnail), original);
        assert_eq!(large_cover_url(thumbnail).as_deref(), Some(listed));
        assert_eq!(large_cover_url(listed), None);
        assert_eq!(
            original_cover_url("ab67616d0000b2738664a3fea53f0101be379f8c"),
            original
        );
        assert_eq!(
            original_cover_url("spotify:image:ab67616d0000b2738664a3fea53f0101be379f8c"),
            original
        );
        assert_eq!(original_cover_url("https://example.com/cover.jpg"), None);
        assert_eq!(large_cover_url("https://example.com/cover.jpg"), None);
    }

    #[test]
    fn square_crop_trims_black_bars_and_centers() {
        let (w, h) = (160, 90);
        let mut raw = vec![200u8; w * h];
        // Symmetric 20px black pillarbox bars on left/right and 10px letterbox bars on top/bottom
        for y in 0..h {
            for x in 0..w {
                if !(20..140).contains(&x) || !(10..80).contains(&y) {
                    raw[y * w + x] = 0;
                }
            }
        }
        let (side, cx, cy) = square_crop_box(&raw, w, h);
        assert!((68..=70).contains(&side));
        assert!(cy >= 10 && cy + side <= 80);
        assert!(cx >= 20 && cx + side <= 140);
        assert_eq!(cx, (w - side) / 2);

        // Dark artwork on only one side (e.g. blurred left + black right) must stay centered.
        let mut one_sided = vec![200u8; w * h];
        for y in 0..h {
            for x in 105..w {
                one_sided[y * w + x] = 0;
            }
        }
        assert_eq!(square_crop_box(&one_sided, w, h), (h, (w - h) / 2, 0));
    }
}
