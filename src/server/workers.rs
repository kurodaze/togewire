use axum::extract::ws::Utf8Bytes;
use std::time::{Duration, Instant};
use tracing::{debug, error, info, warn};

use super::{AppState, TogedemonEvent};
use crate::audio::PrepareError;
use crate::config;
use crate::types::{SpotifyState, Track, TrackUpdateMessage, WsMessage};

/// Connect-state poll pace during steady playback (safe for rate limits).
const POLL_INTERVAL_STEADY: Duration = Duration::from_millis(750);
/// Near a track end or right after a change (skip/seek/pause).
const POLL_INTERVAL_FAST: Duration = Duration::from_millis(250);
/// While idle or paused.
const POLL_INTERVAL_IDLE: Duration = Duration::from_millis(1000);
/// Progress vs expected wall-clock must exceed this to count as a seek (not jitter).
const SEEK_DRIFT_MS: i64 = 3000;
/// Start a new track at 0ms if Spotify is within this much of its start, so polling
/// delay does not truncate the opening notes.
const NEW_TRACK_INTRO_GRACE_MS: i64 = 2500;
/// Push interpolated progress to WS clients at least this often.
const WS_PROGRESS_INTERVAL: Duration = Duration::from_secs(1);

fn effective_seek_ms(progress: i64) -> i64 {
    if progress < NEW_TRACK_INTRO_GRACE_MS {
        0
    } else {
        progress
    }
}

/// Polls Spotify and keeps the stream, cache and listeners in step with it.
pub async fn spotify_monitor_worker(state: AppState) {
    let mut last_track_id: Option<String> = None;
    let mut last_playing = false;
    let mut last_progress = 0i64;
    let mut last_poll = Instant::now();
    let mut last_broadcast = Instant::now();
    let mut consecutive_errors = 0u32;
    let mut fast_polls = 0u32;
    let mut near_end_polls = 0u32;

    while !state.shutdown.is_cancelled() {
        let poll_start = Instant::now();
        // Unpaired or just disconnected reads as nothing playing, so a re-pair is picked
        // up on the next idle poll rather than after an error backoff.
        let s = if !config::get().read().is_authenticated() {
            SpotifyState::default()
        } else {
            match state.spotify_client.get_current_track().await {
                Ok(s) => {
                    consecutive_errors = 0;
                    s
                }
                Err(e) => {
                    // Retry-After is enforced by the client's shared cooldown; this only
                    // spaces out repeated failures (5s doubling to 5min).
                    consecutive_errors += 1;
                    let backoff = Duration::from_secs(5 << (consecutive_errors.min(7) - 1))
                        .min(Duration::from_mins(5));
                    error!("Spotify monitor error: {e} (retry in {backoff:?})");
                    tokio::time::sleep(backoff).await;
                    continue;
                }
            }
        };
        let now = Instant::now();

        let Some(track) = s.track.clone() else {
            *state.current.write() = None;
            if last_track_id.take().is_some() {
                if !state.togedemon_live() {
                    state.stream_manager.stop();
                }
                broadcast_track_update(&state, &s);
                last_playing = false;
            }
            tokio::time::sleep(POLL_INTERVAL_IDLE).await;
            continue;
        };
        *state.current.write() = Some(s.clone());

        if last_track_id.as_deref() != Some(track.id.as_str()) {
            let verb = if s.is_playing {
                "now playing"
            } else {
                "paused on"
            };
            info!("Spotify {verb}: {} - {}", track.artist, track.name);
            switch_track(&state, &s, &track);
            last_track_id = Some(track.id.clone());
            fast_polls = 2;
            near_end_polls = 0;
            last_broadcast = now;
        } else {
            let elapsed = if last_playing {
                last_poll.elapsed().as_millis() as i64
            } else {
                0
            };
            let drift = (s.progress - last_progress - elapsed).abs();
            let cached = state.preparer.get_cached_path(&track);
            let mut changed = false;

            if drift > SEEK_DRIFT_MS {
                debug!("Seek detected (drift: {drift}ms), resyncing stream");
                if state.stream_manager.listener_count() > 0 {
                    info!("Player seeked: {} - {}", track.artist, track.name);
                }
                if let Some(path) = &cached {
                    maybe_play(&state, path, &s);
                }
                changed = true;
            }
            if s.is_playing != last_playing {
                let verb = if s.is_playing { "resumed" } else { "paused" };
                info!("Spotify {verb}: {} - {}", track.artist, track.name);
                if s.is_playing {
                    if let Some(path) = &cached {
                        maybe_play(&state, path, &s);
                    }
                } else if !state.togedemon_live() {
                    state.stream_manager.stop();
                }
                changed = true;
            } else if !state.stream_manager.has_track()
                && let Some(path) = &cached
            {
                changed = maybe_play(&state, path, &s);
            }

            if changed
                || (now - last_broadcast >= WS_PROGRESS_INTERVAL
                    && !state.ws_clients.read().is_empty())
            {
                broadcast_track_update(&state, &s);
                last_broadcast = now;
            }
            if changed {
                fast_polls = 2;
            }
        }
        last_progress = s.progress;
        last_poll = now;
        last_playing = s.is_playing;

        prefetch_next(&state, &track, s.next_track.as_ref());
        prepare_current(&state, &track);

        let interval = compute_poll_interval(
            s.is_playing,
            track.duration,
            s.progress,
            &mut near_end_polls,
            &mut fast_polls,
        );
        debug!(
            "Spotify poll in {}ms: playing={}, progress={}ms, next in {}ms",
            poll_start.elapsed().as_millis(),
            s.is_playing,
            s.progress,
            interval.as_millis()
        );
        tokio::time::sleep(interval).await;
    }
}

