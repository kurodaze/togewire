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
/// In-memory tracks kept in `/dev/shm` when disk caching is off (current, next, previous).
const MAX_MEM_TRACKS: usize = 3;

/// Duration tolerance when matching a relinked/duplicate Spotify release to an already-cached
/// file with the exact same `<artist> - <title>` stem.
const SAME_TRACK_DURATION_DRIFT_MS: i64 = 2_000;

fn is_zero(n: &i64) -> bool {
    *n == 0
}

#[derive(Serialize, Deserialize)]
pub struct Entry {
    pub last_accessed_at: i64,
    pub file_path: String,
    pub file_size: i64,
    pub bitrate: String,
    pub normalized: bool,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub source: String,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub duration_ms: i64,
}

#[derive(Clone, Serialize)]
pub struct CacheItem {
    pub id: String,
    pub name: String,
    pub file_name: String,
    pub file_size: i64,
    pub bitrate: String,
    pub normalized: bool,
    pub source: String,
    pub last_accessed_at: i64,
}

struct MemEntry {
    file_path: String,
    bitrate: String,
    normalized: bool,
    duration_ms: i64,
}

/// Saturates: a wrapped product would read as a tiny limit and evict the whole cache.
fn max_bytes(max_mb: u64) -> i64 {
    i64::try_from(max_mb.max(64).saturating_mul(1 << 20)).unwrap_or(i64::MAX)
}

/// Above Normal (112k): what low-data streaming swaps for a 112k copy.
fn above_112k(bitrate: &str) -> bool {
    bitrate
        .trim_end_matches('k')
        .parse::<u32>()
        .is_ok_and(|kbps| kbps > 112)
}

