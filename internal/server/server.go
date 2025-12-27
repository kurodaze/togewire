package server

import (
	"context"
	"crypto/rand"
	"crypto/subtle"
	"encoding/base64"
	"fmt"
	"html/template"
	"io/fs"

	"github.com/kurodaze/togewire/internal/logger"

	"net/http"
	"sync"
	"time"

	"github.com/kurodaze/togewire/internal/config"
	"github.com/kurodaze/togewire/internal/spotify"
	"github.com/kurodaze/togewire/internal/types"
	"github.com/kurodaze/togewire/internal/youtube"

	"github.com/gin-gonic/gin"
	"github.com/gorilla/websocket"
)

const (
	spotifyOAuthStateCookieName = "togewire_spotify_oauth_state"
	spotifyOAuthStateTTL        = 10 * time.Minute
)

const (
	// WebSocket constants
	writeWait      = 10 * time.Second
	pongWait       = 60 * time.Second
	pingPeriod     = (pongWait * 9) / 10
	maxMessageSize = 512

	// Audio streaming constants
	chunkSize = 32 * 1024 // 32KB chunks
)

type Server struct {
	router        *gin.Engine
	upgrader      websocket.Upgrader
	spotifyClient *spotify.Client
	youtubeClient *youtube.Client

	// Application state with proper concurrency control
	stateMu      sync.RWMutex
	currentTrack *types.SpotifyState
	nextTrack    *types.PreparedTrack
	listenAlong  *ListenAlongState

	// WebSocket client management
	clients   map[string]*Client
	clientsMu sync.RWMutex
	broadcast chan types.WebSocketMessage

	// Audio broadcast coordination
	audioBroadcasts   map[string]*audioBroadcast
	audioBroadcastsMu sync.Mutex

	// Context for graceful shutdown
	ctx    context.Context
	cancel context.CancelFunc
}

type audioBroadcast struct {
	trackID  string
	filePath string
	clients  map[string]*Client
	mu       sync.Mutex
	started  bool
	chunks   []types.WebSocketMessage
	complete bool
	err      error
}

type ListenAlongState struct {
	AudioURL  string    `json:"audio_url"`
	Progress  int64     `json:"progress"`
	IsPlaying bool      `json:"is_playing"`
	LastSync  time.Time `json:"last_sync"`
	Error     string    `json:"error,omitempty"` // Error message for failed tracks
}

type Client struct {
	id          string
	name        string
	conn        *websocket.Conn
	send        chan types.WebSocketMessage
	server      *Server
	isListening bool
	listeningMu sync.Mutex
}

// New creates a new server instance
func New() *Server {
	ctx, cancel := context.WithCancel(context.Background())

	server := &Server{
		upgrader: websocket.Upgrader{
			CheckOrigin: func(r *http.Request) bool {
				// Allow all origins for iframe embedding
				return true
			},
		},
		spotifyClient:   spotify.New(),
		youtubeClient:   youtube.New(),
		clients:         make(map[string]*Client),
		broadcast:       make(chan types.WebSocketMessage, 256),
		audioBroadcasts: make(map[string]*audioBroadcast),
		listenAlong: &ListenAlongState{
			LastSync: time.Now(),
		},
		ctx:    ctx,
		cancel: cancel,
	}

	server.setupRouter()
	server.startBackgroundWorkers()

	return server
}