/// Play a cached file if one should be streaming now (playing, player on, no togedemon).
fn maybe_play(state: &AppState, path: &str, s: &SpotifyState) -> bool {
    let play = s.is_playing && !state.togedemon_live() && config::get().read().player_enabled;
    if play {
        state
            .stream_manager
            .play_track(path, effective_seek_ms(s.progress));
    }
    play
}

/// Point the stream at a new current track: its cached file, or quiet until prepared.
pub(crate) fn switch_track(state: &AppState, s: &SpotifyState, track: &Track) {
    let played = (state.preparer.get_cached_path(track)).is_some_and(|p| maybe_play(state, &p, s));
    if !played && !state.togedemon_live() {
        state.stream_manager.stop();
    }
    broadcast_track_update(state, s);
}

pub(crate) fn start_stream_if_playing(state: &AppState) {
    if let Some(s) = state.current()
        && let Some(track) = &s.track
        && let Some(path) = state.preparer.get_cached_path(track)
        && maybe_play(state, &path, &s)
    {
        broadcast_track_update(state, &s);
    }
}

/// Start caching the current track unless it is cached, in flight or cooling down. Runs
/// every poll, so a transient failure is retried once its cooldown lapses.
fn prepare_current(state: &AppState, track: &Track) {
    let p = &state.preparer;
    if p.get_cached_path(track).is_none() && !p.is_preparing(track) && !p.recently_failed(track) {
        spawn_prepare(state, track, "Preparing track cache");
    }
}

fn prefetch_next(state: &AppState, current: &Track, next: Option<&Track>) {
    let Some(next) = next else {
        return;
    };
    let p = &state.preparer;
    // The current track goes first. `is_preparing` alone races the spawn that starts it,
    // and "not cached" alone stalls the queue forever behind a track that cannot be found.
    if p.get_cached_path(current).is_none() && !p.recently_failed(current) {
        return;
    }
    if let Some(path) = p.get_cached_path(next) {
        state.stream_manager.prewarm(&path);
    } else if !p.is_preparing(next) && !p.recently_failed(next) {
        spawn_prepare(state, next, "Prefetching next");
    }
}