struct Inner {
    dir: PathBuf,
    mem_dir: PathBuf,
    meta_file: PathBuf,
    max_bytes: i64,
    entries: HashMap<String, Entry>,
    order: VecDeque<String>,
    total_size_bytes: i64,
    mem_entries: HashMap<String, MemEntry>,
    mem_order: VecDeque<String>,
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
        let mem_dir = crate::shm_dir_path("togewire_mem");
        let _ = fs::remove_dir_all(&mem_dir);
        crate::ensure_shm_dir(&mem_dir);
        let meta_file = dir.join("_metadata.json");
        let mut inner = Inner {
            dir,
            mem_dir,
            meta_file,
            max_bytes: max_bytes(max_mb),
            entries: HashMap::new(),
            order: VecDeque::new(),
            total_size_bytes: 0,
            mem_entries: HashMap::new(),
            mem_order: VecDeque::new(),
        };
        inner.load();
        Self {
            inner: Mutex::new(inner),
        }
    }

    /// Path of a prepared track (disk cache first, else in-memory), checking `track.id`,
    /// any relinked `track.alt_ids`, and finally an already-cached file with the exact same
    /// `<artist> - <title>` stem and matching duration (deduping Spotify regional relinks).
    pub fn get_for_track(&self, track: &Track) -> Option<String> {
        if let Some(path) = self.get_path(&track.id) {
            return Some(path);
        }
        for alt in &track.alt_ids {
            if let Some(path) = self.get_path(alt) {
                return Some(path);
            }
        }
        if track.duration <= 0 || track.name.trim().is_empty() || track.name == track.id {
            return None;
        }
        let want_stem = file_stem(track).to_lowercase();
        let mut inner = self.inner.lock();
        let matched_disk = inner
            .order
            .iter()
            .rev()
            .find(|id| {
                inner.entries.get(*id).is_some_and(|e| {
                    e.duration_ms > 0
                        && (e.duration_ms - track.duration).abs() <= SAME_TRACK_DURATION_DRIFT_MS
                        && stem_matches(&e.file_path, &want_stem)
                })
            })
            .cloned();
        if let Some(id) = matched_disk
            && let Some(path) = inner.entries.get(&id).map(|e| e.file_path.clone())
        {
            if Path::new(&path).exists() {
                if inner.touch(&id) {
                    inner.save();
                }
                return Some(path);
            }
            inner.remove(&id);
            inner.save();
        }
        let matched_mem = inner
            .mem_order
            .iter()
            .rev()
            .find(|id| {
                inner.mem_entries.get(*id).is_some_and(|e| {
                    e.duration_ms > 0
                        && (e.duration_ms - track.duration).abs() <= SAME_TRACK_DURATION_DRIFT_MS
                        && stem_matches(&e.file_path, &want_stem)
                })
            })
            .cloned();
        if let Some(id) = matched_mem
            && let Some(path) = inner.mem_entries.get(&id).map(|e| e.file_path.clone())
        {
            if Path::new(&path).exists() {
                return Some(path);
            }
            inner.remove_mem(&id);
        }
        None
    }

    /// Path of a prepared track (disk cache first, else in-memory), dropping the entry if
    /// the file vanished underneath us.
    pub fn get_path(&self, track_id: &str) -> Option<String> {
        let mut inner = self.inner.lock();
        if let Some(path) = inner.entries.get(track_id).map(|e| e.file_path.clone()) {
            if !Path::new(&path).exists() {
                inner.remove(track_id);
                inner.save();
            } else {
                if inner.touch(track_id) {
                    inner.save();
                }
                return Some(path);
            }
        }
        let path = inner.mem_entries.get(track_id)?.file_path.clone();
        if !Path::new(&path).exists() {
            inner.remove_mem(track_id);
            return None;
        }
        if inner.mem_order.back().is_none_or(|id| id != track_id) {
            inner.mem_order.retain(|id| id != track_id);
            inner.mem_order.push_back(track_id.to_string());
        }
        Some(path)
    }

    pub fn track_path(&self, track: &Track, ext: &str) -> PathBuf {
        let inner = self.inner.lock();
        unique_path(&inner.dir, &file_stem(track), ext)
    }

    /// Path for a track prepared in memory (`/dev/shm`), never touching `data/track_cache/`.
    pub fn mem_track_path(&self, track: &Track, ext: &str) -> PathBuf {
        let inner = self.inner.lock();
        crate::ensure_shm_dir(&inner.mem_dir);
        unique_path(&inner.mem_dir, &file_stem(track), ext)
    }

    /// (above 112k, already normalized) for a cached or in-memory file; unknown files assume
    /// the configured bitrate and no ReplayGain tags.
    pub fn stream_flags(&self, file_path: &str) -> (bool, bool) {
        let inner = self.inner.lock();
        if let Some(e) = inner.entries.values().find(|e| e.file_path == file_path) {
            return (above_112k(&e.bitrate), e.normalized);
        }
        if let Some(e) = inner
            .mem_entries
            .values()
            .find(|e| e.file_path == file_path)
        {
            return (above_112k(&e.bitrate), e.normalized);
        }
        (above_112k(&crate::config::audio_bitrate()), false)
    }

    pub fn put(
        &self,
        track_id: &str,
        file_path: &str,
        bitrate: &str,
        normalized: bool,
        source: &str,
        duration_ms: i64,
    ) {
        let file_size = fs::metadata(file_path).map_or(0, |m| m.len() as i64);
        let entry = Entry {
            last_accessed_at: now_unix(),
            file_path: file_path.to_string(),
            file_size,
            bitrate: bitrate.to_string(),
            normalized,
            source: source.to_string(),
            duration_ms: duration_ms.max(0),
        };
        let mut inner = self.inner.lock();
        inner.remove_mem(track_id);
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

    /// Register a track prepared in memory (`/dev/shm`), keeping at most `MAX_MEM_TRACKS`.
    pub fn put_mem(
        &self,
        track_id: &str,
        file_path: &str,
        bitrate: &str,
        normalized: bool,
        duration_ms: i64,
    ) {
        let entry = MemEntry {
            file_path: file_path.to_string(),
            bitrate: bitrate.to_string(),
            normalized,
            duration_ms: duration_ms.max(0),
        };
        let mut inner = self.inner.lock();
        if let Some(old) = inner.mem_entries.remove(track_id) {
            inner.mem_order.retain(|id| id != track_id);
            if old.file_path != file_path {
                let _ = fs::remove_file(&old.file_path);
            }
        }
        inner.mem_entries.insert(track_id.to_string(), entry);
        inner.mem_order.push_back(track_id.to_string());
        while inner.mem_order.len() > MAX_MEM_TRACKS {
            let Some(oldest) = inner.mem_order.pop_front() else {
                break;
            };
            if let Some(old) = inner.mem_entries.remove(&oldest) {
                let _ = fs::remove_file(&old.file_path);
            }
        }
    }

    /// Drop all in-memory prepared tracks (e.g. when the player is turned off).
    pub fn clear_mem(&self) {
        let mut inner = self.inner.lock();
        for (_, entry) in inner.mem_entries.drain() {
            let _ = fs::remove_file(&entry.file_path);
        }
        inner.mem_order.clear();
    }

    /// Remove the in-memory track directory on graceful shutdown.
    pub fn shutdown_mem(&self) {
        let mut inner = self.inner.lock();
        inner.mem_entries.clear();
        inner.mem_order.clear();
        let _ = fs::remove_dir_all(&inner.mem_dir);
    }

    /// (`used_bytes`, `max_bytes`, `entry_count`)
    pub fn stats(&self) -> (i64, i64, usize) {
        let inner = self.inner.lock();
        (inner.total_size_bytes, inner.max_bytes, inner.entries.len())
    }

    /// Disk-cached tracks, most recently accessed first. Prunes entries whose files vanished.
    pub fn list(&self) -> Vec<CacheItem> {
        let mut inner = self.inner.lock();
        let mut missing = Vec::new();
        let mut items = Vec::with_capacity(inner.entries.len());
        for id in inner.order.iter().rev() {
            let Some(entry) = inner.entries.get(id) else {
                continue;
            };
            let path = Path::new(&entry.file_path);
            if !path.exists() {
                missing.push(id.clone());
                continue;
            }
            let file_name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| format!("{id}.ogg"));
            let name = path
                .file_stem()
                .map(|n| n.to_string_lossy().into_owned())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| file_name.clone());
            items.push(CacheItem {
                id: id.clone(),
                name,
                file_name,
                file_size: entry.file_size,
                bitrate: entry.bitrate.clone(),
                normalized: entry.normalized,
                source: entry.source.clone(),
                last_accessed_at: entry.last_accessed_at,
            });
        }
        if !missing.is_empty() {
            for id in &missing {
                inner.remove(id);
            }
            inner.save();
        }
        items
    }

    /// Look up a disk-cached track's `(path, file_name)` without altering LRU order.
    pub fn get_disk_entry(&self, track_id: &str) -> Option<(PathBuf, String)> {
        let mut inner = self.inner.lock();
        let path = PathBuf::from(&inner.entries.get(track_id)?.file_path);
        if !path.exists() {
            inner.remove(track_id);
            inner.save();
            return None;
        }
        let file_name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| format!("{track_id}.ogg"));
        Some((path, file_name))
    }

    /// Delete a single track from the disk cache. Returns `true` if an entry was removed.
    pub fn delete(&self, track_id: &str) -> bool {
        let mut inner = self.inner.lock();
        if !inner.entries.contains_key(track_id) {
            return false;
        }
        inner.remove(track_id);
        inner.save();
        true
    }

    /// Delete multiple tracks from the disk cache in one pass. Returns the count removed.
    pub fn delete_many(&self, track_ids: &[String]) -> usize {
        let mut inner = self.inner.lock();
        let mut removed = 0;
        for id in track_ids {
            if inner.entries.contains_key(id) {
                inner.remove(id);
                removed += 1;
            }
        }
        if removed > 0 {
            inner.save();
        }
        removed
    }

    /// Resolve existing disk paths for a deduplicated list of cached track IDs.
    pub fn get_disk_paths(&self, track_ids: &[String]) -> Vec<PathBuf> {
        let mut inner = self.inner.lock();
        let mut seen = std::collections::HashSet::with_capacity(track_ids.len());
        let mut missing = Vec::new();
        let mut paths = Vec::with_capacity(track_ids.len());
        for id in track_ids {
            if !seen.insert(id.as_str()) {
                continue;
            }
            let Some(entry) = inner.entries.get(id) else {
                continue;
            };
            let path = PathBuf::from(&entry.file_path);
            if path.exists() {
                paths.push(path);
            } else {
                missing.push(id.clone());
            }
        }
        if !missing.is_empty() {
            for id in &missing {
                inner.remove(id);
            }
            inner.save();
        }
        paths
    }

    pub fn set_max_mb(&self, max_mb: u64) {
        let mut inner = self.inner.lock();
        inner.max_bytes = max_bytes(max_mb);
        inner.evict_if_needed();
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

    fn remove_mem(&mut self, track_id: &str) {
        if let Some(old) = self.mem_entries.remove(track_id) {
            let _ = fs::remove_file(&old.file_path);
        }
        self.mem_order.retain(|id| id != track_id);
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

fn is_temp_cache_file(name: &str) -> bool {
    name.ends_with(".tmp")
        || name.ends_with(".tmp.ogg")
        || name.ends_with(".raw.flac")
        || name.ends_with(".raw.ogg")
        || name.ends_with(".raw.bin")
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
        } else if is_temp_cache_file(&name) {
            let _ = fs::remove_file(&path);
        }
    }
}

