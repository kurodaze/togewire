package cache

import (
	"container/list"
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

const CacheDir = "data/youtube_cache"

// Manager keeps metadata, LRU order, and disk persistence for cached audio.
type Manager struct {
	dir      string
	metaFile string
	entries  map[string]*Entry
	order    *list.List
	nodes    map[string]*list.Element
	mu       sync.Mutex
}

// Entry describes a cached download.
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
	Error          string `json:"error,omitempty"`
	UpgradeFailed  bool   `json:"upgrade_failed,omitempty"` // true if track can't be upgraded to best_audio
}

// Stats reports cache status.
type Stats struct {
	Tracks int   `json:"tracks"`
	Files  int   `json:"files"`
	SizeMB int64 `json:"size_mb"`
}

var validExtensions = []string{".opus", ".webm", ".ogg", ".m4a", ".mp4", ".mkv"}

func IsValidAudioExtension(ext string) bool {
	ext = strings.ToLower(ext)
	for _, valid := range validExtensions {
		if ext == valid {
			return true
		}
	}
	return false
}

func New() *Manager {
	if err := os.MkdirAll(CacheDir, 0o755); err != nil {
		logger.Error("Failed to create cache directory: %v", err)
	}

	m := &Manager{
		dir:      CacheDir,
		metaFile: filepath.Join(CacheDir, "_metadata.json"),
		entries:  make(map[string]*Entry),
		order:    list.New(),
		nodes:    make(map[string]*list.Element),
	}

	m.load()
	return m
}

func (m *Manager) load() {
	data, err := os.ReadFile(m.metaFile)
	if err != nil {
		return
	}

	var entries map[string]*Entry
	if err := json.Unmarshal(data, &entries); err != nil {
		logger.Error("loading cache metadata: %v", err)
		return
	}

	m.mu.Lock()
	m.entries = make(map[string]*Entry, len(entries))
	errorCount := 0
	for key, entry := range entries {
		// Skip error entries - allow retry on restart
		if entry.Error != "" {
			errorCount++
			continue
		}
		// Skip entries without files
		if entry.FilePath == "" {
			continue
		}
		// Validate file exists and get size
		if info, err := os.Stat(entry.FilePath); err == nil {
			if entry.FileSize == 0 {
				entry.FileSize = info.Size()
			}
			m.entries[key] = entry
		}
	}
	m.rebuildLocked()
	// Save to remove error entries from disk
	if errorCount > 0 {
		m.saveLocked()
	}
	m.mu.Unlock()

	logger.Info("Loaded %d cached tracks", len(m.entries))
}

func (m *Manager) rebuildLocked() {
	m.order = list.New()
	m.nodes = make(map[string]*list.Element, len(m.entries))

	type item struct {
		key string
		at  int64
	}

	items := make([]item, 0, len(m.entries))
	for key, entry := range m.entries {
		at := entry.LastAccessedAt
		if at == 0 {
			at = entry.CachedAt
		}
		items = append(items, item{key: key, at: at})
	}

	sort.Slice(items, func(i, j int) bool {
		return items[i].at > items[j].at
	})

	for _, it := range items {
		m.nodes[it.key] = m.order.PushBack(it.key)
	}
}

func (m *Manager) saveLocked() {
	data, err := json.MarshalIndent(m.entries, "", "  ")
	if err != nil {
		logger.Error("marshaling cache metadata: %v", err)
		return
	}
	if err := os.WriteFile(m.metaFile, data, 0o644); err != nil {
		logger.Error("saving cache metadata: %v", err)
	}
}

func (m *Manager) touchLocked(key string, entry *Entry) {
	entry.LastAccessedAt = time.Now().Unix()
	if node, ok := m.nodes[key]; ok {
		m.order.MoveToFront(node)
		return
	}
	m.nodes[key] = m.order.PushFront(key)
}

func (m *Manager) removeLocked(key string) {
	delete(m.entries, key)
	if node, ok := m.nodes[key]; ok {
		m.order.Remove(node)
		delete(m.nodes, key)
	}
}

func (m *Manager) Get(key string) (*Entry, bool) {
	m.mu.Lock()
	defer m.mu.Unlock()

	entry, ok := m.entries[key]
	if !ok {
		return nil, false
	}
	if entry.FilePath != "" {
		if _, err := os.Stat(entry.FilePath); err != nil {
			m.removeLocked(key)
			m.saveLocked()
			return nil, false
		}
	}
	m.touchLocked(key, entry)
	return entry, true
}

func (m *Manager) MarkAccessed(key string) {
	m.mu.Lock()
	if entry, ok := m.entries[key]; ok {
		m.touchLocked(key, entry)
	}
	m.mu.Unlock()
}