// setupRouter configures the HTTP routes
func (s *Server) setupRouter() {
	// Set Gin mode based on config
	cfg := config.Get()
	if cfg.LogLevel != "debug" {
		gin.SetMode(gin.ReleaseMode)
	}

	s.router = gin.Default()

	// Security middleware
	s.router.Use(s.securityHeaders())

	// Load templates from embedded files or filesystem
	if embedded, ok := getEmbeddedFiles(); ok {
		// Use embedded files
		templ := template.Must(template.New("").ParseFS(embedded, "web/templates/*"))
		s.router.SetHTMLTemplate(templ)

		// Serve static files from embedded FS
		staticFS, _ := fs.Sub(embedded, "web/static")
		s.router.StaticFS("/static", http.FS(staticFS))
	} else {
		// Fallback to filesystem (for development)
		s.router.Static("/static", "./web/static")
		s.router.LoadHTMLGlob("web/templates/*")
	}

	// Main routes (protected)
	s.router.GET("/", requireAuth(), s.handleIndex)

	// WebSocket endpoint
	s.router.GET("/ws", s.handleWebSocket)

	// Player endpoints (public)
	s.router.GET("/player", s.servePlayer)

	// Health check
	s.router.GET("/ping", s.handlePing)

	// Admin routes with authentication
	admin := s.router.Group("/admin")
	{
		// Public login routes
		admin.GET("/login", s.handleAdminLogin)
		admin.POST("/login", s.handleAdminLoginPost)
		admin.GET("/logout", s.handleAdminLogout)

		// Protected routes
		protected := admin.Group("/", requireAuth())
		{
			protected.GET("/setup", s.handleAdminSetup)
			protected.POST("/setup", s.handleAdminSetupPost)

			// Cache management
			protected.GET("/cache/stats", s.handleCacheStats)
			protected.POST("/cache/clear", s.handleCacheClear)
			protected.POST("/cache/config", s.handleCacheConfig)
		}
	}

	// Spotify OAuth (protected)
	s.router.GET("/auth/spotify", requireAuth(), s.handleSpotifyAuth)
	s.router.GET("/callback", s.handleSpotifyCallback)
}

// startBackgroundWorkers starts the background goroutines
func (s *Server) startBackgroundWorkers() {
	// WebSocket broadcast worker
	go s.broadcastWorker()

	// handles track changes and queue monitoring
	go s.spotifyMonitorWorker()

	// tries to upgrade low-quality tracks when idle
	go s.idleOptimizationWorker()

	logger.Debug("Background workers started")
}

// Run starts the HTTP server
func (s *Server) Run() error {
	cfg := config.Get()
	addr := fmt.Sprintf("%s:%d", cfg.Host, cfg.Port)

	logger.Info("Server starting on http://%s", addr)

	return s.router.Run(addr)
}

// Shutdown gracefully shuts down the server
func (s *Server) Shutdown() {
	s.cancel()

	// Close all WebSocket connections
	s.clientsMu.Lock()
	for _, client := range s.clients {
		close(client.send)
		client.conn.Close()
	}
	s.clientsMu.Unlock()

	logger.Info("Server shutdown complete")
}

// securityHeaders middleware adds security headers
func (s *Server) securityHeaders() gin.HandlerFunc {
	return gin.HandlerFunc(func(c *gin.Context) {
		c.Header("X-Content-Type-Options", "nosniff")
		c.Header("X-XSS-Protection", "1; mode=block")

		// iframe embedding for player routes
		if c.FullPath() == "/player" {
			c.Header("X-Frame-Options", "ALLOWALL")
			c.Header("Content-Security-Policy", "frame-ancestors *")
		} else if c.FullPath() == "/ping" {
			// CORS for ping endpoint
			c.Header("Access-Control-Allow-Origin", "*")
			c.Header("Access-Control-Allow-Methods", "GET, HEAD, OPTIONS")
			c.Header("Access-Control-Allow-Headers", "Content-Type")
		} else {
			c.Header("X-Frame-Options", "SAMEORIGIN")
		}

		c.Next()
	})
}

// handleWebSocket handles WebSocket connections
func (s *Server) handleWebSocket(c *gin.Context) {
	conn, err := s.upgrader.Upgrade(c.Writer, c.Request, nil)
	if err != nil {
		logger.Info("WebSocket upgrade failed: %v", err)
		return
	}

	clientIP := c.ClientIP()
	clientName := getGeoTag(clientIP)

	// Use unique ID to handle multiple connections from same IP
	clientID := fmt.Sprintf("%s-%d", clientIP, time.Now().UnixNano())

	client := &Client{
		id:     clientID,
		name:   clientName,
		conn:   conn,
		send:   make(chan types.WebSocketMessage, 256),
		server: s,
	}

	// Register client
	s.clientsMu.Lock()
	s.clients[client.id] = client
	s.clientsMu.Unlock()

	// Start client goroutines FIRST so they're ready to receive messages
	go client.writePump()
	go client.readPump()

	// Send current state immediately (after goroutines start)
	s.sendCurrentState(client)
}

