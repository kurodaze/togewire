package types

import "time"

// Track represents a Spotify track
type Track struct {
	ID        string   `json:"id"`
	Name      string   `json:"name"`
	Artist    string   `json:"artist"`
	Artists   []string `json:"artists,omitempty"`
	Album     string   `json:"album,omitempty"`
	AlbumArt  string   `json:"album_art,omitempty"`
	Duration  int64    `json:"duration"`           // milliseconds
	Progress  int64    `json:"progress,omitempty"` // milliseconds
	IsPlaying bool     `json:"is_playing,omitempty"`
}

// SpotifyState represents current Spotify playback state
type SpotifyState struct {
	Track     *Track `json:"track"`
	Progress  int64  `json:"progress"` // milliseconds
	IsPlaying bool   `json:"is_playing"`
}

// PreparedTrack represents a track that has been downloaded and prepared
type PreparedTrack struct {
	Track    *Track
	FilePath string
	ReadyAt  time.Time
}

// AudioStreamChunk represents a chunk of audio data for WebSocket streaming
type AudioStreamChunk struct {
	Data      string `json:"data"`
	ChunkNum  int    `json:"chunk"`
	TotalSize int    `json:"size"`
}

// AudioStreamStart represents the start of an audio stream
type AudioStreamStart struct {
	ContentType   string `json:"content_type"`
	ContentLength int64  `json:"content_length"`
	Filename      string `json:"filename"`
	TrackID       string `json:"track_id"`
}

// AudioStreamEnd represents the end of an audio stream
type AudioStreamEnd struct {
	TotalChunks int    `json:"total_chunks"`
	TotalBytes  int64  `json:"total_bytes"`
	TrackID     string `json:"track_id"`
}

// WebSocketMessage represents a generic WebSocket message
type WebSocketMessage struct {
	Type string      `json:"type"`
	Data interface{} `json:"data"`
}

// TrackUpdateMessage represents a track update broadcast
type TrackUpdateMessage struct {
	Track         *Track `json:"track"`
	Progress      int64  `json:"progress"`
	IsPlaying     bool   `json:"isPlaying"`
	Duration      int64  `json:"duration"`
	AudioReady    bool   `json:"audio_ready"`
	TrackChanged  bool   `json:"track_changed,omitempty"`
	FilePath      string `json:"file_path,omitempty"`
	Error         string `json:"error,omitempty"`
	Timestamp     int64  `json:"timestamp,omitempty"`
	ListenerCount int    `json:"listener_count"`
}

// SyncResponse represents a sync response for the player
type SyncResponse struct {
	Progress  int64  `json:"progress"`
	IsPlaying bool   `json:"is_playing"`
	Timestamp int64  `json:"timestamp"`
	TrackID   string `json:"track_id"`
}
