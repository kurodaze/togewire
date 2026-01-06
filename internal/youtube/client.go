package youtube

import (
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"regexp"
	"sort"
	"strconv"
	"strings"
	"sync"
	"time"

	"github.com/kurodaze/togewire/internal/cache"
	"github.com/kurodaze/togewire/internal/logger"
	"github.com/kurodaze/togewire/internal/types"
)

type Client struct {
	cache            *cache.Manager
	preparing        sync.Map      // track key -> bool
	failed           sync.Map      // track key -> bool
	downloading      sync.Map      // video ID -> bool
	downloadSem      chan struct{} // Semaphore to limit concurrent downloads
	lastDownloadTime time.Time     // Track last download activity
	downloadTimeMu   sync.RWMutex
}

type SearchResult struct {
	ID       string `json:"id"`
	Title    string `json:"title"`
	Uploader string `json:"uploader"`
	Duration int64  `json:"duration"`
}

type SearchQuery struct {
	Query string
	Type  string
	Limit int
}

// New creates a new YouTube downloader client
func New() *Client {
	// Initialize yt-dlp manager
	GetYtdlpManager()

	client := &Client{
		cache:            cache.New(),
		downloadSem:      make(chan struct{}, 2), // Limit to 2 concurrent downloads
		lastDownloadTime: time.Now(),
	}

	return client
}

// getCacheKey generates cache key using Spotify track ID or fallback
func (c *Client) getCacheKey(track *types.Track) string {
	if track.ID != "" {
		return track.ID
	}
	// Fallback to lowercase name and artist
	return fmt.Sprintf("%s|%s", strings.TrimSpace(strings.ToLower(track.Name)), strings.TrimSpace(strings.ToLower(track.Artist)))
}

func (c *Client) IsPreparingTrack(track *types.Track) bool {
	if track == nil {
		return false
	}

	trackKey := fmt.Sprintf("%s|%s", track.Name, track.Artist)
	_, preparing := c.preparing.Load(trackKey)
	return preparing
}

// PrepareTrack downloads and prepares a track for streaming
func (c *Client) PrepareTrack(track *types.Track) (string, error) {
	if track == nil || track.Name == "" || track.Artist == "" {
		return "", fmt.Errorf("insufficient track information")
	}

	trackKey := fmt.Sprintf("%s|%s", track.Name, track.Artist)
	cacheKey := c.getCacheKey(track)

	if _, loading := c.preparing.LoadOrStore(trackKey, true); loading {
		return "", fmt.Errorf("track already being prepared")
	}
	defer c.preparing.Delete(trackKey)

	if _, failed := c.failed.Load(trackKey); failed {
		return "", fmt.Errorf("track previously failed")
	}

	if cached, exists := c.cache.Get(cacheKey); exists {
		// Check if it's an error entry
		if cached.Error != "" {
			logger.Debug("Cached error: %s - %s (%s)", track.Name, track.Artist, cached.Error)
			return "", fmt.Errorf("%s", cached.Error)
		}

		logger.Debug("Cache hit: %s - %s", track.Name, track.Artist)

		// Update last accessed time for LRU tracking
		c.cache.MarkAccessed(cacheKey)

		return filepath.Abs(cached.FilePath)
	}
	videoID, queryType, err := c.searchYoutube(track)
	if err != nil {
		c.failed.Store(trackKey, true)
		logger.Debug("Search failed (won't retry): %s - %s", track.Name, track.Artist)

		// Store error in cache so it persists across restarts
		c.cache.SetError(cacheKey, track.Name, "search failed: no suitable video found")

		return "", fmt.Errorf("search failed: %w", err)
	}

	filePath, err := c.downloadAndCacheTrack(track, videoID, queryType)
	if err != nil {
		c.failed.Store(trackKey, true)

		// Store error in cache
		c.cache.SetError(cacheKey, track.Name, "download failed")

		return "", fmt.Errorf("download failed: %w", err)
	}

	return filePath, nil
}

