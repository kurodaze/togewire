use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use tracing::{debug, warn};

use crate::now_unix;
use crate::types::Track;

pub const CACHE_DIR: &str = "data/track_cache";
/// Re-persist a track's access time at most this often (see `Inner::touch`).
const TOUCH_PERSIST_SECS: i64 = 300;

#[derive(Serialize, Deserialize)]
pub struct Entry {
    pub last_accessed_at: i64,
    pub file_path: String,
    pub file_size: i64,
    pub bitrate: String,
    pub normalized: bool,
}

/// Saturates: a wrapped product would read as a tiny limit and evict the whole cache.
fn max_bytes(max_mb: u64) -> i64 {
    i64::try_from(max_mb.max(64).saturating_mul(1 << 20)).unwrap_or(i64::MAX)
}

/// Above High (144k): what low-data streaming swaps for a 112k copy.
fn above_144k(bitrate: &str) -> bool {
    bitrate
        .trim_end_matches('k')
        .parse::<u32>()
        .is_ok_and(|kbps| kbps > 144)
}

struct Inner {
    dir: PathBuf,
    meta_file: PathBuf,
    max_bytes: i64,
    entries: HashMap<String, Entry>,
    order: VecDeque<String>,
    total_size_bytes: i64,
}

pub struct Manager {
    inner: Mutex<Inner>,
}

impl Manager {
    #[must_use]
    pub fn new(max_mb: u64) -> Self {
        let dir = PathBuf::from(CACHE_DIR);
        let _ = fs::create_dir_all(&dir);
        sweep_work_dirs(&dir);
        let meta_file = dir.join("_metadata.json");
        let mut inner = Inner {
            dir,
            meta_file,
            max_bytes: max_bytes(max_mb),
            entries: HashMap::new(),
            order: VecDeque::new(),
            total_size_bytes: 0,
        };
        inner.load();
        Self {
            inner: Mutex::new(inner),
        }
    }

    /// Path of a cached track, dropping the entry if the file vanished underneath us.
    pub fn get_path(&self, track_id: &str) -> Option<String> {
        let mut inner = self.inner.lock();
        let path = inner.entries.get(track_id)?.file_path.clone();
        if !Path::new(&path).exists() {
            inner.remove(track_id);
            inner.save();
            return None;
        }
        if inner.touch(track_id) {
            inner.save();
        }
        Some(path)
    }

    pub fn track_path(&self, track: &Track, ext: &str) -> PathBuf {
        let inner = self.inner.lock();
        let base = file_stem(track);
        let mut path = inner.dir.join(format!("{base}.{ext}"));
        let mut idx = 1;
        while path.exists() {
            path = inner.dir.join(format!("{base} ({idx}).{ext}"));
            idx += 1;
        }
        path
    }

    /// (above 144k, already normalized) for a cached file; unknown files assume the
    /// configured bitrate and no ReplayGain tags.
    pub fn stream_flags(&self, file_path: &str) -> (bool, bool) {
        let inner = self.inner.lock();
        match inner.entries.values().find(|e| e.file_path == file_path) {
            Some(e) => (above_144k(&e.bitrate), e.normalized),
            None => (above_144k(&crate::config::audio_bitrate()), false),
        }
    }

    pub fn put(&self, track_id: &str, file_path: &str, bitrate: &str, normalized: bool) {
        let file_size = fs::metadata(file_path).map_or(0, |m| m.len() as i64);
        let entry = Entry {
            last_accessed_at: now_unix(),
            file_path: file_path.to_string(),
            file_size,
            bitrate: bitrate.to_string(),
            normalized,
        };
        let mut inner = self.inner.lock();
        if let Some(old) = inner.entries.remove(track_id) {
            inner.total_size_bytes = (inner.total_size_bytes - old.file_size).max(0);
            inner.order.retain(|id| id != track_id);
            if old.file_path != file_path {
                let _ = fs::remove_file(&old.file_path);
            }
        }
        inner.total_size_bytes += entry.file_size;
        inner.entries.insert(track_id.to_string(), entry);
        inner.order.push_back(track_id.to_string());
        inner.evict_if_needed();
        inner.save();
    }

    /// (`used_bytes`, `max_bytes`, `entry_count`)
    pub fn stats(&self) -> (i64, i64, usize) {
        let inner = self.inner.lock();
        (inner.total_size_bytes, inner.max_bytes, inner.entries.len())
    }

