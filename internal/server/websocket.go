package server

import (
	"encoding/base64"
	"encoding/json"
	"fmt"
	"io"
	"log"
	"os"
	"path/filepath"
	"strings"
	"time"

	"github.com/kurodaze/togewire/internal/types"

	"github.com/gorilla/websocket"
)

// readPump pumps messages from the websocket connection to the hub
func (c *Client) readPump() {
	defer func() {
		c.server.unregisterClient(c)
		c.conn.Close()
	}()

	c.conn.SetReadLimit(maxMessageSize)
	c.conn.SetReadDeadline(time.Now().Add(pongWait))
	c.conn.SetPongHandler(func(string) error {
		c.conn.SetReadDeadline(time.Now().Add(pongWait))
		return nil
	})

	for {
		_, message, err := c.conn.ReadMessage()
		if err != nil {
			if websocket.IsUnexpectedCloseError(err, websocket.CloseGoingAway, websocket.CloseAbnormalClosure) {
				log.Printf("WebSocket error: %v", err)
			}
			break
		}

		// Validate message length
		if len(message) > maxMessageSize {
			log.Printf("WebSocket message too large: %d bytes", len(message))
			continue
		}

		// Handle incoming messages
		var msg map[string]interface{}
		if err := json.Unmarshal(message, &msg); err != nil {
			log.Printf("Failed to unmarshal WebSocket message: %v", err)
			continue
		}

		c.handleMessage(msg)
	}
}

// writePump pumps messages from the hub to the websocket connection
func (c *Client) writePump() {
	ticker := time.NewTicker(pingPeriod)
	defer func() {
		ticker.Stop()
		c.conn.Close()
	}()

	for {
		select {
		case message, ok := <-c.send:
			c.conn.SetWriteDeadline(time.Now().Add(writeWait))
			if !ok {
				c.conn.WriteMessage(websocket.CloseMessage, []byte{})
				return
			}

			if err := c.conn.WriteJSON(message); err != nil {
				log.Printf("WebSocket write error: %v", err)
				return
			}

		case <-ticker.C:
			c.conn.SetWriteDeadline(time.Now().Add(writeWait))
			if err := c.conn.WriteMessage(websocket.PingMessage, nil); err != nil {
				return
			}
		}
	}
}

// handleMessage handles incoming WebSocket messages from clients
func (c *Client) handleMessage(msg map[string]interface{}) {
	msgType, ok := msg["type"].(string)
	if !ok {
		return
	}

	switch msgType {
	case "sync_request":
		c.handleSyncRequest()
	case "request_audio_stream":
		c.handleAudioStreamRequest()
	case "stop_listening":
		c.handleStopListening()
	default:
		log.Printf("Unknown WebSocket message type: %s", msgType)
	}
}

// handleSyncRequest handles sync requests from clients
func (c *Client) handleSyncRequest() {
	// Get real-time sync data from Spotify
	state, err := c.server.spotifyClient.GetLatencyCompensatedProgress()
	if err != nil {
		c.send <- types.WebSocketMessage{
			Type: "sync_error",
			Data: map[string]string{"error": err.Error()},
		}
		return
	}

	c.server.stateMu.RLock()
	listenAlong := c.server.listenAlong
	c.server.stateMu.RUnlock()

	progress := listenAlong.Progress
	isPlaying := listenAlong.IsPlaying

	var trackID string
	if state.Track != nil {
		progress = state.Progress
		isPlaying = state.IsPlaying
		trackID = state.Track.ID
	}

	response := types.SyncResponse{
		Progress:  progress,
		IsPlaying: isPlaying,
		Timestamp: time.Now().UnixMilli(),
		TrackID:   trackID,
	}

	c.send <- types.WebSocketMessage{
		Type: "sync_response",
		Data: response,
	}
}

