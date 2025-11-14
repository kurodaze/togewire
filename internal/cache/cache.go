package cache

import (
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"sort"
	"strings"
	"sync"
	"time"

	"github.com/kurodaze/togewire/internal/config"
	"github.com/kurodaze/togewire/internal/logger"
)

const (
	CacheDir = "data/youtube_cache"
)

// Manager handles all cache operations with LRU eviction
type Manager struct {
	cacheDir  string
	cacheFile string
	entries   map[string]*Entry
	mu        sync.RWMutex
}

// Entry represents a cached file with metadata
type Entry struct {
	VideoID        string `json:"video_id"`
	Title          string `json:"title"`
	CachedAt       int64  `json:"cached_at"`
	LastAccessedAt int64  `json:"last_accessed_at"`
	DurationMS     int64  `json:"duration_ms"`
	FilePath       string `json:"file_path"`
	FileSize       int64  `json:"file_size"`
	DownloadMethod string `json:"download_method"`
	QueryType      string `json:"query_type"`
}

// Stats represents cache statistics
type Stats struct {
	Songs  int   `json:"songs"`
	Files  int   `json:"files"`
	SizeMB int64 `json:"size_mb"`
}

var validExtensions = []string{".opus", ".webm", ".ogg", ".m4a", ".mp4", ".mkv"}

// IsValidAudioExtension checks if a file extension is valid for audio files
func IsValidAudioExtension(ext string) bool {
	ext = strings.ToLower(ext)
	for _, valid := range validExtensions {
		if ext == valid {
			return true
		}
	}
	return false
}

// create new cache manager
func New() *Manager {
	cacheDir := CacheDir
	cacheFile := filepath.Join(cacheDir, "_metadata.json")

	// Create cache directory if it doesn't exist
	if err := os.MkdirAll(cacheDir, 0755); err != nil {
		logger.Info("Failed to create cache directory: %v", err)
	}

	m := &Manager{
		cacheDir:  cacheDir,
		cacheFile: cacheFile,
		entries:   make(map[string]*Entry),
	}

	m.load()
	return m
}

// reads cache metadata from disk
func (m *Manager) load() {
	data, err := os.ReadFile(m.cacheFile)
	if err != nil {
		return // File doesn't exist yet, which is fine
	}

	if err := json.Unmarshal(data, &m.entries); err != nil {
		logger.Error(" loading cache metadata: %v", err)
		m.entries = make(map[string]*Entry)
	} else {
		logger.Info("Loaded %d cached songs", len(m.entries))
	}
}

// writes cache metadata to disk
func (m *Manager) save() {
	m.mu.Lock()
	defer m.mu.Unlock()

	data, err := json.MarshalIndent(m.entries, "", "  ")
	if err != nil {
		logger.Error(" marshaling cache metadata: %v", err)
		return
	}

	if err := os.WriteFile(m.cacheFile, data, 0644); err != nil {
		logger.Error(" saving cache metadata: %v", err)
	}
}

// retrieves a cache entry by key
func (m *Manager) Get(key string) (*Entry, bool) {
	m.mu.RLock()
	defer m.mu.RUnlock()

	entry, exists := m.entries[key]
	if !exists {
		return nil, false
	}

	// Verify file exists
	if _, err := os.Stat(entry.FilePath); os.IsNotExist(err) {
		return nil, false
	}

	return entry, true
}

// updates the last accessed time for a cache entry
func (m *Manager) MarkAccessed(key string) {
	m.mu.Lock()
	defer m.mu.Unlock()

	if entry, exists := m.entries[key]; exists {
		entry.LastAccessedAt = time.Now().Unix()
	}
}

// adds a new entry to the cache
func (m *Manager) Add(key string, entry *Entry) {
	m.mu.Lock()
	m.entries[key] = entry
	m.mu.Unlock()

	m.save()
	m.CleanupIfNeeded()
}

// removes an entry from the cache
func (m *Manager) Remove(key string) {
	m.mu.Lock()
	defer m.mu.Unlock()

	delete(m.entries, key)
}

// removes entries for files that no longer exist on disk
func (m *Manager) cleanupStaleEntries() {
	m.mu.Lock()
	staleCount := 0
	for key, entry := range m.entries {
		if entry.FilePath == "" || !fileExists(entry.FilePath) {
			delete(m.entries, key)
			staleCount++
		}
	}
	m.mu.Unlock()

	if staleCount > 0 {
		m.save()
		logger.Info("Cleaned up %d stale cache entries", staleCount)
	}
}

// fileExists checks if a file exists
func fileExists(path string) bool {
	_, err := os.Stat(path)
	return err == nil
}

// returns cache statistics
func (m *Manager) GetStats() Stats {
	m.cleanupStaleEntries()

	m.mu.RLock()
	defer m.mu.RUnlock()

	totalSize, totalFiles := m.calculateSize()

	return Stats{
		Songs:  len(m.entries),
		Files:  totalFiles,
		SizeMB: totalSize / (1024 * 1024),
	}
}