    pub fn set_max_mb(&self, max_mb: u64) {
        let mut inner = self.inner.lock();
        inner.max_bytes = max_bytes(max_mb);
        inner.evict_if_needed();
        inner.save();
    }

    pub fn clear(&self) {
        let mut inner = self.inner.lock();
        inner.entries.clear();
        inner.order.clear();
        inner.total_size_bytes = 0;
        if let Ok(rd) = fs::read_dir(&inner.dir) {
            for entry in rd.flatten() {
                let path = entry.path();
                if path.file_name().and_then(|n| n.to_str()) == Some("_metadata.json") {
                    continue;
                }
                let _ = if path.is_dir() {
                    fs::remove_dir_all(&path)
                } else {
                    fs::remove_file(&path)
                };
            }
        }
        inner.save();
    }
}

impl Inner {
    fn load(&mut self) {
        let Ok(data) = fs::read_to_string(&self.meta_file) else {
            return;
        };
        let Ok(entries): Result<HashMap<String, Entry>, _> = serde_json::from_str(&data) else {
            warn!("track cache metadata corrupt, starting fresh");
            return;
        };
        let mut order = VecDeque::new();
        let mut total = 0i64;
        let mut cleaned = HashMap::new();
        let mut sorted: Vec<_> = entries.into_iter().collect();
        sorted.sort_by_key(|(_, e)| e.last_accessed_at);
        for (id, entry) in sorted {
            if !Path::new(&entry.file_path).exists() {
                continue;
            }
            total += entry.file_size;
            order.push_back(id.clone());
            cleaned.insert(id, entry);
        }
        self.entries = cleaned;
        self.order = order;
        self.total_size_bytes = total;
        debug!(
            "track cache loaded: {} entries, {:.1} MB",
            self.entries.len(),
            self.total_size_bytes as f64 / (1024.0 * 1024.0)
        );
    }

    fn save(&self) {
        match serde_json::to_string(&self.entries) {
            Ok(data) => {
                let tmp = self.meta_file.with_extension("json.tmp");
                if fs::write(&tmp, data).is_ok() {
                    let _ = fs::rename(tmp, &self.meta_file);
                }
            }
            Err(e) => warn!("failed to serialize track cache: {e}"),
        }
    }

    /// Recency bump; true when worth persisting. Gated per track, since the playing track
    /// and the queue head are both looked up every poll.
    fn touch(&mut self, track_id: &str) -> bool {
        let now = now_unix();
        let already_newest = self.order.back().is_some_and(|id| id == track_id);
        let Some(entry) = self.entries.get_mut(track_id) else {
            return false;
        };
        if now - entry.last_accessed_at < TOUCH_PERSIST_SECS {
            return false;
        }
        entry.last_accessed_at = now;
        if !already_newest {
            self.order.retain(|id| id != track_id);
            self.order.push_back(track_id.to_string());
        }
        true
    }

    fn remove_entry(&mut self, track_id: &str) {
        if let Some(old) = self.entries.remove(track_id) {
            self.total_size_bytes = (self.total_size_bytes - old.file_size).max(0);
            let _ = fs::remove_file(&old.file_path);
        }
    }

    fn remove(&mut self, track_id: &str) {
        self.remove_entry(track_id);
        self.order.retain(|id| id != track_id);
    }

    fn evict_if_needed(&mut self) {
        while self.total_size_bytes > self.max_bytes {
            let Some(oldest) = self.order.pop_front() else {
                break;
            };
            debug!("evicting cached track {oldest}");
            self.remove_entry(&oldest);
        }
    }
}

/// yt-dlp scratch dirs and unfinished encodes survive a crash, and eviction only knows
/// about files it has an entry for. Startup is the one moment none of them can be in use.
fn sweep_work_dirs(dir: &Path) {
    let Ok(rd) = fs::read_dir(dir) else { return };
    for entry in rd.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if path.extension().is_some_and(|ext| ext == "ytwork") {
            let _ = fs::remove_dir_all(&path);
        } else if name.ends_with(".tmp")
            || name.ends_with(".tmp.ogg")
            || name.ends_with(".raw.flac")
            || name.ends_with(".raw.ogg")
            || name.ends_with(".raw.bin")
        {
            let _ = fs::remove_file(&path);
        }
    }
}