// handleStopListening handles when a client stops listening
func (c *Client) handleStopListening() {
	c.listeningMu.Lock()
	wasListening := c.isListening
	c.isListening = false
	c.listeningMu.Unlock()

	if wasListening {
		c.server.clientsMu.RLock()
		totalListening := 0
		for _, client := range c.server.clients {
			client.listeningMu.Lock()
			if client.isListening {
				totalListening++
			}
			client.listeningMu.Unlock()
		}
		c.server.clientsMu.RUnlock()
		log.Printf("%s stopped listening (listening: %d)", c.name, totalListening)

		// Broadcast track update to refresh listener count for remaining clients
		c.server.stateMu.RLock()
		if c.server.currentSong != nil {
			go c.server.broadcastTrackUpdate(c.server.currentSong, false)
		}
		c.server.stateMu.RUnlock()
	}
}

// handleAudioStreamRequest handles audio stream requests
func (c *Client) handleAudioStreamRequest() {
	c.server.stateMu.RLock()
	audioURL := c.server.listenAlong.AudioURL
	var trackID string
	if c.server.currentSong != nil && c.server.currentSong.Track != nil {
		trackID = c.server.currentSong.Track.ID
	}
	c.server.stateMu.RUnlock()

	if audioURL == "" {
		c.send <- types.WebSocketMessage{
			Type: "audio_stream_response",
			Data: map[string]string{"error": "No audio available"},
		}
		return
	}

	// Check if it's a local file
	if len(audioURL) > 7 && audioURL[:7] == "file://" {
		filePath := audioURL[7:]
		// Join or create broadcast for this track
		c.server.joinAudioBroadcast(c, filePath, trackID)
	} else {
		c.send <- types.WebSocketMessage{
			Type: "audio_stream_response",
			Data: map[string]string{"error": "Unsupported audio URL"},
		}
	}
}

// joinAudioBroadcast joins or creates an audio broadcast for a track
// Multiple clients requesting the same track will share a single file read
func (s *Server) joinAudioBroadcast(client *Client, filePath string, trackID string) {
	// Mark client as listening and log if first time
	client.listeningMu.Lock()
	wasListening := client.isListening
	client.isListening = true
	client.listeningMu.Unlock()

	if !wasListening {
		s.clientsMu.RLock()
		totalListening := 0
		for _, c := range s.clients {
			c.listeningMu.Lock()
			if c.isListening {
				totalListening++
			}
			c.listeningMu.Unlock()
		}
		s.clientsMu.RUnlock()
		log.Printf("%s started listening (total: %d)", client.name, totalListening)

		// Broadcast track update to refresh listener count for all clients
		s.stateMu.RLock()
		currentSong := s.currentSong
		s.stateMu.RUnlock()

		if currentSong != nil {
			go s.broadcastTrackUpdate(currentSong, false)
		}
	}

	s.audioBroadcastsMu.Lock()

	broadcast, exists := s.audioBroadcasts[trackID]
	if !exists {
		// Create new broadcast
		broadcast = &audioBroadcast{
			trackID:  trackID,
			filePath: filePath,
			clients:  make(map[string]*Client),
		}
		s.audioBroadcasts[trackID] = broadcast
	}

	broadcast.mu.Lock()
	broadcast.clients[client.id] = client
	clientCount := len(broadcast.clients)
	alreadyStarted := broadcast.started
	broadcast.mu.Unlock()

	s.audioBroadcastsMu.Unlock()

	// If broadcast already started, replay cached chunks
	if alreadyStarted {
		go s.replayBroadcast(client, broadcast)
		return
	}

	// Start the broadcast (first request triggers it)
	broadcast.mu.Lock()
	broadcast.started = true
	broadcast.mu.Unlock()

	log.Printf("Broadcasting to %d clients", clientCount)
	go s.runAudioBroadcast(broadcast)
}

