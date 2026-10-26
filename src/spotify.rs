use anyhow::{Result, bail};
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex as TokioMutex;
use tracing::{info, warn};

use crate::config;
use crate::types::{SpotifyState, Track};

pub mod parsing;
pub(crate) use parsing::{
    ConnectStateSnapshot, parse_connect_state_snapshot, parse_metadata_track, track_id_from_uri,
};

pub const SPOTIFY_CLIENT_ID: &str = "65b708073fc0480ea92a077233ca87bd";
const SPOTIFY_TOKEN_URL: &str = "https://accounts.spotify.com/api/token";
const DEVICE_AUTH_URL: &str = "https://accounts.spotify.com/oauth2/device/authorize";
const APRESOLVE_URL: &str = "https://apresolve.spotify.com/?type=spclient";
const SPCLIENT_DEFAULT_HOST: &str = "spclient.wg.spotify.com";

const SPCLIENT_CACHE_SECS: u64 = 900; // 15 min
const MIN_RATE_LIMIT_COOLDOWN_SECS: u64 = 10;
const TRACK_METADATA_CACHE_MAX_ENTRIES: usize = 2048;
/// A failed metadata lookup is tried again after this, not every poll.
const TRACK_METADATA_RETRY_SECS: u64 = 60;

/// Token endpoint reply, shared by refresh and device pairing.
#[derive(Deserialize)]
pub struct TokenResponse {
    pub access_token: String,
    pub refresh_token: Option<String>,
    #[serde(default = "default_expires_in")]
    pub expires_in: u64,
}

fn default_expires_in() -> u64 {
    3600
}

/// RFC 8628 device authorization reply, handed to the dashboard as-is.
#[derive(Serialize, Deserialize)]
pub struct DeviceAuthResponse {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: Option<String>,
    pub verification_uri_complete: Option<String>,
    pub interval: Option<u64>,
}

/// A track's metadata, None for a missing entry, and when a lookup that failed may be tried
/// again.
type CachedTrack = (Option<Track>, Option<Instant>);

#[derive(Clone)]
pub struct SpotifyClient {
    inner: Arc<Inner>,
}

struct Inner {
    /// Cached access token and when it expires.
    token_cache: TokioMutex<Option<(String, Instant)>>,
    /// Serialises token refresh so a simultaneous miss costs one request, not several.
    token_refresh: TokioMutex<()>,
    /// Global cooldown set from Retry-After responses.
    rate_limit_until: TokioMutex<Option<Instant>>,
    /// Cached spclient host from apresolve.
    spclient_host_cache: TokioMutex<Option<(String, Instant)>>,
    /// Cached track metadata by track URI.
    track_metadata_cache: TokioMutex<HashMap<String, CachedTrack>>,
    /// Stable connect-state observer identifiers for this process.
    connect_observer_device_id: String,
    connect_connection_id: String,
}

#[derive(Deserialize)]
struct ApresolveResponse {
    #[serde(default)]
    spclient: Vec<String>,
}

impl Default for SpotifyClient {
    fn default() -> Self {
        Self {
            inner: Arc::new(Inner {
                token_cache: TokioMutex::new(None),
                token_refresh: TokioMutex::new(()),
                rate_limit_until: TokioMutex::new(None),
                spclient_host_cache: TokioMutex::new(None),
                track_metadata_cache: TokioMutex::new(HashMap::new()),
                connect_observer_device_id: crate::random_hex(20),
                connect_connection_id: crate::random_hex(16),
            }),
        }
    }
}

impl SpotifyClient {
    pub async fn clear_cached_token(&self) {
        *self.inner.token_cache.lock().await = None;
    }

    pub async fn clear_session(&self) {
        self.clear_cached_token().await;
        let _ = config::update(|cfg| cfg.spotify_auth = None);
    }

    /// Get a valid bearer token, refreshing if expired.
    pub async fn access_token(&self) -> Result<String> {
        if let Some(cached) = self.cached_token().await {
            return Ok(cached);
        }

        let _refresh = self.inner.token_refresh.lock().await;
        if let Some(cached) = self.cached_token().await {
            return Ok(cached);
        }

        let Some(auth) = config::get().read().spotify_auth.clone() else {
            bail!("Spotify not authenticated. Pair Togewire via spotify.com/pair.")
        };
        let now = crate::now_unix();
        let (token, expires_in) = if now < auth.expires_at_unix - 60 {
            (auth.access_token, (auth.expires_at_unix - now) as u64)
        } else {
            let refresh = auth
                .refresh_token
                .ok_or_else(|| anyhow::anyhow!("Spotify token expired and no refresh token"))?;
            self.refresh_oauth_token(&refresh).await?
        };
        *self.inner.token_cache.lock().await = Some((
            token.clone(),
            Instant::now() + Duration::from_secs(expires_in),
        ));
        Ok(token)
    }

