package server

import (
	"fmt"
	"time"

	"github.com/kurodaze/togewire/internal/config"
	"github.com/kurodaze/togewire/internal/logger"
	"github.com/kurodaze/togewire/internal/types"
)

// spotifyMonitorWorker monitors Spotify for track changes
func (s *Server) spotifyMonitorWorker() {
	cfg := config.Get()
	if !cfg.IsAuthenticated() {
		logger.Info("Waiting for Spotify authentication...")
		for !cfg.IsAuthenticated() {
			time.Sleep(5 * time.Second)
		}
	}

	logger.Debug("Spotify monitoring started")

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

		// Get current track
		state, err := s.spotifyClient.GetCurrentTrack()
		if err != nil {
			logger.Error("Spotify monitor error: %v", err)
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
		s.currentTrack = state
		s.listenAlong.Progress = state.Progress
		s.listenAlong.IsPlaying = state.IsPlaying
		s.listenAlong.LastSync = currentTime
		s.stateMu.Unlock()

		trackID := state.Track.ID

		// Handle track changes
		if trackID != lastTrackID {
			logger.Info("Now playing: %s - %s", state.Track.Artist, state.Track.Name)
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
	// Clear current audio and error state
	s.stateMu.Lock()
	s.listenAlong.AudioURL = ""
	s.listenAlong.Error = ""
	s.stateMu.Unlock()

	// Broadcast immediately to stop old audio
	s.broadcastTrackUpdate(state, true)

	if track == nil {
		// Nothing playing - clear next track preparation as well
		s.stateMu.Lock()
		s.nextTrack = nil
		s.stateMu.Unlock()
		return
	}

	// Check if next track is already prepared
	s.stateMu.RLock()
	nextTrack := s.nextTrack
	s.stateMu.RUnlock()

	if nextTrack != nil && nextTrack.Track.ID == track.ID {
		// Use prepared track
		logger.Debug("Using cached track: %s - %s", track.Artist, track.Name)

		s.stateMu.Lock()
		s.listenAlong.AudioURL = fmt.Sprintf("file://%s", nextTrack.FilePath)
		s.nextTrack = nil
		s.stateMu.Unlock()

		s.broadcastTrackUpdate(state, false)

		// Queue monitoring will handle next track preparation
		return
	}

	// Prepare current track in background goroutine for minimal blocking
	// Uses cached state to avoid 500ms Spotify API delay on broadcast
	logger.Debug("Preparing: %s - %s", track.Artist, track.Name)

	go func(currentState *types.SpotifyState) {
		filePath, err := s.youtubeClient.PrepareTrack(track)
		if err != nil {
			// Check if track is just already being prepared (not a real error)
			errMsg := err.Error()
			if errMsg == "track already being prepared" {
				logger.Debug("Track already being prepared: %s - %s", track.Artist, track.Name)
				// Don't set error state - the other goroutine will complete and broadcast
				return
			}

			logger.Info("Failed to prepare track: %v", err)

			// Set error state if track is still current
			s.stateMu.Lock()
			stillCurrent := s.currentTrack != nil && s.currentTrack.Track != nil &&
				s.currentTrack.Track.ID == track.ID
			if stillCurrent {
				s.listenAlong.Error = "Audio not found"
			}
			s.stateMu.Unlock()

			if stillCurrent {
				// Broadcast with error
				s.broadcastTrackUpdateWithError(currentState, false, "Audio not found")
			}
			return
		}

		// Verify track is still current before broadcasting
		// Prevents stale audio broadcasts if user skipped during download
		s.stateMu.Lock()
		stillCurrent := s.currentTrack != nil && s.currentTrack.Track != nil &&
			s.currentTrack.Track.ID == track.ID
		if stillCurrent {
			s.listenAlong.AudioURL = fmt.Sprintf("file://%s", filePath)
			s.listenAlong.Error = "" // Clear any previous error
		}
		s.stateMu.Unlock()

		if !stillCurrent {
			logger.Info("Track changed during download: %s - %s", track.Artist, track.Name)
			return
		}

		logger.Debug("Audio ready: %s - %s", track.Artist, track.Name)

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

	nextInQueue := queue[0]

	s.stateMu.RLock()
	currentNextTrack := s.nextTrack
	currentTrack := s.currentTrack
	s.stateMu.RUnlock()

	// If already preparing this track, skip
	if currentNextTrack != nil && currentNextTrack.Track.ID == nextInQueue.ID {
		return
	}

	// Check if it's the same as current track (rapid skipping)
	if currentTrack != nil && currentTrack.Track != nil && currentTrack.Track.ID == nextInQueue.ID {
		return
	}

	// Only log when queue actually changes to a different track
	logger.Debug("Queue: next track %s - %s", nextInQueue.Artist, nextInQueue.Name)

	go s.prepareNextTrack(&nextInQueue)
}

// prepareNextTrack prepares the next queued track in background for seamless transitions
// This allows instant playback when user skips to the next track
func (s *Server) prepareNextTrack(track *types.Track) {
	// Check if already being prepared (before logging to avoid spam)
	if s.youtubeClient.IsPreparingTrack(track) {
		return
	}

	// Check if already prepared as next track
	s.stateMu.RLock()
	if s.nextTrack != nil && s.nextTrack.Track.ID == track.ID {
		s.stateMu.RUnlock()
		return
	}
	s.stateMu.RUnlock()

	logger.Debug("Preparing next track: %s - %s", track.Artist, track.Name)

	bgPrepStart := time.Now()
	filePath, err := s.youtubeClient.PrepareTrack(track)
	bgPrepDuration := time.Since(bgPrepStart)

	if err != nil {
		logger.Info("Failed to prepare next track: %v", err)
		return
	}

	// Validate track is still next or current
	queue, err := s.spotifyClient.GetQueue()
	if err == nil && len(queue) > 0 && queue[0].ID == track.ID {
		// Still next track
		s.stateMu.Lock()
		s.nextTrack = &types.PreparedTrack{
			Track:    track,
			FilePath: filePath,
			ReadyAt:  time.Now(),
		}
		s.stateMu.Unlock()

		logger.Debug("Next track ready: %s - %s (took %v)", track.Artist, track.Name, bgPrepDuration)
	} else {
		// Check if it became current track (without expensive API call)
		s.stateMu.Lock()
		isCurrent := s.currentTrack != nil && s.currentTrack.Track != nil && s.currentTrack.Track.ID == track.ID
		if isCurrent {
			s.listenAlong.AudioURL = fmt.Sprintf("file://%s", filePath)
			state := s.currentTrack // Use cached state
			s.stateMu.Unlock()

			logger.Debug("Background prep completed: %s - %s", track.Artist, track.Name)
			s.broadcastTrackUpdate(state, false)
		} else {
			s.stateMu.Unlock()
			logger.Debug("Track no longer relevant: %s - %s", track.Artist, track.Name)
		}
	}
}

// broadcastTrackUpdate broadcasts a track update to all clients
func (s *Server) broadcastTrackUpdate(state *types.SpotifyState, trackChanged bool) {
	s.broadcastTrackUpdateWithError(state, trackChanged, "")
}

// broadcastTrackUpdateWithError broadcasts a track update with error override
// errorMsg temporarily overrides s.listenAlong.Error for this broadcast only
func (s *Server) broadcastTrackUpdateWithError(state *types.SpotifyState, trackChanged bool, errorMsg string) {
	if state == nil {
		return
	}

	s.stateMu.RLock()
	audioReady := s.listenAlong.AudioURL != ""
	// If no error override provided, use the stored error state
	if errorMsg == "" {
		errorMsg = s.listenAlong.Error
	}
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
			Error:        errorMsg,
		},
	}

	select {
	case s.broadcast <- msg:
	default:
		logger.Warn("Broadcast channel full")
	}
}

// idleOptimizationWorker upgrades low-quality cached tracks when yt-dlp is idle
func (s *Server) idleOptimizationWorker() {
	const idleThreshold = 2 * time.Hour
	const maxUpgradesPerRun = 10
	const checkInterval = 5 * time.Minute

	logger.Debug("Idle optimization worker started")

	ticker := time.NewTicker(checkInterval)
	defer ticker.Stop()

	upgradeTimer := time.NewTimer(0)
	upgradeTimer.Stop()

	for {
		select {
		case <-s.ctx.Done():
			return

		case <-ticker.C:
			// Check if yt-dlp has been idle long enough
			timeSinceDownload := s.youtubeClient.GetTimeSinceLastDownload()

			if timeSinceDownload < idleThreshold {
				upgradeTimer.Stop()
				continue
			}

			if s.youtubeClient.IsCurrentlyDownloading() {
				upgradeTimer.Stop()
				continue
			}

			needsUpgrade := s.youtubeClient.GetEntriesNeedingUpgrade()
			if len(needsUpgrade) == 0 {
				continue
			}

			upgradeTimer.Reset(0)

		case <-upgradeTimer.C:
			if s.youtubeClient.IsCurrentlyDownloading() {
				logger.Info("Download activity detected, skipping upgrade cycle")
				continue
			}

			timeSinceDownload := s.youtubeClient.GetTimeSinceLastDownload()
			if timeSinceDownload < idleThreshold {
				logger.Info("Recent download activity detected, skipping upgrade cycle")
				continue
			}

			needsUpgrade := s.youtubeClient.GetEntriesNeedingUpgrade()
			if len(needsUpgrade) == 0 {
				continue
			}

			totalTracks := len(needsUpgrade)
			upgradeCount := min(totalTracks, maxUpgradesPerRun)

			logger.Info("yt-dlp idle for %v, upgrading up to %d low-quality tracks (from %d total)...",
				timeSinceDownload.Round(time.Minute), upgradeCount, totalTracks)

			upgraded := 0
			skipped := 0

			for key, entry := range needsUpgrade {
				if upgraded >= maxUpgradesPerRun {
					break
				}

				if s.youtubeClient.IsCurrentlyDownloading() {
					logger.Info("Download activity detected, pausing optimization (upgraded %d, skipped %d)", upgraded, skipped)
					upgradeTimer.Stop()
					break
				}

				// Upgrade track
				if err := s.youtubeClient.UpgradeTrack(key, entry); err == nil {
					upgraded++
				} else {
					skipped++
				}

				// Small delay between upgrades
				time.Sleep(2 * time.Second)
			}

			if upgraded > 0 || skipped > 0 {
				logger.Info("Optimization cycle complete: upgraded %d tracks, skipped %d", upgraded, skipped)
			}

			// Check if there are still tracks needing upgrade
			remainingUpgrades := s.youtubeClient.GetEntriesNeedingUpgrade()
			if len(remainingUpgrades) > 0 && !s.youtubeClient.IsCurrentlyDownloading() {
				logger.Info("%d tracks still need upgrade, scheduling next cycle in %v",
					len(remainingUpgrades), idleThreshold)
				upgradeTimer.Reset(idleThreshold)
			}
		}
	}
}
