package spotify

import (
	"context"
	"crypto/rand"
	"encoding/base64"
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"strings"
	"sync"
	"time"

	"github.com/kurodaze/togewire/internal/config"
	"github.com/kurodaze/togewire/internal/types"

	"golang.org/x/oauth2"
)

const (
	authURL  = "https://accounts.spotify.com/authorize"
	tokenURL = "https://accounts.spotify.com/api/token"
	apiURL   = "https://api.spotify.com/v1"
)

type Client struct {
	oauthConfig *oauth2.Config
	httpClient  *http.Client
	token       *oauth2.Token
	mu          sync.RWMutex
}

type spotifyTrack struct {
	ID    string `json:"id"`
	Name  string `json:"name"`
	Album struct {
		Name string `json:"name"`
	} `json:"album"`
	Artists []struct {
		Name string `json:"name"`
	} `json:"artists"`
	DurationMS int64 `json:"duration_ms"`
}

type spotifyCurrentlyPlaying struct {
	IsPlaying bool          `json:"is_playing"`
	Progress  int64         `json:"progress_ms"`
	Item      *spotifyTrack `json:"item"`
}

type spotifyQueue struct {
	Queue []spotifyTrack `json:"queue"`
}

// New creates a new Spotify client
func New() *Client {
	cfg := config.Get()

	oauthConfig := &oauth2.Config{
		ClientID:     cfg.SpotifyClientID,
		ClientSecret: cfg.SpotifyClientSecret,
		RedirectURL:  cfg.SpotifyRedirectURI,
		Scopes: []string{
			"user-read-currently-playing",
			"user-read-playback-state",
			"user-modify-playback-state",
		},
		Endpoint: oauth2.Endpoint{
			AuthURL:  authURL,
			TokenURL: tokenURL,
		},
	}

	client := &Client{
		oauthConfig: oauthConfig,
		httpClient:  &http.Client{Timeout: 10 * time.Second},
	}

	// Load existing token if available
	if cfg.SpotifyAccessToken != "" && cfg.SpotifyRefreshToken != "" {
		client.token = &oauth2.Token{
			AccessToken:  cfg.SpotifyAccessToken,
			RefreshToken: cfg.SpotifyRefreshToken,
			TokenType:    "Bearer",
			Expiry:       time.Now().Add(-1 * time.Hour), // Force refresh on first use
		}
	}

	return client
}

func generateState() (string, error) {
	b := make([]byte, 32)
	if _, err := rand.Read(b); err != nil {
		return "", err
	}
	return base64.URLEncoding.EncodeToString(b), nil
}

// UpdateCredentials updates the OAuth configuration with new credentials
func (c *Client) UpdateCredentials(clientID, clientSecret, redirectURI string) {
	c.mu.Lock()
	defer c.mu.Unlock()

	c.oauthConfig.ClientID = clientID
	c.oauthConfig.ClientSecret = clientSecret
	c.oauthConfig.RedirectURL = redirectURI
}

// GetAuthURL returns the Spotify OAuth authorization URL
func (c *Client) GetAuthURL() string {
	state, err := generateState()
	if err != nil {
		// In the unlikely event of a failure to generate state, fall back to empty state.
		state = ""
	}
	return c.oauthConfig.AuthCodeURL(state, oauth2.AccessTypeOffline)
}

// ExchangeCode exchanges authorization code for tokens
func (c *Client) ExchangeCode(code string) error {
	ctx := context.Background()
	token, err := c.oauthConfig.Exchange(ctx, code)
	if err != nil {
		return fmt.Errorf("failed to exchange code: %w", err)
	}

	c.mu.Lock()
	c.token = token
	c.mu.Unlock()

	// Save tokens to config
	cfg := config.Get()
	return cfg.UpdateSpotifyTokens(token.AccessToken, token.RefreshToken)
}

// ValidateCredentials checks if the provided credentials are valid
func (c *Client) ValidateCredentials(clientID, clientSecret string) bool {
	testConfig := &oauth2.Config{
		ClientID:     clientID,
		ClientSecret: clientSecret,
		Endpoint: oauth2.Endpoint{
			TokenURL: tokenURL,
		},
	}

	// Try to get client credentials token (basic validation)
	ctx := context.Background()
	_, err := testConfig.PasswordCredentialsToken(ctx, "", "")

	// an error is expected, but not an auth error
	return err != nil && !strings.Contains(err.Error(), "invalid_client")
}

