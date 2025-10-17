package server

import (
	"fmt"
	"log"
	"time"

	"github.com/kurodaze/togewire/internal/config"
	"github.com/kurodaze/togewire/internal/types"
)

// spotifyMonitorWorker monitors Spotify for track changes
func (s *Server) spotifyMonitorWorker() {
	cfg := config.Get()
	if !cfg.IsAuthenticated() {
		log.Println("Waiting for Spotify authentication...")
		for !cfg.IsAuthenticated() {
			time.Sleep(5 * time.Second)
		}
	}

	log.Println("Spotify monitoring started")

	var lastTrackID string
	var lastPlayingState bool
	var lastQueueCheck time.Time
	var lastQueueTrackID string // Track the last queue track to avoid spam

	for {
		select {
		case <-s.ctx.Done():
			return
		default:
		}

		// Get current song
		state, err := s.spotifyClient.GetCurrentSong()
		if err != nil {
			log.Printf("Spotify monitor error: %v", err)
			time.Sleep(5 * time.Second)
			continue
		}

		currentTime := time.Now()

		// Handle case when nothing is playing
		if state.Track == nil {
			if lastTrackID != "" {
				s.handleTrackChange(nil, state)
				lastTrackID = ""
				lastPlayingState = false
			}
			time.Sleep(2 * time.Second)
			continue
		}

		// Update current state
		s.stateMu.Lock()
		s.currentSong = state
		s.listenAlong.Progress = state.Progress
		s.listenAlong.IsPlaying = state.IsPlaying
		s.listenAlong.LastSync = currentTime
		s.stateMu.Unlock()

		trackID := state.Track.ID

		// Handle track changes
		if trackID != lastTrackID {
			log.Printf("Now playing: %s - %s", state.Track.Artist, state.Track.Name)
			s.handleTrackChange(state.Track, state)
			lastTrackID = trackID
			lastQueueCheck = currentTime.Add(-10 * time.Second) // Force queue check
		} else if state.IsPlaying != lastPlayingState {
			// Broadcast state changes without track change
			s.broadcastTrackUpdate(state, false)
		}

		// Queue checking for next track preparation
		queueInterval := 10 * time.Second // Less frequent checking
		if !state.IsPlaying {
			queueInterval = 20 * time.Second // Even less when paused
		}

		if currentTime.Sub(lastQueueCheck) >= queueInterval {
			queue, err := s.spotifyClient.GetQueue()
			if err == nil && len(queue) > 0 {
				nextTrackID := queue[0].ID
				if nextTrackID != lastQueueTrackID {
					lastQueueTrackID = nextTrackID
					s.checkQueueForNextTrack()
				}
			}
			lastQueueCheck = currentTime
		}

		lastPlayingState = state.IsPlaying

		// Dynamic polling interval
		pollInterval := s.spotifyClient.GetRecommendedPollInterval(state.IsPlaying)

		// Poll faster when clients are connected
		s.clientsMu.RLock()
		if len(s.clients) > 0 {
			pollInterval = pollInterval / 2
		}
		s.clientsMu.RUnlock()

		time.Sleep(pollInterval)
	}
}

// handleTrackChange handles Spotify track changes with optimized preparation
func (s *Server) handleTrackChange(track *types.Track, state *types.SpotifyState) {
	// Clear current audio
	s.stateMu.Lock()
	s.listenAlong.AudioURL = ""
	s.stateMu.Unlock()

	// Broadcast immediately to stop old audio
	s.broadcastTrackUpdate(state, true)

	if track == nil {
		return
	}

	// Check if next track is already prepared
	s.stateMu.RLock()
	nextSong := s.nextSong
	s.stateMu.RUnlock()

	if nextSong != nil && nextSong.Track.ID == track.ID {
		// Use prepared track
		log.Printf("Using cached track: %s - %s", track.Artist, track.Name)

		s.stateMu.Lock()
		s.listenAlong.AudioURL = fmt.Sprintf("file://%s", nextSong.FilePath)
		s.nextSong = nil
		s.stateMu.Unlock()

		s.broadcastTrackUpdate(state, false)

		// Queue monitoring will handle next track preparation
		return
	}

	// Prepare current track in background goroutine for minimal blocking
	// Uses cached state to avoid 500ms Spotify API delay on broadcast
	log.Printf("Preparing: %s - %s", track.Artist, track.Name)

	go func(currentState *types.SpotifyState) {
		filePath, err := s.youtubeClient.PrepareTrack(track)
		if err != nil {
			log.Printf("Failed to prepare track: %v", err)
			return
		}

		// Verify track is still current before broadcasting
		// Prevents stale audio broadcasts if user skipped during download
		s.stateMu.Lock()
		stillCurrent := s.currentSong != nil && s.currentSong.Track != nil &&
			s.currentSong.Track.ID == track.ID
		if stillCurrent {
			s.listenAlong.AudioURL = fmt.Sprintf("file://%s", filePath)
		}
		s.stateMu.Unlock()

		if !stillCurrent {
			log.Printf("Track changed during download: %s - %s", track.Artist, track.Name)
			return
		}

		log.Printf("Audio ready: %s - %s", track.Artist, track.Name)

		// Broadcast with cached state (no Spotify API delay)
		s.broadcastTrackUpdate(currentState, false)
	}(state)
}