// searchYoutube searches YouTube for a track and returns the best match
func (c *Client) searchYoutube(track *types.Track) (string, string, error) {
	queries := []SearchQuery{
		{
			Query: fmt.Sprintf(`%s %s "Provided to YouTube by"`, track.Name, track.Artist),
			Type:  "official",
			Limit: 3,
		},
		{
			Query: fmt.Sprintf("%s %s", track.Name, track.Artist),
			Type:  "general",
			Limit: 3,
		},
		{
			Query: fmt.Sprintf("%s - %s", track.Artist, track.Name),
			Type:  "fallback",
			Limit: 5,
		},
	}

	// Try each query
	for _, query := range queries {
		videoID, err := c.searchWithQuery(query, track)
		if err == nil && videoID != "" {
			return videoID, query.Type, nil
		}
	}

	return "", "", fmt.Errorf("no suitable video found")
}

func (c *Client) searchWithQuery(query SearchQuery, track *types.Track) (string, error) {
	mgr := GetYtdlpManager()
	cmd, err := mgr.RunCommand(
		"--dump-json",
		"--skip-download",
		"--playlist-end", strconv.Itoa(query.Limit),
		"--no-warnings",
		"--quiet",
		fmt.Sprintf("ytsearch%d:%s", query.Limit, query.Query))

	if err != nil {
		return "", fmt.Errorf("failed to create command: %w", err)
	}

	output, err := cmd.CombinedOutput()

	// to prevent restricted videos from blocking
	results, parseErr := c.parseSearchResults(string(output))

	if err != nil && len(results) == 0 {
		if updateErr := mgr.HandleFailure(err); updateErr == nil {
			return c.searchWithQuery(query, track)
		}
		logger.Debug("No results for %s query", query.Type)
		return "", fmt.Errorf("search command failed: %w", err)
	}

	mgr.ResetFailures()

	if parseErr != nil || len(results) == 0 {
		logger.Debug("No valid results for %s query", query.Type)
		return "", fmt.Errorf("no valid results")
	}

	logger.Debug("Found %d results (%s query)", len(results), query.Type)
	for i, result := range results {
		logger.Debug("  %d. '%s' by %s", i+1, result.Title, result.Uploader)
	}

	bestMatch := c.findBestMatch(results, track, query)
	if bestMatch != nil {
		return bestMatch.ID, nil
	}

	return "", fmt.Errorf("no suitable match found")
}

// parseSearchResults parses yt-dlp JSON output into SearchResult structs
func (c *Client) parseSearchResults(output string) ([]*SearchResult, error) {
	var results []*SearchResult

	lines := strings.Split(strings.TrimSpace(output), "\n")
	for _, line := range lines {
		if strings.TrimSpace(line) == "" {
			continue
		}

		var data map[string]interface{}
		if err := json.Unmarshal([]byte(line), &data); err != nil {
			continue
		}

		result := &SearchResult{
			ID:       getStringValue(data, "id"),
			Title:    getStringValue(data, "title"),
			Uploader: getStringValue(data, "uploader"),
		}

		if durationFloat, ok := data["duration"].(float64); ok {
			result.Duration = int64(durationFloat)
		}

		if result.ID != "" && result.Title != "" {
			results = append(results, result)
		}
	}

	return results, nil
}

// getStringValue safely extracts string values from map[string]interface{}
func getStringValue(data map[string]interface{}, key string) string {
	if val, exists := data[key]; exists {
		if str, ok := val.(string); ok {
			return str
		}
	}
	return ""
}