// ensureValidToken checks if token is valid and refreshes if needed
// Returns error if not authenticated or refresh fails
func (c *Client) ensureValidToken() error {
	// Check if token needs refresh (with read lock)
	c.mu.RLock()
	if c.token == nil {
		c.mu.RUnlock()
		return fmt.Errorf("not authenticated")
	}
	needsRefresh := c.token.Expiry.Before(time.Now())
	c.mu.RUnlock()

	// Refresh token if needed (with write lock)
	if needsRefresh {
		c.mu.Lock()
		if err := c.refreshToken(); err != nil {
			c.mu.Unlock()
			return fmt.Errorf("failed to refresh token: %w", err)
		}
		c.mu.Unlock()
	}
	return nil
}

// GetCurrentTrack gets the currently playing track
func (c *Client) GetCurrentTrack() (*types.SpotifyState, error) {
	if err := c.ensureValidToken(); err != nil {
		return nil, err
	}

	// Now make the API request (with read lock)
	c.mu.RLock()
	defer c.mu.RUnlock()

	resp, err := c.makeAPIRequest("GET", "/me/player/currently-playing", nil)
	if err != nil {
		return nil, err
	}
	defer resp.Body.Close()

	if resp.StatusCode == 204 {
		// No content - nothing playing
		return &types.SpotifyState{}, nil
	}

	if resp.StatusCode != 200 {
		return nil, fmt.Errorf("unexpected status code: %d", resp.StatusCode)
	}

	var current spotifyCurrentlyPlaying
	if err := json.NewDecoder(resp.Body).Decode(&current); err != nil {
		return nil, fmt.Errorf("failed to decode response: %w", err)
	}

	state := &types.SpotifyState{
		Progress:  current.Progress,
		IsPlaying: current.IsPlaying,
	}

	if current.Item != nil {
		artist := "Unknown Artist"
		if len(current.Item.Artists) > 0 {
			artist = current.Item.Artists[0].Name
		}

		state.Track = &types.Track{
			ID:       current.Item.ID,
			Name:     current.Item.Name,
			Artist:   artist,
			Duration: current.Item.DurationMS,
		}
	}

	return state, nil
}

// GetQueue gets the user's queue
func (c *Client) GetQueue() ([]types.Track, error) {
	if err := c.ensureValidToken(); err != nil {
		return nil, err
	}

	// Now make the API request (with read lock)
	c.mu.RLock()
	defer c.mu.RUnlock()

	resp, err := c.makeAPIRequest("GET", "/me/player/queue", nil)
	if err != nil {
		return nil, err
	}
	defer resp.Body.Close()

	if resp.StatusCode != 200 {
		return nil, fmt.Errorf("unexpected status code: %d", resp.StatusCode)
	}

	var queue spotifyQueue
	if err := json.NewDecoder(resp.Body).Decode(&queue); err != nil {
		return nil, fmt.Errorf("failed to decode response: %w", err)
	}

	tracks := make([]types.Track, len(queue.Queue))
	for i, track := range queue.Queue {
		artist := "Unknown Artist"
		if len(track.Artists) > 0 {
			artist = track.Artists[0].Name
		}

		tracks[i] = types.Track{
			ID:       track.ID,
			Name:     track.Name,
			Artist:   artist,
			Duration: track.DurationMS,
		}
	}

	return tracks, nil
}

// GetLatencyCompensatedProgress gets current progress with network latency compensation
func (c *Client) GetLatencyCompensatedProgress() (*types.SpotifyState, error) {
	start := time.Now()
	state, err := c.GetCurrentTrack()
	if err != nil {
		return nil, err
	}

	if state.Track != nil && state.IsPlaying {
		// Compensate for network latency
		latency := time.Since(start)
		state.Progress += latency.Milliseconds()

		// Clamp to track duration
		if state.Progress > state.Track.Duration {
			state.Progress = state.Track.Duration
		}
	}

	return state, nil
}

// GetRecommendedPollInterval returns recommended polling interval based on playback state
func (c *Client) GetRecommendedPollInterval(isPlaying bool) time.Duration {
	if isPlaying {
		return 1 * time.Second // More frequent when playing
	}
	return 3 * time.Second // Less frequent when paused
}

// refreshToken refreshes the OAuth token
func (c *Client) refreshToken() error {
	ctx := context.Background()
	tokenSource := c.oauthConfig.TokenSource(ctx, c.token)
	newToken, err := tokenSource.Token()
	if err != nil {
		return err
	}

	c.token = newToken

	// Save to config
	cfg := config.Get()
	return cfg.UpdateSpotifyTokens(newToken.AccessToken, newToken.RefreshToken)
}

// makeAPIRequest makes an authenticated API request to Spotify
func (c *Client) makeAPIRequest(method, endpoint string, body io.Reader) (*http.Response, error) {
	req, err := http.NewRequest(method, apiURL+endpoint, body)
	if err != nil {
		return nil, err
	}

	req.Header.Set("Authorization", "Bearer "+c.token.AccessToken)
	if body != nil {
		req.Header.Set("Content-Type", "application/json")
	}

	return c.httpClient.Do(req)
}
