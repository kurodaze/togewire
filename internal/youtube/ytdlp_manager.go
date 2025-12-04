package youtube

import (
	"bytes"
	"fmt"
	"io"
	"net/http"
	"os"
	"os/exec"
	"path/filepath"
	"runtime"
	"sync"

	"github.com/kurodaze/togewire/internal/logger"
)

const (
	ytdlpBaseURL = "https://github.com/yt-dlp/yt-dlp/releases/latest/download/"
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

func resolveYtdlpBinary() (string, string) {
	name := map[string]string{
		"windows/amd64": "yt-dlp.exe", "windows/arm64": "yt-dlp_arm64.exe",
		"darwin/amd64": "yt-dlp_macos", "darwin/arm64": "yt-dlp_macos",
		"linux/amd64": "yt-dlp_linux", "linux/arm64": "yt-dlp_linux_aarch64", "linux/arm": "yt-dlp_linux_armv7l",
	}[runtime.GOOS+"/"+runtime.GOARCH]

	if runtime.GOOS == "linux" && isMuslLibc() {
		switch runtime.GOARCH {
		case "amd64":
			name = "yt-dlp_musllinux"
		case "arm64":
			name = "yt-dlp_musllinux_aarch64"
		}
	}
	if name == "" {
		name = "yt-dlp"
	}
	return name, ytdlpBaseURL + name
}

const failureThreshold = 3

// GetYtdlpManager returns the singleton yt-dlp manager
func GetYtdlpManager() *YtdlpManager {
	ytdlpManagerOnce.Do(func() {
		binDir := filepath.Join(".", "bin")
		_ = os.MkdirAll(binDir, 0755)

		ytdlpManager = &YtdlpManager{
			binDir: binDir,
		}

		if err := ytdlpManager.ensureYtdlp(); err != nil {
			logger.Error("Failed to initialize yt-dlp: %v", err)
		}
	})
	return ytdlpManager
}

func (m *YtdlpManager) ensureYtdlp() error {
	m.mu.Lock()
	defer m.mu.Unlock()

	logger.Debug("Downloading latest yt-dlp...")
	if err := m.downloadYtdlp(); err != nil {
		logger.Warn("Failed to download yt-dlp, trying system: %v", err)
		return m.useSystemYtdlp()
	}

	return nil
}

func (m *YtdlpManager) useSystemYtdlp() error {
	systemPath, err := exec.LookPath("yt-dlp")
	if err != nil {
		return fmt.Errorf("yt-dlp not found in PATH: %w", err)
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

// HandleFailure tracks failures and re-downloads yt-dlp after threshold
func (m *YtdlpManager) HandleFailure(err error) error {
	m.mu.Lock()
	defer m.mu.Unlock()

	m.failureCount++
	if m.failureCount < failureThreshold {
		return err
	}

	logger.Info("Multiple yt-dlp failures, downloading fresh...")
	if dlErr := m.downloadYtdlp(); dlErr != nil {
		return fmt.Errorf("original error: %v, download failed: %v", err, dlErr)
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

func (m *YtdlpManager) downloadYtdlp() error {
	binaryName, url := resolveYtdlpBinary()
	m.binPath = filepath.Join(m.binDir, binaryName)

	// Remove existing binary
	os.Remove(m.binPath)

	resp, err := http.Get(url)
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

	_, copyErr := io.Copy(out, resp.Body)
	out.Close()
	if copyErr != nil {
		os.Remove(tmpFile)
		return fmt.Errorf("failed to write file: %w", copyErr)
	}

	if err := os.Rename(tmpFile, m.binPath); err != nil {
		os.Remove(tmpFile)
		return fmt.Errorf("failed to move file: %w", err)
	}

	logger.Debug("yt-dlp downloaded successfully")
	return nil
}

func isMuslLibc() bool {
	if runtime.GOOS != "linux" {
		return false
	}
	out, _ := exec.Command("ldd", "--version").CombinedOutput()
	return bytes.Contains(out, []byte("musl"))
}