// runAudioBroadcast reads the file once and broadcasts to all listening clients
func (s *Server) runAudioBroadcast(broadcast *audioBroadcast) {
	startTime := time.Now()

	// Track validation
	s.stateMu.RLock()
	var currentTrackID string
	if s.currentSong != nil && s.currentSong.Track != nil {
		currentTrackID = s.currentSong.Track.ID
	}
	s.stateMu.RUnlock()

	if broadcast.trackID != "" && currentTrackID != "" && broadcast.trackID != currentTrackID {
		log.Printf("Aborting broadcast (track changed: %s -> %s)", broadcast.trackID, currentTrackID)
		broadcast.mu.Lock()
		broadcast.err = fmt.Errorf("track changed")
		broadcast.complete = true
		broadcast.mu.Unlock()
		return
	}

	// Security validation
	if !s.isValidAudioFile(broadcast.filePath) {
		s.broadcastError(broadcast, "Access denied")
		return
	}

	file, err := os.Open(broadcast.filePath)
	if err != nil {
		s.broadcastError(broadcast, "File not found")
		return
	}
	defer file.Close()

	info, err := file.Stat()
	if err != nil {
		s.broadcastError(broadcast, "Failed to get file info")
		return
	}

	// Send stream start to all clients
	startMsg := types.WebSocketMessage{
		Type: "audio_stream_start",
		Data: types.AudioStreamStart{
			ContentType:   "audio/ogg; codecs=opus",
			ContentLength: info.Size(),
			Filename:      info.Name(),
			TrackID:       broadcast.trackID,
		},
	}
	s.broadcastToClients(broadcast, startMsg)

	// Read and broadcast chunks
	buffer := make([]byte, chunkSize)
	chunkNum := 0
	totalBytes := int64(0)

	for {
		n, err := file.Read(buffer)
		if n == 0 {
			break
		}

		if err != nil && err != io.EOF {
			s.broadcastError(broadcast, "Read error")
			return
		}

		encodedData := base64.StdEncoding.EncodeToString(buffer[:n])
		chunkMsg := types.WebSocketMessage{
			Type: "audio_chunk",
			Data: types.AudioStreamChunk{
				Data:      encodedData,
				ChunkNum:  chunkNum,
				TotalSize: n,
			},
		}

		s.broadcastToClients(broadcast, chunkMsg)

		chunkNum++
		totalBytes += int64(n)

		if err == io.EOF {
			break
		}
	}

	// Send stream end
	endMsg := types.WebSocketMessage{
		Type: "audio_stream_end",
		Data: types.AudioStreamEnd{
			TotalChunks: chunkNum,
			TotalBytes:  totalBytes,
			TrackID:     broadcast.trackID,
		},
	}
	s.broadcastToClients(broadcast, endMsg)

	broadcast.mu.Lock()
	broadcast.complete = true
	broadcast.mu.Unlock()

	duration := time.Since(startTime)

	broadcast.mu.Lock()
	currentClientCount := len(broadcast.clients)
	broadcast.mu.Unlock()

	log.Printf("Processed %d bytes (%d chunks) to %d clients in %v",
		totalBytes, chunkNum, currentClientCount, duration)

	// Cleanup after a delay
	time.AfterFunc(120*time.Second, func() {
		s.audioBroadcastsMu.Lock()
		delete(s.audioBroadcasts, broadcast.trackID)
		s.audioBroadcastsMu.Unlock()
	})
}

// broadcastToClients sends a message to all clients in a broadcast and caches it
func (s *Server) broadcastToClients(broadcast *audioBroadcast, msg types.WebSocketMessage) {
	broadcast.mu.Lock()
	broadcast.chunks = append(broadcast.chunks, msg)
	clients := make([]*Client, 0, len(broadcast.clients))
	for _, client := range broadcast.clients {
		clients = append(clients, client)
	}
	broadcast.mu.Unlock()

	for _, client := range clients {
		select {
		case client.send <- msg:
		case <-time.After(5 * time.Second):
			log.Printf("%s not consuming, skipping", client.name)
		}
	}
}