/// "<artist> - <title>", browsable and sanitized for filesystem safety.
fn file_stem(track: &Track) -> String {
    let mut cleaned = String::new();
    for c in track
        .artist
        .chars()
        .chain(" - ".chars())
        .chain(track.name.chars())
    {
        let ch = if c.is_control() || c.is_whitespace() || c == '/' || c == '\\' {
            ' '
        } else {
            c
        };
        if ch == ' ' {
            if !cleaned.ends_with(' ') && !cleaned.is_empty() {
                cleaned.push(' ');
            }
        } else {
            cleaned.push(ch);
        }
    }
    let cleaned = cleaned.trim_matches(['.', ' ']);

    let mut stem = String::new();
    for c in cleaned.chars() {
        if stem.len() + c.len_utf8() > 180 {
            break;
        }
        stem.push(c);
    }

    let stem = stem.trim();
    if stem.is_empty() {
        "track".into()
    } else {
        stem.into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track(artist: &str, name: &str) -> Track {
        Track {
            name: name.into(),
            artist: artist.into(),
            ..Default::default()
        }
    }

    fn inner_with(ids: &[&str]) -> Inner {
        let mut inner = Inner {
            dir: PathBuf::from("."),
            meta_file: PathBuf::from("./_metadata.json"),
            max_bytes: i64::MAX,
            entries: HashMap::new(),
            order: VecDeque::new(),
            total_size_bytes: 0,
        };
        for id in ids {
            inner.entries.insert(
                (*id).to_string(),
                Entry {
                    last_accessed_at: now_unix(),
                    file_path: format!("/tmp/{id}.ogg"),
                    file_size: 1,
                    bitrate: "144k".into(),
                    normalized: false,
                },
            );
            inner.order.push_back((*id).to_string());
        }
        inner
    }

    #[test]
    fn touch_reorders_but_does_not_resave_hot_tracks() {
        let mut inner = inner_with(&["old", "new"]);

        // Recently touched: no persist even when not newest, or two hot tracks ping-pong.
        assert!(!inner.touch("new"));
        assert!(!inner.touch("old"));
        assert_eq!(inner.order.back().map(String::as_str), Some("new"));

        // Stale recency: promote and persist so LRU survives a restart.
        inner.entries.get_mut("old").unwrap().last_accessed_at =
            now_unix() - TOUCH_PERSIST_SECS - 1;
        assert!(inner.touch("old"));
        assert_eq!(inner.order.back().map(String::as_str), Some("old"));

        assert!(!inner.touch("missing"));
    }

    #[test]
    fn huge_cache_limits_saturate_instead_of_wrapping() {
        assert_eq!(max_bytes(0), 64 << 20);
        assert_eq!(max_bytes(u64::MAX), i64::MAX);
        assert_eq!(max_bytes(1 << 53), i64::MAX);
    }

    #[test]
    fn sweep_removes_scratch_dirs_and_temp_files() {
        let dir = std::env::temp_dir().join(format!("togewire-sweep-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("Artist - Title abcd1234.ytwork")).unwrap();
        fs::write(dir.join("Artist - Title abcd1234.tmp.ogg"), b"part").unwrap();
        fs::write(dir.join("Artist - Title abcd1234.raw.flac"), b"raw").unwrap();
        fs::write(dir.join("_metadata.json.tmp"), b"{}").unwrap();
        fs::write(dir.join("Artist - Title abcd1234.ogg"), b"OggS").unwrap();
        fs::write(dir.join("_metadata.json"), b"{}").unwrap();

        sweep_work_dirs(&dir);

        assert!(!dir.join("Artist - Title abcd1234.ytwork").exists());
        assert!(!dir.join("Artist - Title abcd1234.tmp.ogg").exists());
        assert!(!dir.join("Artist - Title abcd1234.raw.flac").exists());
        assert!(!dir.join("_metadata.json.tmp").exists());
        assert!(dir.join("Artist - Title abcd1234.ogg").exists());
        assert!(dir.join("_metadata.json").exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn file_stem_is_readable_and_stays_inside_the_cache_directory() {
        assert_eq!(file_stem(&track("Basshunter", "DotA")), "Basshunter - DotA");
        // A title is arbitrary text; nothing in it may escape the directory or hide the file.
        for (artist, name) in [
            ("../../etc", "passwd"),
            (".........", "Satellite"),
            ("a\nb", "c/d"),
        ] {
            let stem = file_stem(&track(artist, name));
            assert!(!stem.contains('/'), "{stem}");
            assert!(!stem.starts_with('.'), "{stem}");
            assert!(!stem.contains('\n'), "{stem}");
        }
        assert!(file_stem(&track(&"ながい".repeat(200), "x")).len() <= 200);
    }
}