// servePlayer serves the music player
func (s *Server) servePlayer(c *gin.Context) {
	c.HTML(http.StatusOK, "player.html", gin.H{})
}

// handlePing handles health check requests
func (s *Server) handlePing(c *gin.Context) {
	// Handle OPTIONS request for CORS preflight
	if c.Request.Method == "OPTIONS" {
		c.Header("Access-Control-Max-Age", "86400")
		c.Status(http.StatusOK)
		return
	}

	isPlaying := false
	var track *types.Track

	s.stateMu.RLock()
	if s.currentTrack != nil && s.currentTrack.Track != nil {
		trackCopy := *s.currentTrack.Track
		track = &trackCopy
		isPlaying = s.currentTrack.IsPlaying
	}
	s.stateMu.RUnlock()

	c.JSON(http.StatusOK, gin.H{
		"status":        "ok",
		"timestamp":     time.Now().Unix(),
		"is_playing":    isPlaying,
		"has_track":     track != nil,
		"current_track": track,
	})
}

// sendCurrentState sends the current application state to a client
func (s *Server) sendCurrentState(client *Client) {
	s.stateMu.RLock()
	defer s.stateMu.RUnlock()

	if s.currentTrack != nil && s.currentTrack.Track != nil {
		// Calculate listener count (excluding the recipient)
		listenerCount := 0
		s.clientsMu.RLock()
		for _, c := range s.clients {
			c.listeningMu.Lock()
			if c.isListening && c != client {
				listenerCount++
			}
			c.listeningMu.Unlock()
		}
		s.clientsMu.RUnlock()

		msg := types.WebSocketMessage{
			Type: "track_update",
			Data: types.TrackUpdateMessage{
				Track:         s.currentTrack.Track,
				Progress:      s.currentTrack.Progress,
				IsPlaying:     s.currentTrack.IsPlaying,
				Duration:      s.currentTrack.Track.Duration,
				AudioReady:    s.listenAlong.AudioURL != "",
				ListenerCount: listenerCount,
				Error:         s.listenAlong.Error,
			},
		}

		select {
		case client.send <- msg:
		default:
			close(client.send)
		}
	}
}

// broadcastWorker handles broadcasting messages to all clients
func (s *Server) broadcastWorker() {
	for {
		select {
		case message := <-s.broadcast:
			s.clientsMu.RLock()

			// Calculate total listener count once
			totalListeners := 0
			for _, c := range s.clients {
				c.listeningMu.Lock()
				if c.isListening {
					totalListeners++
				}
				c.listeningMu.Unlock()
			}

			// Send customized message to each client (excluding themselves from count)
			for _, client := range s.clients {
				// Create a new message for each client with customized listener count
				customMsg := types.WebSocketMessage{
					Type: message.Type,
					Data: message.Data,
				}

				if message.Type == "track_update" {
					if trackUpdate, ok := message.Data.(types.TrackUpdateMessage); ok {
						client.listeningMu.Lock()
						isListening := client.isListening
						client.listeningMu.Unlock()

						// Exclude self from count if listening
						if isListening {
							trackUpdate.ListenerCount = totalListeners - 1
						} else {
							trackUpdate.ListenerCount = totalListeners
						}
						customMsg.Data = trackUpdate
					}
				}

				select {
				case client.send <- customMsg:
				default:
					close(client.send)
				}
			}
			s.clientsMu.RUnlock()

		case <-s.ctx.Done():
			return
		}
	}
}

func (s *Server) handleIndex(c *gin.Context) {
	cfg := config.Get()

	// Check if Spotify is configured and authenticated
	authenticated := cfg.IsAuthenticated()

	scheme := "http"
	if c.Request.TLS != nil || c.GetHeader("X-Forwarded-Proto") == "https" {
		scheme = "https"
	}

	data := gin.H{
		"Authenticated": authenticated,
		"Host":          c.Request.Host,
		"Scheme":        scheme,
	}

	if authenticated {
		data["CacheMaxSizeMB"] = cfg.CacheMaxSizeMB
	}

	// Check for error parameter
	if errorMsg := c.Query("error"); errorMsg != "" {
		data["Error"] = errorMsg
	}

	c.HTML(http.StatusOK, "dashboard.html", data)
}

