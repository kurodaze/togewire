pub mod librespot_src;
pub mod prepare;
pub mod ytdlp;

pub use prepare::{PrepareError, TrackPreparer};

use base64::Engine;
use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};
use tokio::process::Command;
use tracing::{debug, warn};

use crate::types::Track;
use crate::{config, http};

/// A cache encode or remux still running after this is killed.
const ENCODE_TIMEOUT: Duration = Duration::from_mins(2);

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

/// Run a subprocess to completion (killed if the future is dropped); a non-zero exit is
/// an error carrying the tail of its stderr.
pub(crate) async fn run(cmd: &mut Command, what: &str) -> Result<(), String> {
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

/// Write `input` to the cache as Ogg/Opus tagged with Spotify metadata: encoded at
/// `bitrate`, or stream-copied when `None` (already Opus). Returns whether ReplayGain
/// tags were applied.
pub(crate) async fn write_cache_opus(
    input: &Path,
    output: &Path,
    bitrate: Option<&str>,
    meta: &AudioMetadata,
) -> Result<bool, String> {
    let started = Instant::now();
    let mut cmd = Command::new("ffmpeg");
    cmd.args(["-y", "-hide_banner", "-loglevel", "error", "-i"])
        .arg(input);

    // Cover art rides in as METADATA_BLOCK_PICTURE via an ffmetadata side input.
    let pic_file = output.with_extension(format!("meta.{}.tmp", crate::random_hex(6)));
    if let Some(pic) = meta.picture.as_deref().filter(|p| !p.is_empty())
        && tokio::fs::write(&pic_file, flac_picture_ffmetadata(pic))
            .await
            .is_ok()
    {
        cmd.arg("-i").arg(&pic_file).args(["-map_metadata", "1"]);
    }

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

    let res = tokio::time::timeout(ENCODE_TIMEOUT, run(&mut cmd, "ffmpeg"))
        .await
        .unwrap_or_else(|_| Err("ffmpeg timed out".into()));
    let _ = tokio::fs::remove_file(&pic_file).await;
    if let Err(e) = res {
        let _ = tokio::fs::remove_file(output).await;
        return Err(e);
    }
    debug!(
        "cache {} done in {}ms: {}",
        bitrate.unwrap_or("remux"),
        started.elapsed().as_millis(),
        output.display()
    );

    let normalize = crate::config::normalization() == crate::config::Normalization::Cache;
    Ok(normalize
        && apply_rsgain(output)
            .await
            .inspect_err(|e| warn!("{e}"))
            .is_ok())
}

/// True when the file's first audio stream is Opus.
pub(crate) async fn is_opus(path: &Path) -> bool {
    Command::new("ffprobe")
        .args(["-v", "error", "-select_streams", "a:0", "-show_entries"])
        .args([
            "stream=codec_name",
            "-of",
            "default=noprint_wrappers=1:nokey=1",
        ])
        .arg(path)
        .output()
        .await
        .is_ok_and(|out| out.stdout.trim_ascii() == b"opus")
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
    run(&mut cmd, "rsgain").await?;
    debug!(
        "rsgain applied to {} in {}ms",
        path.display(),
        started.elapsed().as_millis()
    );
    Ok(())
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
const LISTED_COVERS: &[&str] = &["ab67616d0000b273", "ab67616d00001e02", "ab67616d00004851"];

/// Album art from the track's Spotify cover URL, else (missing or failing) Spotify's
/// public oEmbed.
pub async fn fetch_cover(track: &Track) -> Option<Vec<u8>> {
    if let Some(url) = &track.cover_url {
        if let Some(image) = original_cover(url).await {
            return Some(image);
        }
        if let Some(image) = download_image(url).await {
            return Some(image);
        }
    }
    let oembed = format!(
        "https://open.spotify.com/oembed?url=spotify:track:{}",
        track.id
    );
    let json: serde_json::Value = http().get(oembed).send().await.ok()?.json().await.ok()?;
    let thumbnail = json.get("thumbnail_url")?.as_str()?;
    if let Some(image) = original_cover(thumbnail).await {
        return Some(image);
    }
    download_image(thumbnail).await
}

/// The cover an image id or URL names at its uploaded size, when the config asks for it.
pub async fn original_cover(id_or_url: &str) -> Option<Vec<u8>> {
    if config::cover_art() != config::CoverArt::Original {
        return None;
    }
    download_image(&original_cover_url(id_or_url)?).await
}

fn original_cover_url(id_or_url: &str) -> Option<String> {
    let id = id_or_url.rsplit(['/', ':']).next()?;
    let hash = LISTED_COVERS.iter().find_map(|p| id.strip_prefix(p))?;
    Some(format!("https://i.scdn.co/image/{ORIGINAL_COVER}{hash}"))
}

async fn download_image(url: &str) -> Option<Vec<u8>> {
    let resp = http().get(url).send().await.ok()?.error_for_status().ok()?;
    resp.bytes().await.ok().map(|b| b.to_vec())
}

#[cfg(test)]
mod tests {
    use super::original_cover_url;

    #[test]
    fn original_cover_swaps_the_size_prefix() {
        let original =
            Some("https://i.scdn.co/image/ab67616d000082c18664a3fea53f0101be379f8c".to_string());
        let listed = "https://i.scdn.co/image/ab67616d0000b2738664a3fea53f0101be379f8c";
        assert_eq!(original_cover_url(listed), original);
        let thumbnail =
            "https://image-cdn-ak.spotifycdn.com/image/ab67616d00001e028664a3fea53f0101be379f8c";
        assert_eq!(original_cover_url(thumbnail), original);
        assert_eq!(
            original_cover_url("ab67616d0000b2738664a3fea53f0101be379f8c"),
            original
        );
        assert_eq!(
            original_cover_url("spotify:image:ab67616d0000b2738664a3fea53f0101be379f8c"),
            original
        );
        assert_eq!(original_cover_url("https://example.com/cover.jpg"), None);
    }
}