// replayBroadcast sends cached chunks to a late-joining client
func (s *Server) replayBroadcast(client *Client, broadcast *audioBroadcast) {
	startTime := time.Now()

	broadcast.mu.Lock()
	chunks := make([]types.WebSocketMessage, len(broadcast.chunks))
	copy(chunks, broadcast.chunks)
	broadcast.mu.Unlock()

	for _, msg := range chunks {
		select {
		case client.send <- msg:
		case <-time.After(5 * time.Second):
			log.Printf("Replay timeout for %s", client.name)
			return
		}
	}

	duration := time.Since(startTime)
	log.Printf("Replayed %d chunks to 1 client in %v", len(chunks), duration)
}

// broadcastError sends an error to all clients in a broadcast
func (s *Server) broadcastError(broadcast *audioBroadcast, errMsg string) {
	broadcast.mu.Lock()
	broadcast.err = fmt.Errorf("%s", errMsg)
	broadcast.complete = true
	clients := make([]*Client, 0, len(broadcast.clients))
	for _, client := range broadcast.clients {
		clients = append(clients, client)
	}
	broadcast.mu.Unlock()

	errorMessage := types.WebSocketMessage{
		Type: "audio_stream_error",
		Data: map[string]string{"error": errMsg},
	}

	for _, client := range clients {
		select {
		case client.send <- errorMessage:
		default:
		}
	}
}

// unregisterClient removes a client from the server
func (s *Server) unregisterClient(client *Client) {
	client.listeningMu.Lock()
	wasListening := client.isListening
	client.listeningMu.Unlock()

	s.clientsMu.Lock()
	if _, ok := s.clients[client.id]; ok {
		delete(s.clients, client.id)
		close(client.send)

		// Only log disconnect if they were listening
		if wasListening {
			totalListening := 0
			for _, c := range s.clients {
				c.listeningMu.Lock()
				if c.isListening {
					totalListening++
				}
				c.listeningMu.Unlock()
			}
			log.Printf("%s disconnected (listening: %d)", client.name, totalListening)

			// Broadcast track update to refresh listener count for remaining clients
			s.stateMu.RLock()
			currentSong := s.currentSong
			s.stateMu.RUnlock()

			if currentSong != nil {
				s.clientsMu.Unlock() // Unlock before broadcast to avoid deadlock
				go s.broadcastTrackUpdate(currentSong, false)
				return
			}
		}
	}
	s.clientsMu.Unlock()
}

// isValidAudioFile validates that a file path is safe to serve
func (s *Server) isValidAudioFile(filePath string) bool {
	// Security checks
	if filePath == "" {
		return false
	}

	// Get absolute cache directory path
	cacheDir := filepath.Join("data", "youtube_cache")
	absCacheDir, err := filepath.Abs(cacheDir)
	if err != nil {
		log.Printf("Security: Failed to resolve cache directory: %v", err)
		return false
	}

	// Clean and get absolute file path
	cleanPath := filepath.Clean(filePath)
	absFilePath, err := filepath.Abs(cleanPath)
	if err != nil {
		log.Printf("Security: Failed to resolve file path: %v", err)
		return false
	}

	// Use filepath.Rel to ensure no directory traversal
	rel, err := filepath.Rel(absCacheDir, absFilePath)
	if err != nil || strings.HasPrefix(rel, "..") {
		log.Printf("Security: Path outside cache directory")
		return false
	}

	// Check if it's actually a file (not directory or symlink)
	info, err := os.Lstat(filePath) // Use Lstat to detect symlinks
	if err != nil || info.IsDir() {
		return false
	}

	// Block symlinks
	if info.Mode()&os.ModeSymlink != 0 {
		log.Printf("Security: Symlinks not allowed")
		return false
	}

	// Check file extension
	allowedExt := []string{".opus", ".webm", ".ogg", ".m4a", ".mp4", ".mkv"}
	ext := strings.ToLower(filepath.Ext(filePath))

	for _, allowed := range allowedExt {
		if ext == allowed {
			return true
		}
	}

	log.Printf("Security: Invalid file extension: %s", ext)
	return false
}