// calculates total cache size and file count
func (m *Manager) calculateSize() (int64, int) {
	var totalSize int64
	totalFiles := 0

	for _, ext := range validExtensions {
		pattern := filepath.Join(m.cacheDir, "*"+ext)
		files, err := filepath.Glob(pattern)
		if err != nil {
			continue
		}

		for _, file := range files {
			if info, err := os.Stat(file); err == nil {
				totalSize += info.Size()
				totalFiles++
			}
		}
	}

	return totalSize, totalFiles
}

// Clear removes all cached files and metadata
func (m *Manager) Clear() error {
	m.mu.Lock()
	defer m.mu.Unlock()

	// Clear memory cache
	m.entries = make(map[string]*Entry)

	// Remove metadata file
	os.Remove(m.cacheFile)

	// Remove all audio files
	removedCount := 0

	for _, ext := range validExtensions {
		pattern := filepath.Join(m.cacheDir, "*"+ext)
		files, err := filepath.Glob(pattern)
		if err != nil {
			continue
		}

		for _, file := range files {
			if err := os.Remove(file); err != nil {
				logger.Info("Failed to remove cached file %s: %v", file, err)
			} else {
				removedCount++
			}
		}
	}

	logger.Info("Cache cleared: %d files removed", removedCount)
	return nil
}

// removes least recently used files if cache exceeds size limit
func (m *Manager) CleanupIfNeeded() {
	cfg := config.Get()
	if cfg.CacheMaxSizeMB <= 0 {
		return // Unlimited cache
	}

	totalSize, _ := m.calculateSize()
	maxSizeBytes := int64(cfg.CacheMaxSizeMB) * 1024 * 1024

	if totalSize <= maxSizeBytes {
		return // Under limit
	}

	logger.Info("Cache cleanup (LRU): %d MB exceeds %d MB limit",
		totalSize/(1024*1024), cfg.CacheMaxSizeMB)

	// Build list of files sorted by LRU
	type fileInfo struct {
		path           string
		size           int64
		key            string
		lastAccessedAt int64
		cachedAt       int64
	}

	var files []fileInfo

	m.mu.RLock()
	for key, entry := range m.entries {
		if entry.FilePath == "" {
			continue
		}

		// Verify file exists and get current size
		info, err := os.Stat(entry.FilePath)
		if err != nil {
			continue
		}

		// Use LastAccessedAt for LRU, fallback to CachedAt if not set
		accessTime := entry.LastAccessedAt
		if accessTime == 0 {
			accessTime = entry.CachedAt
		}

		files = append(files, fileInfo{
			path:           entry.FilePath,
			size:           info.Size(),
			key:            key,
			lastAccessedAt: accessTime,
			cachedAt:       entry.CachedAt,
		})
	}
	m.mu.RUnlock()

	// Sort by last accessed time (least recently used first)
	sort.Slice(files, func(i, j int) bool {
		return files[i].lastAccessedAt < files[j].lastAccessedAt
	})

	// Remove files until under limit
	removedCount := 0
	currentSize := totalSize
	var removedKeys []string

	for _, file := range files {
		if currentSize <= maxSizeBytes {
			break
		}

		if err := os.Remove(file.path); err == nil {
			currentSize -= file.size
			removedCount++
			removedKeys = append(removedKeys, file.key)

			// Log with last access time
			lastAccess := time.Unix(file.lastAccessedAt, 0)
			daysSinceAccess := time.Since(lastAccess).Hours() / 24
			logger.Info("Removed (LRU): %s (last played %.1f days ago)",
				filepath.Base(file.path), daysSinceAccess)
		}
	}

	if removedCount > 0 {
		// Remove entries from cache
		m.mu.Lock()
		for _, key := range removedKeys {
			delete(m.entries, key)
		}
		m.mu.Unlock()

		m.save()

		freedMB := (totalSize - currentSize) / (1024 * 1024)
		logger.Info("LRU cleanup complete: %d files removed (freed %d MB)",
			removedCount, freedMB)

		// Clean up any stale entries
		m.cleanupStaleEntries()
	}
}

// FindDownloadedFile finds a downloaded file by video ID
func (m *Manager) FindDownloadedFile(videoID string) (string, error) {
	pattern := filepath.Join(m.cacheDir, "*"+videoID+"*")
	files, err := filepath.Glob(pattern)
	if err != nil {
		return "", fmt.Errorf("failed to search for downloaded file: %w", err)
	}

	for _, file := range files {
		for _, ext := range validExtensions {
			if filepath.Ext(file) == ext {
				return file, nil
			}
		}
	}

	return "", fmt.Errorf("downloaded file not found")
}

// returns the yt-dlp output template for the cache directory
func (m *Manager) GetOutputTemplate() string {
	return filepath.Join(m.cacheDir, "%(title)s [%(id)s].%(ext)s")
}

// returns cache entries that were downloaded with fallback methods
func (m *Manager) GetEntriesNeedingUpgrade() map[string]*Entry {
	m.mu.RLock()
	defer m.mu.RUnlock()

	needsUpgrade := make(map[string]*Entry)
	for key, entry := range m.entries {
		// Only upgrade entries that used fallback methods
		if entry.DownloadMethod != "best_audio" {
			needsUpgrade[key] = entry
		}
	}

	return needsUpgrade
}
