package config

import (
	"crypto/rand"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"os"
	"sync"

	"github.com/joho/godotenv"
)

type Config struct {
	// Server settings
	Port     int    `json:"port"`
	Host     string `json:"host"`
	LogLevel string `json:"log_level"`

	// Spotify OAuth
	SpotifyClientID     string `json:"spotify_client_id,omitempty"`
	SpotifyClientSecret string `json:"spotify_client_secret,omitempty"`
	SpotifyRedirectURI  string `json:"spotify_redirect_uri,omitempty"`
	SpotifyAccessToken  string `json:"spotify_access_token,omitempty"`
	SpotifyRefreshToken string `json:"spotify_refresh_token,omitempty"`

	// Admin authentication
	AdminPasswordHash string `json:"admin_password_hash,omitempty"`
	SessionSecret     string `json:"session_secret"`

	// Cache settings
	CacheMaxSizeMB int64 `json:"cache_max_size_mb"`

	mu sync.RWMutex
}

var (
	instance *Config
	once     sync.Once
)

// Get returns the singleton config instance
func Get() *Config {
	once.Do(func() {
		instance = &Config{
			Port:           7093,
			Host:           "0.0.0.0",
			LogLevel:       "info",
			CacheMaxSizeMB: 1024, // 0 = unlimited
		}
		instance.load()
	})
	return instance
}

// load reads configuration from file and environment
func (c *Config) load() {
	// Load .env file if it exists
	_ = godotenv.Load()

	// Load from configs/config.json
	if data, err := os.ReadFile("configs/config.json"); err == nil {
		_ = json.Unmarshal(data, c)
	}

	// Override with environment variables
	if port := os.Getenv("PORT"); port != "" {
		if _, err := fmt.Sscanf(port, "%d", &c.Port); err != nil {
			fmt.Fprintf(os.Stderr, "invalid PORT value %q: %v\n", port, err)
		}
	}
	if host := os.Getenv("HOST"); host != "" {
		c.Host = host
	}
	if logLevel := os.Getenv("LOG_LEVEL"); logLevel != "" {
		c.LogLevel = logLevel
	}
	if clientID := os.Getenv("SPOTIFY_CLIENT_ID"); clientID != "" {
		c.SpotifyClientID = clientID
	}
	if clientSecret := os.Getenv("SPOTIFY_CLIENT_SECRET"); clientSecret != "" {
		c.SpotifyClientSecret = clientSecret
	}

	// Generate session secret if not exists
	if c.SessionSecret == "" {
		c.SessionSecret = generateRandomSecret()
		if err := c.Save(); err != nil {
			fmt.Fprintf(os.Stderr, "failed to save config: %v\n", err)
		}
	}
}

// Save writes the configuration to config.json
func (c *Config) Save() error {
	c.mu.Lock()
	defer c.mu.Unlock()

	data, err := json.MarshalIndent(c, "", "  ")
	if err != nil {
		return err
	}

	// Ensure configs directory exists
	if err := os.MkdirAll("configs", 0755); err != nil {
		return err
	}

	if err := os.WriteFile("configs/config.json", data, 0600); err != nil {
		return err
	}

	return nil
}

// IsAuthenticated checks if Spotify is configured and authenticated
func (c *Config) IsAuthenticated() bool {
	c.mu.RLock()
	defer c.mu.RUnlock()
	return c.SpotifyAccessToken != "" && c.SpotifyRefreshToken != ""
}

// HasCredentials checks if Spotify credentials are configured
func (c *Config) HasCredentials() bool {
	c.mu.RLock()
	defer c.mu.RUnlock()
	return c.SpotifyClientID != "" && c.SpotifyClientSecret != ""
}

// HasAdminPassword checks if admin password is set
func (c *Config) HasAdminPassword() bool {
	c.mu.RLock()
	defer c.mu.RUnlock()
	return c.AdminPasswordHash != ""
}

// UpdateSpotifyCredentials updates Spotify OAuth credentials
func (c *Config) UpdateSpotifyCredentials(clientID, clientSecret, redirectURI string) error {
	c.mu.Lock()
	c.SpotifyClientID = clientID
	c.SpotifyClientSecret = clientSecret
	c.SpotifyRedirectURI = redirectURI
	c.mu.Unlock()

	return c.Save()
}

// UpdateSpotifyTokens updates Spotify OAuth tokens
func (c *Config) UpdateSpotifyTokens(accessToken, refreshToken string) error {
	c.mu.Lock()
	c.SpotifyAccessToken = accessToken
	c.SpotifyRefreshToken = refreshToken
	c.mu.Unlock()

	return c.Save()
}

// UpdateAdminPassword updates admin password hash
func (c *Config) UpdateAdminPassword(passwordHash string) error {
	c.mu.Lock()
	c.AdminPasswordHash = passwordHash
	c.mu.Unlock()

	return c.Save()
}

// generateRandomSecret generates a random hex string for sessions
func generateRandomSecret() string {
	bytes := make([]byte, 32)
	if _, err := rand.Read(bytes); err != nil {
		panic(err)
	}
	return hex.EncodeToString(bytes)
}