// checkQueueForNextTrack checks the queue and prepares the next track
func (s *Server) checkQueueForNextTrack() {
	queue, err := s.spotifyClient.GetQueue()
	if err != nil || len(queue) == 0 {
		return
	}

	nextTrack := queue[0]

	s.stateMu.RLock()
	currentNextTrack := s.nextSong
	currentTrack := s.currentSong
	s.stateMu.RUnlock()

	// If already preparing this track, skip
	if currentNextTrack != nil && currentNextTrack.Track.ID == nextTrack.ID {
		return
	}

	// Check if it's the same as current track (rapid skipping)
	if currentTrack != nil && currentTrack.Track != nil && currentTrack.Track.ID == nextTrack.ID {
		return
	}

	// Only log when queue actually changes to a different track
	log.Printf("Queue: next track %s - %s", nextTrack.Artist, nextTrack.Name)

	go s.prepareNextTrack(&nextTrack)
}

// prepareNextTrack prepares the next queued track in background for seamless transitions
// This allows instant playback when user skips to the next song
func (s *Server) prepareNextTrack(track *types.Track) {
	// Check if already being prepared (before logging to avoid spam)
	if s.youtubeClient.IsPreparingTrack(track) {
		return
	}

	// Check if already prepared as next track
	s.stateMu.RLock()
	if s.nextSong != nil && s.nextSong.Track.ID == track.ID {
		s.stateMu.RUnlock()
		return
	}
	s.stateMu.RUnlock()

	log.Printf("Preparing next track: %s - %s", track.Artist, track.Name)

	bgPrepStart := time.Now()
	filePath, err := s.youtubeClient.PrepareTrack(track)
	bgPrepDuration := time.Since(bgPrepStart)

	if err != nil {
		log.Printf("Failed to prepare next track: %v", err)
		return
	}

	// Validate track is still next or current
	queue, err := s.spotifyClient.GetQueue()
	if err == nil && len(queue) > 0 && queue[0].ID == track.ID {
		// Still next track
		s.stateMu.Lock()
		s.nextSong = &types.PreparedTrack{
			Track:    track,
			FilePath: filePath,
			ReadyAt:  time.Now(),
		}
		s.stateMu.Unlock()

		log.Printf("Next track ready: %s - %s (took %v)", track.Artist, track.Name, bgPrepDuration)
	} else {
		// Check if it became current track (without expensive API call)
		s.stateMu.Lock()
		isCurrent := s.currentSong != nil && s.currentSong.Track != nil && s.currentSong.Track.ID == track.ID
		if isCurrent {
			s.listenAlong.AudioURL = fmt.Sprintf("file://%s", filePath)
			state := s.currentSong // Use cached state
			s.stateMu.Unlock()

			log.Printf("Background prep completed: %s - %s", track.Artist, track.Name)
			s.broadcastTrackUpdate(state, false)
		} else {
			s.stateMu.Unlock()
			log.Printf("Track no longer relevant: %s - %s", track.Artist, track.Name)
		}
	}
}

// broadcastTrackUpdate broadcasts a track update to all clients
func (s *Server) broadcastTrackUpdate(state *types.SpotifyState, trackChanged bool) {
	if state == nil {
		return
	}

	s.stateMu.RLock()
	audioReady := s.listenAlong.AudioURL != ""
	s.stateMu.RUnlock()

	var track *types.Track
	var progress, duration int64
	var isPlaying bool

	if state.Track != nil {
		track = state.Track
		progress = state.Progress
		duration = state.Track.Duration
		isPlaying = state.IsPlaying
	}

	msg := types.WebSocketMessage{
		Type: "track_update",
		Data: types.TrackUpdateMessage{
			Track:        track,
			Progress:     progress,
			IsPlaying:    isPlaying,
			Duration:     duration,
			AudioReady:   audioReady,
			TrackChanged: trackChanged,
		},
	}

	select {
	case s.broadcast <- msg:
	default:
		log.Println("Warning: Broadcast channel full")
	}
}