/// Cache `track` in the background. Once ready it plays if it is current by then (a
/// prefetch the listener skipped to included), else its streaming copy is prewarmed.
fn spawn_prepare(state: &AppState, track: &Track, what: &str) {
    info!("{what}: {} - {}", track.artist, track.name);
    let state = state.clone();
    let track = track.clone();
    tokio::spawn(async move {
        let result = state.preparer.prepare_track(&track).await;
        let name = format!("{} - {}", track.artist, track.name);
        match result {
            Ok(path) => {
                let path = path.to_string_lossy();
                match state
                    .current()
                    .filter(|s| s.track.as_ref().is_some_and(|t| t.id == track.id))
                {
                    // The monitor may have started it already once the entry appeared.
                    Some(s) if !state.stream_manager.has_track() => {
                        maybe_play(&state, &path, &s);
                    }
                    Some(_) => {}
                    None => state.stream_manager.prewarm(&path),
                }
            }
            Err(PrepareError::Busy) => debug!("Already preparing: {name}"),
            Err(PrepareError::NoSource) => warn!("No YouTube source for {name}"),
            Err(PrepareError::Failed(msg)) => warn!("Caching failed for {name}: {msg}"),
        }
        // `preparing` / `no_source` just changed.
        rebroadcast(&state);
    });
}

/// The `track_update` frame for both the on-connect snapshot and broadcasts, serialised
/// once so N listeners cost one encode plus N refcount bumps.
pub(crate) fn track_update_message(state: &AppState, s: &SpotifyState) -> Utf8Bytes {
    let track = s.track.clone();
    let data = TrackUpdateMessage {
        progress: if track.is_some() { s.progress } else { 0 },
        is_playing: track.is_some() && s.is_playing,
        stream_active: state.togedemon_live() || state.stream_manager.has_track(),
        stream_gen: state.stream_manager.generation(),
        listener_count: state.stream_manager.listener_count(),
        preparing: (track.as_ref()).is_some_and(|t| state.preparer.is_preparing(t)),
        no_source: (track.as_ref()).is_some_and(|t| state.preparer.recently_failed(t)),
        track,
    };
    let msg = WsMessage {
        msg_type: "track_update",
        data,
    };
    serde_json::to_string(&msg).unwrap_or_default().into()
}

pub fn broadcast_track_update(state: &AppState, s: &SpotifyState) {
    let msg = track_update_message(state, s);
    for tx in state.ws_clients.read().values() {
        let _ = tx.try_send(msg.clone());
    }
    let _ = state.togedemon_events.send(TogedemonEvent::new(Some(s)));
}

/// Re-send the current state after something besides Spotify changed (listeners, prepare).
pub(crate) fn rebroadcast(state: &AppState) {
    if let Some(s) = state.current() {
        broadcast_track_update(state, &s);
    }
}

fn compute_poll_interval(
    is_playing: bool,
    duration: i64,
    progress: i64,
    near_end_polls: &mut u32,
    fast_polls: &mut u32,
) -> Duration {
    let remaining = duration - progress;
    let near_end =
        is_playing && duration > 0 && (0..=2500).contains(&remaining) && *near_end_polls < 8;
    if near_end {
        *near_end_polls += 1;
    } else if !(-1000..=2500).contains(&remaining) {
        *near_end_polls = 0;
    }

    if !is_playing {
        *near_end_polls = 0;
        POLL_INTERVAL_IDLE
    } else if near_end {
        POLL_INTERVAL_FAST
    } else if *fast_polls > 0 {
        *fast_polls -= 1;
        POLL_INTERVAL_FAST
    } else {
        POLL_INTERVAL_STEADY
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn poll_interval_bounds_and_caps() {
        let (mut near, mut fast) = (0, 0);
        // Normal playback is steady.
        assert_eq!(
            compute_poll_interval(true, 200_000, 50_000, &mut near, &mut fast),
            POLL_INTERVAL_STEADY
        );
        // Near end polls fast up to the cap (8), then backs off to steady.
        for _ in 0..8 {
            assert_eq!(
                compute_poll_interval(true, 200_000, 198_500, &mut near, &mut fast),
                POLL_INTERVAL_FAST
            );
        }
        assert_eq!(
            compute_poll_interval(true, 200_000, 198_500, &mut near, &mut fast),
            POLL_INTERVAL_STEADY
        );
        // Past track end must never fast-poll.
        assert_eq!(
            compute_poll_interval(true, 200_000, 205_000, &mut near, &mut fast),
            POLL_INTERVAL_STEADY
        );
        // Paused is idle and resets near-end tracking.
        assert_eq!(
            compute_poll_interval(false, 200_000, 198_500, &mut near, &mut fast),
            POLL_INTERVAL_IDLE
        );
        assert_eq!(near, 0);
    }
}