    async fn refresh_oauth_token(&self, refresh_token: &str) -> Result<(String, u64)> {
        let resp = crate::http()
            .post(SPOTIFY_TOKEN_URL)
            .form(&[
                ("client_id", SPOTIFY_CLIENT_ID),
                ("grant_type", "refresh_token"),
                ("refresh_token", refresh_token),
            ])
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            if text.contains("invalid_grant") {
                self.clear_session().await;
            }
            bail!("Spotify token refresh failed ({status}): {text}");
        }

        let payload: TokenResponse = resp.json().await?;
        let next_refresh = payload.refresh_token.as_deref().or(Some(refresh_token));
        // Spotify may rotate the refresh token; losing it here means re-pairing after a restart.
        if let Err(e) =
            config::save_spotify_auth(&payload.access_token, next_refresh, payload.expires_in)
        {
            warn!("Could not save refreshed Spotify tokens: {e}");
        }
        Ok((payload.access_token, payload.expires_in))
    }

    /// Cached token, treated as expired 60s early so callers never use a dying one.
    async fn cached_token(&self) -> Option<String> {
        let cache = self.inner.token_cache.lock().await;
        let (token, expires) = cache.as_ref()?;
        (Instant::now() + Duration::from_mins(1) < *expires).then(|| token.clone())
    }

    /// Start pairing via `spotify.com/pair` (RFC 8628) with the desktop client id.
    pub async fn request_device_code(&self) -> Result<DeviceAuthResponse, String> {
        let resp = crate::http()
            .post(DEVICE_AUTH_URL)
            .form(&[("client_id", SPOTIFY_CLIENT_ID), ("scope", "streaming")])
            .send()
            .await
            .map_err(|e| format!("failed to reach Spotify device auth endpoint: {e}"))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(format!(
                "Spotify rejected device authorization ({status}): {}",
                crate::tail_chars(&body, 200)
            ));
        }
        resp.json()
            .await
            .map_err(|e| format!("invalid device auth JSON from Spotify: {e}"))
    }

    /// One RFC 8628 §3.4 poll. `Ok(Err(status))` while the user has not approved yet:
    /// "pending", or "slow_down" when the client must add 5s to its interval.
    pub async fn poll_device_token(
        &self,
        device_code: &str,
    ) -> Result<Result<TokenResponse, &'static str>, String> {
        let resp = crate::http()
            .post(SPOTIFY_TOKEN_URL)
            .form(&[
                ("client_id", SPOTIFY_CLIENT_ID),
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                ("device_code", device_code),
            ])
            .send()
            .await
            .map_err(|e| format!("token poll failed: {e}"))?;
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if status.is_success() {
            return serde_json::from_str(&body)
                .map(Ok)
                .map_err(|e| format!("invalid token JSON from Spotify: {e}"));
        }
        let err: Value = serde_json::from_str(&body).unwrap_or_default();
        match err["error"].as_str() {
            Some("authorization_pending") => Ok(Err("pending")),
            Some("slow_down") => Ok(Err("slow_down")),
            Some("access_denied") => {
                Err("Spotify authorization was declined on the pair page".into())
            }
            Some("expired_token") => Err("Spotify pairing code expired".into()),
            Some(other) => Err(format!(
                "Spotify returned error '{other}': {}",
                err["error_description"].as_str().unwrap_or_default()
            )),
            None => Err(format!(
                "token exchange failed ({status}): {}",
                crate::tail_chars(&body, 200)
            )),
        }
    }

    /// Store freshly paired tokens and pick the cache source for the plan; returns Premium.
    pub async fn complete_pairing(&self, token: &TokenResponse) -> Result<bool, String> {
        config::save_spotify_auth(
            &token.access_token,
            token.refresh_token.as_deref(),
            token.expires_in,
        )
        .map_err(|e| format!("failed to save auth tokens: {e}"))?;
        self.clear_cached_token().await;
        Ok(self.sync_librespot_for_subscription().await)
    }

    /// Derive `use_librespot` from the account plan; returns whether it is Premium.
    pub async fn sync_librespot_for_subscription(&self) -> bool {
        // Fail closed: librespot-core calls exit(1) on a non-premium account (session.rs
        // check_catalogue), so an unconfirmed plan must not enable it. yt-dlp covers it.
        let plan = match self.account_is_premium().await {
            Ok(premium) => Some(premium),
            Err(e) => {
                warn!("Could not confirm Spotify Premium ({e}); librespot stays off");
                None
            }
        };
        config::set_premium(plan);
        let premium = plan == Some(true);
        config::set_use_librespot(premium);
        info!(
            "Spotify {} - librespot {}",
            match plan {
                Some(true) => "Premium",
                Some(false) => "Free",
                None => "plan unknown",
            },
            if config::use_librespot() {
                "enabled"
            } else {
                "off, caching via yt-dlp"
            }
        );
        premium
    }

    /// Read the plan from spclient rather than `api.spotify.com/v1/me`.
    pub async fn account_is_premium(&self) -> Result<bool> {
        let host = self.resolve_spclient_host().await;
        let url = format!("https://{host}/melody/v1/product_state");
        let body = self
            .send_authed_json("product-state", |http| http.get(&url))
            .await?;
        Ok(plan_is_premium(&body))
    }

    pub async fn get_current_track(&self) -> Result<SpotifyState> {
        let snapshot = self.fetch_connect_state_snapshot().await?;

        let Some(current_uri) = snapshot.current_track_uri.clone() else {
            return Ok(SpotifyState::default());
        };

        let metadata = self.fetch_track_metadata(&current_uri).await;
        let track = Self::build_track_from_uri(
            &current_uri,
            metadata.as_ref(),
            snapshot.current_track_fallback.as_ref(),
            snapshot.duration_ms,
        );

        // Skip ads / non-tracks and self-duplicates.
        let next_uri = snapshot.next_track_uri.as_deref().filter(|uri| {
            let id = track_id_from_uri(uri);
            !id.is_empty() && id != track.id
        });
        let next_track = match next_uri {
            Some(uri) => Some(Self::build_track_from_uri(
                uri,
                self.fetch_track_metadata(uri).await.as_ref(),
                snapshot.next_track_fallback.as_ref(),
                None,
            )),
            None => None,
        };

        // An active track cannot play past its duration. If extrapolated progress exceeds
        // duration + grace without a track transition, playback has finished or the device dropped.
        let (is_playing, progress) = if snapshot.is_playing
            && track.duration > 0
            && snapshot.progress_ms >= track.duration + 2000
        {
            (false, track.duration)
        } else {
            (snapshot.is_playing, snapshot.progress_ms)
        };

        Ok(SpotifyState {
            track: Some(track),
            next_track,
            progress,
            is_playing,
        })
    }

    fn build_track_from_uri(
        uri: &str,
        metadata: Option<&Track>,
        fallback: Option<&Track>,
        duration_hint: Option<i64>,
    ) -> Track {
        let mut track = metadata.or(fallback).cloned().unwrap_or_default();
        if let (Some(_), Some(fb)) = (metadata, fallback) {
            track.fill_from(fb.clone());
        }
        if track.id.is_empty() {
            track.id = track_id_from_uri(uri);
        }
        if track.name.trim().is_empty() {
            track.name = track.id.clone();
        }
        if track.artist.trim().is_empty() {
            track.artist = "Unknown Artist".into();
        }
        if track.duration <= 0
            && let Some(ms) = duration_hint
        {
            track.duration = ms.max(0);
        }
        track
    }

    async fn fetch_connect_state_snapshot(&self) -> Result<ConnectStateSnapshot> {
        let spclient_host = self.resolve_spclient_host().await;
        let endpoint = format!(
            "https://{spclient_host}/connect-state/v1/devices/{}",
            self.inner.connect_observer_device_id
        );

        let body = serde_json::json!({
            "member_type": "CONNECT_STATE",
            "device": {
                "device_info": {
                    "capabilities": {
                        "can_be_player": false,
                        "hidden": true,
                        "needs_full_player_state": true
                    }
                }
            }
        });

        // `.json()` sets Content-Type itself.
        let payload = self
            .send_authed_json("connect-state", |http| {
                http.put(&endpoint)
                    .header("X-Spotify-Connection-Id", &self.inner.connect_connection_id)
                    .json(&body)
            })
            .await?;

        parse_connect_state_snapshot(&payload)
            .ok_or_else(|| anyhow::anyhow!("Spotify connect-state response missing player_state"))
    }

    /// Send an authenticated web-player request and decode the JSON body, retrying once
    /// with a fresh token on 401. Shared by connect-state and track metadata so the auth,
    /// rate-limit and status handling exist in exactly one place.
    async fn send_authed_json(
        &self,
        what: &str,
        build: impl Fn(&reqwest::Client) -> reqwest::RequestBuilder,
    ) -> Result<Value> {
        let mut token = self.access_token().await?;

        for attempt in 0..2 {
            self.wait_for_rate_limit_window().await;

            let response = build(crate::http())
                .bearer_auth(&token)
                .header("Accept", "application/json")
                .header("Origin", "https://open.spotify.com")
                .header("Referer", "https://open.spotify.com/")
                .header("App-Platform", "WebPlayer")
                .header("Client-Id", SPOTIFY_CLIENT_ID)
                .send()
                .await?;

            if response.status() == StatusCode::UNAUTHORIZED && attempt == 0 {
                self.clear_cached_token().await;
                config::invalidate_spotify_access_token();
                token = self.access_token().await?;
                continue;
            }

            if response.status() == StatusCode::TOO_MANY_REQUESTS {
                // Honour Retry-After for every caller via the shared cooldown.
                let secs = response
                    .headers()
                    .get("retry-after")
                    .and_then(|v| v.to_str().ok()?.parse::<u64>().ok())
                    .unwrap_or(MIN_RATE_LIMIT_COOLDOWN_SECS)
                    .clamp(MIN_RATE_LIMIT_COOLDOWN_SECS, 120);
                *self.inner.rate_limit_until.lock().await =
                    Some(Instant::now() + Duration::from_secs(secs));
                bail!("Spotify {what} rate limited; retry-after={secs}s");
            }

            if !response.status().is_success() {
                bail!(
                    "Spotify {what} request failed with status {}",
                    response.status()
                );
            }

            *self.inner.rate_limit_until.lock().await = None;
            return Ok(response.json().await?);
        }

        bail!("Spotify {what} request failed after retries")
    }

    async fn resolve_spclient_host(&self) -> String {
        {
            let cache = self.inner.spclient_host_cache.lock().await;
            if let Some((host, expires)) = &*cache
                && Instant::now() < *expires
            {
                return host.clone();
            }
        }

        let resolved = async {
            let resp = crate::http().get(APRESOLVE_URL).send().await.ok()?;
            let body: ApresolveResponse = resp.error_for_status().ok()?.json().await.ok()?;
            body.spclient
                .into_iter()
                .find(|host| !host.trim().is_empty())
        };
        let host = resolved
            .await
            .unwrap_or_else(|| SPCLIENT_DEFAULT_HOST.to_string());
        let expires = Instant::now() + Duration::from_secs(SPCLIENT_CACHE_SECS);
        *self.inner.spclient_host_cache.lock().await = Some((host.clone(), expires));
        host
    }

    /// Cached metadata for one track URI.
    async fn fetch_track_metadata(&self, uri: &str) -> Option<Track> {
        if uri.is_empty() {
            return None;
        }

        {
            let cache = self.inner.track_metadata_cache.lock().await;
            if let Some((cached, retry)) = cache.get(uri)
                && retry.is_none_or(|at| Instant::now() < at)
            {
                return cached.clone();
            }
        }

        // A failure is kept only briefly, so one hiccup doesn't stick for good.
        let (track, retry) = match self.fetch_track_metadata_from_spclient(uri).await {
            Ok(track) => (track, None),
            Err(err) => {
                tracing::debug!("Spotify track metadata lookup failed for {uri}: {err}");
                let retry = Instant::now() + Duration::from_secs(TRACK_METADATA_RETRY_SECS);
                (None, Some(retry))
            }
        };

        let mut cache = self.inner.track_metadata_cache.lock().await;
        // Cleared wholesale at the cap.
        if cache.len() > TRACK_METADATA_CACHE_MAX_ENTRIES {
            cache.clear();
        }
        cache.insert(uri.to_string(), (track.clone(), retry));
        track
    }

    /// spclient's track record: every artist, album artists, release date and covers.
    async fn fetch_track_metadata_from_spclient(&self, uri: &str) -> Result<Option<Track>> {
        let id = track_id_from_uri(uri);
        let gid = librespot_core::SpotifyId::from_base62(&id)?.to_base16()?;
        let host = self.resolve_spclient_host().await;
        let payload = self
            .send_authed_json("track metadata", |http| {
                // Without it, artists come back in their native script ("結束バンド", not
                // "kessoku band").
                http.get(format!("https://{host}/metadata/4/track/{gid}"))
                    .query(&[("market", "from_token")])
                    .header("Accept-Language", "en")
            })
            .await?;
        Ok(parse_metadata_track(&payload, &id))
    }

    async fn wait_for_rate_limit_window(&self) {
        let until = *self.inner.rate_limit_until.lock().await;
        if let Some(remaining) = until.and_then(|u| u.checked_duration_since(Instant::now())) {
            tokio::time::sleep(remaining).await;
        }
    }
}

/// librespot exits the whole process unless the account's `type` is exactly "premium", so
/// when product state carries a `type` too, it has to say so as well.
fn plan_is_premium(state: &serde_json::Value) -> bool {
    let field = |k: &str| state.get(k).and_then(serde_json::Value::as_str);
    field("product").is_some_and(|p| p.eq_ignore_ascii_case("premium"))
        && field("type").is_none_or(|t| t == "premium")
}