// Admin handlers
func (s *Server) handleAdminLogin(c *gin.Context) {
	cfg := config.Get()

	c.HTML(http.StatusOK, "login.html", gin.H{
		"Error":   c.Query("error"),
		"IsSetup": !cfg.HasAdminPassword(),
	})
}

func (s *Server) handleAdminLoginPost(c *gin.Context) {
	cfg := config.Get()
	password := c.PostForm("password")

	// Rate limiting check
	if !checkRateLimit(c.ClientIP()) {
		c.Redirect(http.StatusFound, "/admin/login?error=Too many attempts. Try again later")
		return
	}

	// If no password set yet, set it
	if !cfg.HasAdminPassword() {
		// Validate setup token
		token := c.PostForm("setup_token")
		if !ValidateSetupToken(token) {
			c.Redirect(http.StatusFound, "/admin/login?error=Invalid setup token. Check terminal output")
			return
		}

		if password == "" {
			c.Redirect(http.StatusFound, "/admin/login?error=Password required")
			return
		}

		hash, err := hashPassword(password)
		if err != nil {
			c.Redirect(http.StatusFound, "/admin/login?error=Failed to set password")
			return
		}

		if err := cfg.UpdateAdminPassword(hash); err != nil {
			c.Redirect(http.StatusFound, "/admin/login?error=Failed to save password")
			return
		}

		// Create session and login
		sessionToken, err := createSession()
		if err != nil {
			c.Redirect(http.StatusFound, "/admin/login?error=Failed to create session")
			return
		}

		setSecureCookie(c, sessionToken)
		c.Redirect(http.StatusFound, "/")
		return
	}

	// Verify password
	if !verifyPassword(cfg.AdminPasswordHash, password) {
		recordFailedAttempt(c.ClientIP())
		c.Redirect(http.StatusFound, "/admin/login?error=Invalid password")
		return
	}

	// Create session
	sessionToken, err := createSession()
	if err != nil {
		c.Redirect(http.StatusFound, "/admin/login?error=Failed to create session")
		return
	}

	clearFailedAttempts(c.ClientIP())
	setSecureCookie(c, sessionToken)
	c.Redirect(http.StatusFound, "/")
}

func (s *Server) handleAdminLogout(c *gin.Context) {
	sessionToken, _ := c.Cookie(sessionCookieName)
	if sessionToken != "" {
		destroySession(sessionToken)
	}
	c.SetCookie(sessionCookieName, "", -1, "/", "", true, true)
	c.Redirect(http.StatusFound, "/admin/login")
}

func (s *Server) handleAdminSetup(c *gin.Context) {
	c.Redirect(http.StatusFound, "/")
}

func (s *Server) handleAdminSetupPost(c *gin.Context) {
	clientID := c.PostForm("client_id")
	clientSecret := c.PostForm("client_secret")

	if clientID == "" || clientSecret == "" {
		c.Redirect(http.StatusFound, "/?error=Please provide both Client ID and Client Secret")
		return
	}

	// Validate credentials by attempting a basic request
	if !s.spotifyClient.ValidateCredentials(clientID, clientSecret) {
		c.Redirect(http.StatusFound, "/?error=Invalid Spotify credentials")
		return
	}

	// Save credentials to config
	cfg := config.Get()
	scheme := "http"
	if c.Request.TLS != nil || c.GetHeader("X-Forwarded-Proto") == "https" {
		scheme = "https"
	}
	redirectURI := fmt.Sprintf("%s://%s/callback", scheme, c.Request.Host)

	if err := cfg.UpdateSpotifyCredentials(clientID, clientSecret, redirectURI); err != nil {
		logger.Error("Failed to save Spotify credentials: %v", err)
		c.Redirect(http.StatusFound, "/?error=Failed to save credentials")
		return
	}

	// Update the Spotify client with new credentials
	s.spotifyClient.UpdateCredentials(clientID, clientSecret, redirectURI)

	logger.Info("Spotify credentials configured")

	// Redirect to Spotify OAuth
	c.Redirect(http.StatusFound, "/auth/spotify")
}

