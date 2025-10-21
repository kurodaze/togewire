package youtube

import (
	"fmt"
	"io"
	"log"
	"net/http"
	"os"
	"os/exec"
	"path/filepath"
	"runtime"
	"sync"
	"time"
)

const (
	updateCheckFile = ".ytdlp_last_update"
)

var (
	ytdlpManager     *YtdlpManager
	ytdlpManagerOnce sync.Once
)

type YtdlpManager struct {
	binPath      string
	binDir       string
	mu           sync.RWMutex
	lastUpdate   time.Time
	failureCount int
}

// GetYtdlpManager returns the singleton yt-dlp manager
func GetYtdlpManager() *YtdlpManager {
	ytdlpManagerOnce.Do(func() {
		binDir := filepath.Join(".", "bin")
		if err := os.MkdirAll(binDir, 0755); err != nil {
			log.Printf("Failed to create bin directory: %v", err)
		}

		ytdlpManager = &YtdlpManager{
			binDir:  binDir,
			binPath: filepath.Join(binDir, getYtdlpBinaryName()),
		}

		// Initialize yt-dlp
		if err := ytdlpManager.ensureYtdlp(); err != nil {
			log.Printf("Failed to initialize yt-dlp: %v", err)
		}
	})
	return ytdlpManager
}

// getYtdlpBinaryName returns the binary name
func getYtdlpBinaryName() string {
	if runtime.GOOS == "windows" {
		return "yt-dlp.exe"
	}
	return "yt-dlp"
}

// ensureYtdlp ensures yt-dlp is available and up-to-date
func (m *YtdlpManager) ensureYtdlp() error {
	m.mu.Lock()
	defer m.mu.Unlock()

	// Check if binary exists
	if !m.fileExists(m.binPath) {
		log.Println("yt-dlp not found, downloading...")
		return m.downloadYtdlp()
	}

	// Check if update is needed (every 7 days)
	if m.shouldUpdate() {
		log.Println("Checking for yt-dlp updates...")
		if err := m.updateYtdlp(); err != nil {
			log.Printf("Update check failed: %v", err)
		}
	}

	return nil
}

// GetBinaryPath returns the path to the yt-dlp binary
func (m *YtdlpManager) GetBinaryPath() string {
	m.mu.RLock()
	defer m.mu.RUnlock()
	return m.binPath
}

// RunCommand executes yt-dlp with the given arguments, auto-updating on failure
func (m *YtdlpManager) RunCommand(args ...string) (*exec.Cmd, error) {
	m.mu.RLock()
	binPath := m.binPath
	m.mu.RUnlock()

	cmd := exec.Command(binPath, args...)
	return cmd, nil
}

// HandleFailure increments failure count and triggers update if threshold reached
func (m *YtdlpManager) HandleFailure(err error) error {
	m.mu.Lock()
	m.failureCount++
	count := m.failureCount
	m.mu.Unlock()

	// Auto-update after 3 consecutive failures
	if count >= 3 {
		log.Println("Multiple yt-dlp failures, attempting auto-update...")
		if updateErr := m.ForceUpdate(); updateErr != nil {
			return fmt.Errorf("original error: %v, update failed: %v", err, updateErr)
		}

		m.mu.Lock()
		m.failureCount = 0 // Reset counter after successful update
		m.mu.Unlock()

		log.Println("yt-dlp updated, retrying...")
		return nil
	}

	return err
}

// ResetFailures resets the failure counter (call after successful operation)
func (m *YtdlpManager) ResetFailures() {
	m.mu.Lock()
	m.failureCount = 0
	m.mu.Unlock()
}

// ForceUpdate forces an immediate update of yt-dlp
func (m *YtdlpManager) ForceUpdate() error {
	m.mu.Lock()
	defer m.mu.Unlock()

	log.Println("Forcing yt-dlp update...")
	return m.updateYtdlp()
}

// downloadYtdlp downloads the latest yt-dlp binary
func (m *YtdlpManager) downloadYtdlp() error {
	url := m.getBinaryURL()

	resp, err := http.Get(url)
	if err != nil {
		return fmt.Errorf("failed to download: %w", err)
	}
	defer resp.Body.Close()

	if resp.StatusCode != http.StatusOK {
		return fmt.Errorf("download failed with status: %d", resp.StatusCode)
	}

	// Create temporary file
	tmpFile := m.binPath + ".tmp"
	out, err := os.OpenFile(tmpFile, os.O_CREATE|os.O_WRONLY|os.O_TRUNC, 0755)
	if err != nil {
		return fmt.Errorf("failed to create file: %w", err)
	}

	_, err = io.Copy(out, resp.Body)
	out.Close()
	if err != nil {
		os.Remove(tmpFile)
		return fmt.Errorf("failed to write file: %w", err)
	}

	// Move temp file to final location
	if m.fileExists(m.binPath) {
		os.Remove(m.binPath)
	}
	if err := os.Rename(tmpFile, m.binPath); err != nil {
		return fmt.Errorf("failed to move file: %w", err)
	}

	m.markUpdated()
	log.Println("yt-dlp downloaded successfully")
	return nil
}

// updateYtdlp updates yt-dlp using self-update or download
func (m *YtdlpManager) updateYtdlp() error {
	// Try self-update first
	cmd := exec.Command(m.binPath, "-U")
	if err := cmd.Run(); err == nil {
		m.markUpdated()
		log.Println("yt-dlp updated via self-update")
		return nil
	}

	// Fallback to download
	log.Println("Self-update failed, downloading fresh binary...")
	return m.downloadYtdlp()
}

// shouldUpdate checks if update check is due
func (m *YtdlpManager) shouldUpdate() bool {
	checkFile := filepath.Join(m.binDir, updateCheckFile)
	info, err := os.Stat(checkFile)
	if err != nil {
		return true // No check file, should update
	}

	return time.Since(info.ModTime()) > 7*24*time.Hour
}

// markUpdated marks the current time as last update check
func (m *YtdlpManager) markUpdated() {
	checkFile := filepath.Join(m.binDir, updateCheckFile)
	if err := os.WriteFile(checkFile, []byte(time.Now().Format(time.RFC3339)), 0644); err != nil {
		log.Printf("Failed to write ytdlp update check file: %v", err)
	}
	m.lastUpdate = time.Now()
}

// getBinaryURL returns the download URL based on OS and architecture
func (m *YtdlpManager) getBinaryURL() string {
	switch runtime.GOOS {
	case "windows":
		return "https://github.com/yt-dlp/yt-dlp/releases/latest/download/yt-dlp.exe"
	case "darwin":
		return "https://github.com/yt-dlp/yt-dlp/releases/latest/download/yt-dlp_macos"
	case "linux":
		// ARM64 requires Python-based version
		if runtime.GOARCH == "arm64" || runtime.GOARCH == "arm" {
			return "https://github.com/yt-dlp/yt-dlp/releases/latest/download/yt-dlp"
		}
		// AMD64/x86_64 uses standalone binary (no Python needed)
		return "https://github.com/yt-dlp/yt-dlp/releases/latest/download/yt-dlp_linux"
	default:
		// Fallback to generic Python-based version
		return "https://github.com/yt-dlp/yt-dlp/releases/latest/download/yt-dlp"
	}
}

// fileExists checks if a file exists
func (m *YtdlpManager) fileExists(path string) bool {
	_, err := os.Stat(path)
	return err == nil
}