/// True when `file_path`'s stem equals `want_lower` (or `want_lower (N)` from collision suffixing).
fn stem_matches(file_path: &str, want_lower: &str) -> bool {
    let Some(stem) = Path::new(file_path).file_stem().and_then(|s| s.to_str()) else {
        return false;
    };
    let stem = stem.to_lowercase();
    if stem == want_lower {
        return true;
    }
    stem.strip_prefix(want_lower)
        .and_then(|rest| rest.strip_prefix(" ("))
        .and_then(|rest| rest.strip_suffix(')'))
        .is_some_and(|num| !num.is_empty() && num.chars().all(|c| c.is_ascii_digit()))
}

/// "<artist> - <title>", browsable and sanitized for filesystem safety.
pub(crate) fn file_stem(track: &Track) -> String {
    if track.artist.trim().is_empty() {
        return sanitize_filename(&track.name, "track");
    }
    let combined = format!("{} - {}", track.artist, track.name);
    sanitize_filename(&combined, "track")
}

/// Sanitize arbitrary text into a safe, non-hidden single-component filename stem.
pub(crate) fn sanitize_filename(raw: &str, fallback: &str) -> String {
    let mut cleaned = String::new();
    for c in raw.chars() {
        let ch = if c.is_control()
            || c.is_whitespace()
            || matches!(c, '/' | '\\' | '<' | '>' | ':' | '"' | '|' | '?' | '*')
        {
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

    let stem = stem.trim_matches(['.', ' ']);
    if stem.is_empty() {
        fallback.into()
    } else {
        stem.into()
    }
}

/// Unique `<stem>.<ext>` path inside `dir`, adding ` (1)`, ` (2)` on collision.
pub(crate) fn unique_path(dir: &Path, stem: &str, ext: &str) -> PathBuf {
    let mut path = dir.join(format!("{stem}.{ext}"));
    let mut idx = 1;
    while path.exists() {
        path = dir.join(format!("{stem} ({idx}).{ext}"));
        idx += 1;
    }
    path
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
            mem_dir: PathBuf::new(),
            meta_file: PathBuf::from("./_metadata.json"),
            max_bytes: i64::MAX,
            entries: HashMap::new(),
            order: VecDeque::new(),
            total_size_bytes: 0,
            mem_entries: HashMap::new(),
            mem_order: VecDeque::new(),
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
                    source: String::new(),
                    duration_ms: 0,
                },
            );
            inner.order.push_back((*id).to_string());
        }
        inner
    }

    #[test]
    fn touch_skips_hot_tracks_and_promotes_stale_ones() {
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
        // Windows-forbidden characters are stripped so downloaded files and zips extract cleanly.
        let win = sanitize_filename(r#"Track: "Who?" <1|2> *test*."#, "track");
        assert_eq!(win, "Track Who 1 2 test");
    }

    #[test]
    fn mem_cache_rolls_over_without_touching_disk_stats() {
        let base = std::env::temp_dir().join(format!("togewire-memtest-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let disk_dir = base.join("disk");
        let mem_dir = base.join("mem");
        fs::create_dir_all(&disk_dir).unwrap();
        fs::create_dir_all(&mem_dir).unwrap();

        let mgr = Manager {
            inner: Mutex::new(Inner {
                dir: disk_dir.clone(),
                mem_dir: mem_dir.clone(),
                meta_file: disk_dir.join("_metadata.json"),
                max_bytes: i64::MAX,
                entries: HashMap::new(),
                order: VecDeque::new(),
                total_size_bytes: 0,
                mem_entries: HashMap::new(),
                mem_order: VecDeque::new(),
            }),
        };

        let mut paths = Vec::new();
        for i in 0..=MAX_MEM_TRACKS {
            let p = mem_dir.join(format!("t{i}.ogg"));
            fs::write(&p, b"OggS").unwrap();
            mgr.put_mem(&format!("t{i}"), &p.to_string_lossy(), "192k", true, 0);
            paths.push(p);
        }

        // Oldest in-memory track was evicted and unlinked; newest remain; disk stats stay empty.
        assert!(!paths[0].exists());
        assert!(mgr.get_path("t0").is_none());
        let last = paths.last().unwrap().to_string_lossy().into_owned();
        assert_eq!(
            mgr.get_path(&format!("t{MAX_MEM_TRACKS}")).as_deref(),
            Some(last.as_str())
        );
        assert_eq!(mgr.stream_flags(&last), (true, true));
        assert_eq!(mgr.stats(), (0, i64::MAX, 0));

        mgr.clear_mem();
        assert!(mgr.get_path(&format!("t{MAX_MEM_TRACKS}")).is_none());
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn list_and_delete_manage_individual_disk_entries() {
        let dir = std::env::temp_dir().join(format!("togewire-listdel-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let mgr = Manager {
            inner: Mutex::new(Inner {
                dir: dir.clone(),
                mem_dir: dir.join("mem"),
                meta_file: dir.join("_metadata.json"),
                max_bytes: i64::MAX,
                entries: HashMap::new(),
                order: VecDeque::new(),
                total_size_bytes: 0,
                mem_entries: HashMap::new(),
                mem_order: VecDeque::new(),
            }),
        };

        let p1 = dir.join("Artist A - First.ogg");
        let p2 = dir.join("Artist B - Second.ogg");
        fs::write(&p1, vec![1u8; 100]).unwrap();
        fs::write(&p2, vec![2u8; 250]).unwrap();
        mgr.put(
            "id1",
            &p1.to_string_lossy(),
            "144k",
            false,
            "Spotify",
            200_000,
        );
        mgr.put(
            "id2",
            &p2.to_string_lossy(),
            "192k",
            true,
            "YouTube",
            240_000,
        );

        // Most recently added comes first.
        let items = mgr.list();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].id, "id2");
        assert_eq!(items[0].name, "Artist B - Second");
        assert_eq!(items[0].file_name, "Artist B - Second.ogg");
        assert_eq!(items[1].id, "id1");
        assert_eq!(mgr.stats().2, 2);

        assert_eq!(
            mgr.get_disk_entry("id1"),
            Some((p1.clone(), "Artist A - First.ogg".into()))
        );
        assert!(mgr.delete("id1"));
        assert!(!mgr.delete("id1"));
        assert!(!p1.exists());
        assert!(p2.exists());
        assert_eq!(mgr.stats(), (250, i64::MAX, 1));
        assert_eq!(mgr.list().len(), 1);

        let p3 = dir.join("Artist C - Third.ogg");
        fs::write(&p3, vec![3u8; 300]).unwrap();
        mgr.put(
            "id3",
            &p3.to_string_lossy(),
            "112k",
            false,
            "Spotify",
            180_000,
        );
        assert_eq!(
            mgr.get_disk_paths(&["id2".into(), "id3".into(), "id2".into(), "missing".into()]),
            vec![p2.clone(), p3.clone()]
        );
        assert_eq!(
            mgr.delete_many(&["id2".into(), "id3".into(), "missing".into()]),
            2
        );
        assert!(!p2.exists());
        assert!(!p3.exists());
        assert_eq!(mgr.stats(), (0, i64::MAX, 0));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn get_for_track_dedups_relinked_ids_and_same_stem_duration() {
        let dir = std::env::temp_dir().join(format!("togewire-relink-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let mgr = Manager {
            inner: Mutex::new(Inner {
                dir: dir.clone(),
                mem_dir: dir.join("mem"),
                meta_file: dir.join("_metadata.json"),
                max_bytes: i64::MAX,
                entries: HashMap::new(),
                order: VecDeque::new(),
                total_size_bytes: 0,
                mem_entries: HashMap::new(),
                mem_order: VecDeque::new(),
            }),
        };

        let p = dir.join("SuG - 幻燈.ogg");
        fs::write(&p, b"OggS").unwrap();
        let p_str = p.to_string_lossy().into_owned();
        mgr.put(
            "1KqGhBG3bmVpBVLckvmNga",
            &p_str,
            "160k",
            false,
            "ytdlp",
            247_000,
        );

        // Relinked playback ID matches via alt_ids even without duration.
        let by_alt = Track {
            id: "4uxqVCPIhyktvGz53SkZEa".into(),
            name: "幻燈".into(),
            artist: "SuG".into(),
            alt_ids: vec!["1KqGhBG3bmVpBVLckvmNga".into()],
            ..Default::default()
        };
        assert_eq!(mgr.get_for_track(&by_alt).as_deref(), Some(p_str.as_str()));

        // Independent album release with same stem & duration within 2s also reuses cache.
        let by_stem = Track {
            id: "differentReleaseId1234".into(),
            name: "幻燈".into(),
            artist: "SuG".into(),
            duration: 247_500,
            ..Default::default()
        };
        assert_eq!(mgr.get_for_track(&by_stem).as_deref(), Some(p_str.as_str()));

        // A meaningfully different duration (e.g. short cut) does not collide.
        let different_len = Track {
            id: "shortCutReleaseId12345".into(),
            name: "幻燈".into(),
            artist: "SuG".into(),
            duration: 90_000,
            ..Default::default()
        };
        assert!(mgr.get_for_track(&different_len).is_none());

        let _ = fs::remove_dir_all(&dir);
    }
}