// findBestMatch finds the best matching video from search results
func (c *Client) findBestMatch(results []*SearchResult, track *types.Track, query SearchQuery) *SearchResult {
	if len(results) == 0 {
		return nil
	}

	trackLower := strings.ToLower(track.Name)
	artistLower := strings.ToLower(track.Artist)
	targetDuration := track.Duration / 1000

	artistNames := c.extractArtistNames(c.normalizeArtistName(artistLower))
	trackWords := c.extractSignificantWords(trackLower)

	type match struct {
		score   int
		result  *SearchResult
		index   int
		reasons []string
	}

	var matches []match

	for i, result := range results {
		titleLower := strings.ToLower(result.Title)
		uploaderLower := strings.ToLower(result.Uploader)

		// Skip if duration doesn't match tolerance
		if targetDuration > 0 && result.Duration > 0 {
			diff := abs(result.Duration - targetDuration)
			maxDiff := int64(6)
			if query.Type == "official" {
				maxDiff = 3
			}
			if diff > maxDiff {
				continue
			}
		}

		var score int
		var reasons []string

		// Official query: highest priority for "Provided to YouTube" results
		if query.Type == "official" {
			score = 100
			reasons = []string{"Official + duration match"}
		} else {
			// Track name matching (+50)
			if c.allWordsInText(trackWords, titleLower) {
				score += 50
				reasons = append(reasons, "Title match")
			}

			// Artist matching (+30 in title, +40 in uploader)
			for _, artist := range artistNames {
				if strings.Contains(titleLower, artist) {
					score += 30
					reasons = append(reasons, "Artist in title")
					break
				} else if strings.Contains(uploaderLower, artist) {
					score += 40
					reasons = append(reasons, "Artist in uploader")
					break
				}
			}

			// Duration bonus for fallback queries (+30 if ≤2s diff, +20 if ≤3s diff)
			if query.Type == "fallback" && targetDuration > 0 && result.Duration > 0 {
				diff := abs(result.Duration - targetDuration)
				if diff <= 2 {
					score += 30
					reasons = append(reasons, "Duration match")
				} else if diff <= 3 {
					score += 20
					reasons = append(reasons, "Duration close")
				}
			}
		}

		matches = append(matches, match{score, result, i, reasons})
	}

	if len(matches) == 0 {
		logger.Debug("No valid matches found")
		return nil
	}

	sort.Slice(matches, func(i, j int) bool {
		return matches[i].score > matches[j].score
	})

	best := matches[0]
	minScore := 60
	if query.Type == "fallback" {
		minScore = 70
	}

	if best.score >= minScore {
		logger.Debug("Chosen result: [%d] %s (score: %d)",
			best.index+1, strings.Join(best.reasons, " + "), best.score)
		return best.result
	}

	logger.Debug("Score too low: %d for '%s'",
		best.score, best.result.Title)
	return nil
}

// Compile regex patterns once
var (
	separatorsRe  = regexp.MustCompile(`[,&]`)
	featRe        = regexp.MustCompile(`\b(feat\.|ft\.)\b`)
	decorationsRe = regexp.MustCompile(`[†‡§¶•◦▪▫‣⁃]`)
	nonAlphanumRe = regexp.MustCompile(`[^\w\s]`)
)

// Helper functions for text processing
func (c *Client) normalizeArtistName(artist string) string {
	// Replace common separators and decorations
	artist = separatorsRe.ReplaceAllString(artist, " ")
	artist = featRe.ReplaceAllString(artist, " ")
	artist = decorationsRe.ReplaceAllString(artist, " ")
	artist = nonAlphanumRe.ReplaceAllString(artist, " ")
	return artist
}

func (c *Client) extractArtistNames(normalized string) []string {
	words := strings.Fields(normalized)
	names := make([]string, 0, len(words))
	for _, word := range words {
		// Fields already trims whitespace, and normalized is already lowercase
		if len(word) > 2 {
			names = append(names, word)
		}
	}
	return names
}

func (c *Client) extractSignificantWords(text string) []string {
	words := strings.Fields(text)
	result := make([]string, 0, len(words))
	for _, word := range words {
		if len(word) > 2 {
			result = append(result, word)
		}
	}
	return result
}

func (c *Client) allWordsInText(words []string, text string) bool {
	for _, word := range words {
		if !strings.Contains(text, word) {
			return false
		}
	}
	return len(words) > 0
}

// abs returns absolute value of int64
func abs(x int64) int64 {
	if x < 0 {
		return -x
	}
	return x
}

func (c *Client) downloadAndCacheTrack(track *types.Track, videoID, queryType string) (string, error) {
	if _, downloading := c.downloading.LoadOrStore(videoID, true); downloading {
		return "", fmt.Errorf("video already being downloaded")
	}
	defer c.downloading.Delete(videoID)

	// Acquire semaphore to limit concurrent downloads
	c.downloadSem <- struct{}{}
	defer func() { <-c.downloadSem }()

	// Try multiple download methods with fallback
	methods := []struct {
		name   string
		format string
	}{
		{"best_audio", "ba"},
		{"codec_fallback", "bestaudio[ext=webm]/bestaudio[ext=m4a]/bestaudio"},
		{"video_combo", "worstvideo+bestaudio/best"},
	}

	var lastErr error
	for _, method := range methods {
		filePath, err := c.tryDownloadMethod(videoID, method.format, method.name)
		if err != nil {
			lastErr = err
			logger.Debug("Method %s failed: %v", method.name, err)
			continue
		}

		// Success - cache the result
		cacheKey := c.getCacheKey(track)
		now := time.Now().Unix()
		entry := &cache.Entry{
			VideoID:        videoID,
			Title:          fmt.Sprintf("%s - %s", track.Name, track.Artist),
			CachedAt:       now,
			LastAccessedAt: now, // Set initial access time to download time
			DurationMS:     track.Duration,
			FilePath:       filePath,
			DownloadMethod: method.name,
			QueryType:      queryType,
		}

		// Get file size
		if info, err := os.Stat(filePath); err == nil {
			entry.FileSize = info.Size()
		}

		c.cache.Add(cacheKey, entry)

		// Track download activity
		c.downloadTimeMu.Lock()
		c.lastDownloadTime = time.Now()
		c.downloadTimeMu.Unlock()

		logger.Debug("Downloaded [%s/%s]: %s",
			method.name, queryType, filepath.Base(filePath))

		return filepath.Abs(filePath)
	}

	return "", fmt.Errorf("all download methods failed: %v", lastErr)
}

