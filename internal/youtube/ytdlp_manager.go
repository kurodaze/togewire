package youtube

import (
	"fmt"
	"io"
	"net/http"
	"os"
	"os/exec"
	"path/filepath"
	"runtime"
	"strings"
	"sync"
	"time"

	"github.com/kurodaze/togewire/internal/logger"
)

const (
	updateCheckFile  = ".ytdlp_last_update"
	updateInterval   = 7 * 24 * time.Hour
	failureThreshold = 3
)

var (
	ytdlpManager     *YtdlpManager
	ytdlpManagerOnce sync.Once
)

type YtdlpManager struct {
	binPath      string
	binDir       string
	mu           sync.RWMutex
	failureCount int
}

// GetYtdlpManager returns the singleton yt-dlp manager
func GetYtdlpManager() *YtdlpManager {
	ytdlpManagerOnce.Do(func() {
		binDir := filepath.Join(".", "bin")
		if err := os.MkdirAll(binDir, 0755); err != nil {
			logger.Info("Failed to create bin directory: %v", err)
		}

		ytdlpManager = &YtdlpManager{
			binDir:  binDir,
			binPath: filepath.Join(binDir, getYtdlpBinaryName()),
		}

		// Initialize yt-dlp
		if err := ytdlpManager.ensureYtdlp(); err != nil {
			logger.Info("Failed to initialize yt-dlp: %v", err)
		}
	})
	return ytdlpManager
}

func getYtdlpBinaryName() string {
	if runtime.GOOS == "windows" {
		return "yt-dlp.exe"
	}
	return "yt-dlp"
}

func fileExists(path string) bool {
	_, err := os.Stat(path)
	return err == nil
}

func (m *YtdlpManager) ensureYtdlp() error {
	m.mu.Lock()
	defer m.mu.Unlock()

	if !fileExists(m.binPath) {
		logger.Debug("yt-dlp not found, downloading...")
		if err := m.downloadYtdlp(); err != nil {
			logger.Info("Failed to download yt-dlp: %v", err)
			return m.trySystemYtdlp()
		}
	}

	if m.shouldUpdate() {
		logger.Debug("Checking for yt-dlp updates...")
		if err := m.updateYtdlp(); err != nil {
			logger.Info("Update check failed: %v", err)
		}
	}

	return nil
}

func (m *YtdlpManager) trySystemYtdlp() error {
	systemPath, err := exec.LookPath("yt-dlp")
	if err != nil {
		return fmt.Errorf("managed yt-dlp download failed and system yt-dlp not found: %w", err)
	}

	logger.Debug("Using system yt-dlp at: %s", systemPath)
	m.binPath = systemPath
	return nil
}

func (m *YtdlpManager) RunCommand(args ...string) (*exec.Cmd, error) {
	m.mu.RLock()
	defer m.mu.RUnlock()
	return exec.Command(m.binPath, args...), nil
}

func (m *YtdlpManager) HandleFailure(err error) error {
	m.mu.Lock()
	defer m.mu.Unlock()

	m.failureCount++
	if m.failureCount < failureThreshold {
		return err
	}

	logger.Info("Multiple yt-dlp failures, attempting auto-update...")
	if updateErr := m.updateYtdlp(); updateErr != nil {
		return fmt.Errorf("original error: %v, update failed: %v", err, updateErr)
	}

	m.failureCount = 0
	logger.Info("yt-dlp updated, retrying...")
	return nil
}

func (m *YtdlpManager) ResetFailures() {
	m.mu.Lock()
	defer m.mu.Unlock()
	m.failureCount = 0
}

func (m *YtdlpManager) ForceUpdate() error {
	m.mu.Lock()
	defer m.mu.Unlock()
	logger.Info("Forcing yt-dlp update...")
	return m.updateYtdlp()
}

func (m *YtdlpManager) downloadYtdlp() error {
	if runtime.GOOS != "windows" && runtime.GOOS != "linux" {
		return fmt.Errorf("platform %s not supported for managed yt-dlp", runtime.GOOS)
	}

	resp, err := http.Get(m.getBinaryURL())
	if err != nil {
		return fmt.Errorf("failed to download: %w", err)
	}
	defer resp.Body.Close()

	if resp.StatusCode != http.StatusOK {
		return fmt.Errorf("download failed with status: %d", resp.StatusCode)
	}

	tmpFile := m.binPath + ".tmp"
	out, err := os.OpenFile(tmpFile, os.O_CREATE|os.O_WRONLY|os.O_TRUNC, 0755)
	if err != nil {
		return fmt.Errorf("failed to create file: %w", err)
	}
	defer out.Close()

	if _, err := io.Copy(out, resp.Body); err != nil {
		os.Remove(tmpFile)
		return fmt.Errorf("failed to write file: %w", err)
	}

	if fileExists(m.binPath) {
		os.Remove(m.binPath)
	}

	if err := os.Rename(tmpFile, m.binPath); err != nil {
		return fmt.Errorf("failed to move file: %w", err)
	}

	m.markUpdated()
	logger.Debug("yt-dlp downloaded successfully")
	return nil
}

func (m *YtdlpManager) updateYtdlp() error {
	if m.isSystemYtdlp() {
		logger.Debug("Using system yt-dlp, skipping update")
		m.markUpdated()
		return nil
	}

	if err := exec.Command(m.binPath, "-U").Run(); err == nil {
		m.markUpdated()
		logger.Debug("yt-dlp updated via self-update")
		return nil
	}

	logger.Info("Self-update failed, downloading fresh binary...")
	return m.downloadYtdlp()
}

func (m *YtdlpManager) isSystemYtdlp() bool {
	absDir, _ := filepath.Abs(m.binDir)
	absBin, _ := filepath.Abs(m.binPath)
	return !strings.HasPrefix(absBin, absDir)
}

func (m *YtdlpManager) shouldUpdate() bool {
	info, err := os.Stat(filepath.Join(m.binDir, updateCheckFile))
	if err != nil {
		return true
	}
	return time.Since(info.ModTime()) > updateInterval
}

func (m *YtdlpManager) markUpdated() {
	checkFile := filepath.Join(m.binDir, updateCheckFile)
	if err := os.WriteFile(checkFile, []byte(time.Now().Format(time.RFC3339)), 0644); err != nil {
		logger.Info("Failed to write update check file: %v", err)
	}
}

func (m *YtdlpManager) getBinaryURL() string {
	const baseURL = "https://github.com/yt-dlp/yt-dlp/releases/latest/download/"

	switch runtime.GOOS {
	case "windows":
		return baseURL + "yt-dlp.exe"
	case "linux":
		if fileExists("/etc/alpine-release") || runtime.GOARCH == "arm64" || runtime.GOARCH == "arm" {
			return baseURL + "yt-dlp"
		}
		return baseURL + "yt-dlp_linux"
	default:
		return baseURL + "yt-dlp"
	}
}