// Spotify OAuth handlers
func (s *Server) handleSpotifyAuth(c *gin.Context) {
	stateBytes := make([]byte, 32)
	if _, err := rand.Read(stateBytes); err != nil {
		logger.Error("Failed to generate OAuth state: %v", err)
		c.Redirect(http.StatusFound, "/?error=Failed to start Spotify authentication")
		return
	}
	state := base64.RawURLEncoding.EncodeToString(stateBytes)

	isSecure := c.Request.TLS != nil || c.GetHeader("X-Forwarded-Proto") == "https"
	c.SetCookie(spotifyOAuthStateCookieName, state, int(spotifyOAuthStateTTL.Seconds()), "/", "", isSecure, true)
	c.Header("Set-Cookie", c.Writer.Header().Get("Set-Cookie")+"; SameSite=Lax")

	url := s.spotifyClient.GetAuthURL(state)
	c.Redirect(302, url)
}

func (s *Server) handleSpotifyCallback(c *gin.Context) {
	code := c.Query("code")
	state := c.Query("state")
	if code == "" {
		c.Redirect(http.StatusFound, "/?error=No authorization code received from Spotify")
		return
	}
	if state == "" {
		c.Redirect(http.StatusFound, "/?error=Missing state from Spotify callback")
		return
	}

	expectedState, err := c.Cookie(spotifyOAuthStateCookieName)
	if err != nil || expectedState == "" {
		c.Redirect(http.StatusFound, "/?error=Spotify authentication expired, please try again")
		return
	}
	if subtle.ConstantTimeCompare([]byte(state), []byte(expectedState)) != 1 {
		c.Redirect(http.StatusFound, "/?error=Invalid Spotify authentication response")
		return
	}

	isSecure := c.Request.TLS != nil || c.GetHeader("X-Forwarded-Proto") == "https"
	c.SetCookie(spotifyOAuthStateCookieName, "", -1, "/", "", isSecure, true)
	c.Header("Set-Cookie", c.Writer.Header().Get("Set-Cookie")+"; SameSite=Lax")

	if err := s.spotifyClient.ExchangeCode(code); err != nil {
		logger.Error("Failed to exchange authorization code: %v", err)
		c.Redirect(http.StatusFound, "/?error=Failed to complete Spotify authentication")
		return
	}

	logger.Info("Authenticated with Spotify")

	// Redirect to the main dashboard
	c.Redirect(http.StatusFound, "/")
}

// getGeoTag returns a simple geographic identifier for an IP address
func getGeoTag(ip string) string {
	return ip
}

// Cache management handlers
func (s *Server) handleCacheStats(c *gin.Context) {
	stats := s.youtubeClient.GetCacheStats()

	c.JSON(http.StatusOK, gin.H{
		"tracks":  stats.Tracks,
		"files":   stats.Files,
		"size_mb": stats.SizeMB,
	})
}

func (s *Server) handleCacheClear(c *gin.Context) {
	if err := s.youtubeClient.ClearCache(); err != nil {
		c.JSON(http.StatusInternalServerError, gin.H{
			"success": false,
			"error":   err.Error(),
		})
		return
	}

	c.JSON(http.StatusOK, gin.H{
		"success": true,
	})
}

func (s *Server) handleCacheConfig(c *gin.Context) {
	maxSizeMB := c.PostForm("max_size_mb")

	if maxSizeMB == "" {
		c.Redirect(http.StatusFound, "/?error=Max size is required")
		return
	}

	size := int64(0)
	if _, err := fmt.Sscanf(maxSizeMB, "%d", &size); err != nil {
		c.Redirect(http.StatusFound, "/?error=Invalid number")
		return
	}

	// Validate range: 0 (unlimited) or 100MB to 100GB
	if size < 0 || (size > 0 && size < 100) || size > 102400 {
		c.Redirect(http.StatusFound, "/?error=Size must be 0 (unlimited) or 100-102400 MB")
		return
	}

	cfg := config.Get()
	cfg.CacheMaxSizeMB = size

	if err := cfg.Save(); err != nil {
		c.Redirect(http.StatusFound, "/?error=Failed to save cache config")
		return
	}

	c.Redirect(http.StatusFound, "/")
}