func (c *Client) tryDownloadMethod(videoID, format, methodName string) (string, error) {
	outputTemplate := c.cache.GetOutputTemplate()

	args := []string{
		"--format", format,
		"--output", outputTemplate,
		"--no-playlist",
		"--quiet",
		"--no-warnings",
		"--extract-audio",
		"--audio-format", "opus",
		"--audio-quality", "0",
	}

	args = append(args, fmt.Sprintf("https://www.youtube.com/watch?v=%s", videoID))

	mgr := GetYtdlpManager()
	cmd, err := mgr.RunCommand(args...)
	if err != nil {
		return "", fmt.Errorf("failed to create command: %w", err)
	}

	if err := cmd.Run(); err != nil {
		if updateErr := mgr.HandleFailure(err); updateErr == nil {
			return c.tryDownloadMethod(videoID, format, methodName)
		}
		return "", fmt.Errorf("download command failed: %w", err)
	}

	mgr.ResetFailures()

	return c.findDownloadedFile(videoID)
}

func (c *Client) findDownloadedFile(videoID string) (string, error) {
	return c.cache.FindDownloadedFile(videoID)
}

func (c *Client) GetCacheStats() cache.Stats {
	return c.cache.GetStats()
}

func (c *Client) ClearCache() error {
	return c.cache.Clear()
}

// UpgradeTrack attempts to re-download a track with best_audio method
// Returns error if upgrade fails, nil on success
func (c *Client) UpgradeTrack(cacheKey string, entry *cache.Entry) error {
	logger.Debug("Upgrading track quality: %s (from %s to best_audio)", entry.Title, entry.DownloadMethod)

	// Try to download with best_audio only (don't remove old file yet)
	filePath, err := c.tryDownloadMethod(entry.VideoID, "ba", "best_audio")
	if err != nil {
		logger.Warn("Could not upgrade track, keeping %s quality: %s", entry.DownloadMethod, entry.Title)
		// Mark as upgrade failed to avoid retrying
		c.cache.MarkUpgradeFailed(cacheKey)
		return fmt.Errorf("upgrade failed for %s: %w", entry.Title, err)
	}

	// Success - remove old file
	if err := os.Remove(entry.FilePath); err != nil {
		logger.Warn("Failed to remove old file: %v", err)
	}

	// Update cache entry
	now := time.Now().Unix()
	entry.FilePath = filePath
	entry.DownloadMethod = "best_audio"
	entry.LastAccessedAt = now

	// Get new file size
	if info, err := os.Stat(filePath); err == nil {
		entry.FileSize = info.Size()
	}

	c.cache.Add(cacheKey, entry)

	// Track download activity
	c.downloadTimeMu.Lock()
	c.lastDownloadTime = time.Now()
	c.downloadTimeMu.Unlock()

	logger.Debug("Successfully upgraded: %s", entry.Title)
	return nil
}

func (c *Client) GetEntriesNeedingUpgrade() map[string]*cache.Entry {
	return c.cache.GetEntriesNeedingUpgrade()
}

func (c *Client) GetTimeSinceLastDownload() time.Duration {
	c.downloadTimeMu.RLock()
	defer c.downloadTimeMu.RUnlock()
	return time.Since(c.lastDownloadTime)
}

func (c *Client) IsCurrentlyDownloading() bool {
	isDownloading := false
	c.downloading.Range(func(key, value interface{}) bool {
		isDownloading = true
		return false // Stop iteration
	})
	return isDownloading
}