func (m *Manager) Add(key string, entry *Entry) {
	now := time.Now().Unix()
	if entry.CachedAt == 0 {
		entry.CachedAt = now
	}
	entry.LastAccessedAt = now
	if entry.FilePath != "" && entry.FileSize == 0 {
		if info, err := os.Stat(entry.FilePath); err == nil {
			entry.FileSize = info.Size()
		}
	}

	m.mu.Lock()
	m.entries[key] = entry
	m.touchLocked(key, entry)
	m.saveLocked()
	m.mu.Unlock()

	m.enforceLimit()
}

func (m *Manager) SetError(key, title, errorMsg string) {
	entry := &Entry{
		Title:          title,
		CachedAt:       time.Now().Unix(),
		LastAccessedAt: time.Now().Unix(),
		Error:          errorMsg,
	}
	m.mu.Lock()
	m.entries[key] = entry
	m.touchLocked(key, entry)
	m.saveLocked()
	m.mu.Unlock()
}

func (m *Manager) pruneMissingLocked() int {
	removed := 0
	for key, entry := range m.entries {
		if entry.FilePath == "" {
			continue
		}
		if _, err := os.Stat(entry.FilePath); err != nil {
			m.removeLocked(key)
			removed++
		}
	}
	if removed > 0 {
		m.saveLocked()
	}
	return removed
}

func (m *Manager) GetStats() Stats {
	m.mu.Lock()
	m.pruneMissingLocked()
	tracks := 0
	var size int64
	files := 0
	for _, entry := range m.entries {
		if entry.Error != "" || entry.FilePath == "" {
			continue
		}
		tracks++
		if entry.FileSize == 0 {
			if info, err := os.Stat(entry.FilePath); err == nil {
				entry.FileSize = info.Size()
			}
		}
		if entry.FileSize == 0 {
			continue
		}
		size += entry.FileSize
		files++
	}
	m.mu.Unlock()

	return Stats{Tracks: tracks, Files: files, SizeMB: size / (1024 * 1024)}
}

func (m *Manager) diskUsageLocked() int64 {
	var total int64
	for _, entry := range m.entries {
		total += entry.FileSize
	}
	return total
}

func (m *Manager) Clear() error {
	m.mu.Lock()
	m.entries = make(map[string]*Entry)
	m.order = list.New()
	m.nodes = make(map[string]*list.Element)
	m.saveLocked()
	m.mu.Unlock()

	os.Remove(m.metaFile)
	removed := 0
	for _, ext := range validExtensions {
		pattern := filepath.Join(m.dir, "*"+ext)
		files, err := filepath.Glob(pattern)
		if err != nil {
			continue
		}
		for _, file := range files {
			if err := os.Remove(file); err == nil {
				removed++
			} else {
				logger.Warn("Failed to remove cached file %s: %v", file, err)
			}
		}
	}
	logger.Info("Cache cleared: %d files removed", removed)
	return nil
}

func (m *Manager) enforceLimit() {
	maxMB := config.Get().CacheMaxSizeMB
	if maxMB <= 0 {
		return
	}
	maxBytes := int64(maxMB) * 1024 * 1024
	for {
		m.mu.Lock()
		usage := m.diskUsageLocked()
		if usage <= maxBytes {
			m.mu.Unlock()
			return
		}
		node := m.order.Back()
		if node == nil {
			m.mu.Unlock()
			return
		}
		key := node.Value.(string)
		entry := m.entries[key]
		if entry == nil {
			m.order.Remove(node)
			delete(m.nodes, key)
			m.mu.Unlock()
			continue
		}
		victim := *entry
		m.removeLocked(key)
		m.saveLocked()
		m.mu.Unlock()

		if victim.FilePath != "" {
			if err := os.Remove(victim.FilePath); err == nil {
				logger.Info("Removed (LRU): %s", filepath.Base(victim.FilePath))
			}
		}
	}
}

func (m *Manager) FindDownloadedFile(videoID string) (string, error) {
	pattern := filepath.Join(m.dir, "*"+videoID+"*")
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

func (m *Manager) GetOutputTemplate() string {
	return filepath.Join(m.dir, "%(title)s [%(id)s].%(ext)s")
}

// MarkUpgradeFailed marks a track as unable to be upgraded to best_audio quality
func (m *Manager) MarkUpgradeFailed(key string) {
	m.mu.Lock()
	defer m.mu.Unlock()

	if entry, ok := m.entries[key]; ok {
		entry.UpgradeFailed = true
		m.saveLocked()
	}
}

func (m *Manager) GetEntriesNeedingUpgrade() map[string]*Entry {
	m.mu.Lock()
	defer m.mu.Unlock()

	needs := make(map[string]*Entry)
	for key, entry := range m.entries {
		// Skip if already best_audio, has error, or upgrade previously failed
		if entry.DownloadMethod != "best_audio" && entry.Error == "" && !entry.UpgradeFailed {
			needs[key] = entry
		}
	}
	return needs
}
