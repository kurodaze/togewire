pub mod auth;
pub mod stream;
pub mod websocket;
pub mod workers;

use axum::{
    Form, Json, Router,
    body::{Body, Bytes},
    extract::{ConnectInfo, Path, Query, State, ws::Utf8Bytes, ws::WebSocketUpgrade},
    http::{HeaderMap, HeaderValue, Method, Request, StatusCode, header},
    middleware::{self, Next},
    response::{Html, IntoResponse, Redirect, Response},
    routing::{get, post},
};
use futures_util::{Stream, StreamExt, stream::BoxStream};
use parking_lot::{Mutex, RwLock};
use rust_embed::Embed;
use serde::Deserialize;
use serde_json::json;
use std::collections::HashMap;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::sync::{Arc, LazyLock};
use std::task::{Context, Poll};
use std::time::Duration;
use tera::Tera;
use tokio::sync::mpsc;
use tokio_stream::wrappers::BroadcastStream;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use crate::audio::TrackPreparer;
use crate::audio::manual_download::{ManualDownloadManager, ManualDownloadSource};
use crate::cache::Manager as CacheManager;
use crate::config;
use crate::spotify::SpotifyClient;
use crate::types::SpotifyState;
use stream::StreamManager;

const MAX_CONNECTIONS_PER_IP: usize = 10;
const MAX_TOTAL_STREAM_LISTENERS: usize = 250;
/// Per-IP caps alone do not bound WebSockets from many addresses.
const MAX_TOTAL_WS_CLIENTS: usize = 1_000;
const MAX_STREAM_CONNECTION_DURATION: Duration = Duration::from_hours(6);
const MIN_ADMIN_PASSWORD_LEN: usize = 8;
/// Clients only ever send the tiny `listening` frame.
const MAX_WS_MESSAGE_SIZE: usize = 512;

#[derive(Embed)]
#[folder = "web/"]
struct Asset;

#[derive(Clone)]
pub struct AppState {
    /// Latest Spotify poll; `None` while nothing is playing.
    pub current: Arc<RwLock<Option<SpotifyState>>>,
    pub spotify_client: SpotifyClient,
    pub stream_manager: StreamManager,
    pub preparer: Arc<TrackPreparer>,
    pub manual_downloads: Arc<ManualDownloadManager>,
    pub ws_clients: Arc<RwLock<HashMap<u64, mpsc::Sender<Utf8Bytes>>>>,
    pub tera: Arc<Tera>,
    pub shutdown: CancellationToken,
}

impl AppState {
    #[must_use]
    pub fn current(&self) -> Option<SpotifyState> {
        self.current.read().clone()
    }
}

pub fn create_server() -> (Router, AppState) {
    let spotify_client = SpotifyClient::default();
    let cache = Arc::new(CacheManager::new(config::get().read().cache_max_mb));
    let state = AppState {
        current: Arc::default(),
        stream_manager: StreamManager::new(Arc::clone(&cache)),
        preparer: Arc::new(TrackPreparer::new(spotify_client.clone(), cache)),
        manual_downloads: Arc::default(),
        spotify_client,
        ws_clients: Arc::default(),
        tera: Arc::new(load_templates()),
        shutdown: CancellationToken::new(),
    };
    (build_router(state.clone()), state)
}

fn load_templates() -> Tera {
    let mut tera = Tera::default();
    for name in ["login.html", "dashboard.html"] {
        let file = Asset::get(&format!("templates/{name}")).expect("embedded template");
        let src = std::str::from_utf8(&file.data).expect("template is UTF-8");
        tera.add_raw_template(name, src).expect("template parses");
    }
    tera
}

/// Per-IP connection counts, one entry while anything from that IP is open.
#[derive(Default)]
struct Peer {
    streams: usize,
    sockets: usize,
    listening: usize,
    last_track: String,
}

static PEERS: LazyLock<Mutex<HashMap<String, Peer>>> = LazyLock::new(Mutex::default);

fn with_peer<R>(ip: &str, f: impl FnOnce(&mut Peer) -> R) -> R {
    let mut peers = PEERS.lock();
    let peer = peers.entry(ip.to_string()).or_default();
    let out = f(peer);
    if peer.streams == 0 && peer.sockets == 0 && peer.listening == 0 {
        peers.remove(ip);
    }
    out
}

fn take_slot(count: &mut usize) -> bool {
    let free = *count < MAX_CONNECTIONS_PER_IP;
    *count += usize::from(free);
    free
}

fn build_router(state: AppState) -> Router {
    let admin = Router::new()
        .route("/admin", get(handle_dashboard))
        .route("/admin/pair/start", post(handle_admin_pair_start))
        .route("/admin/pair/poll", post(handle_admin_pair_poll))
        .route("/admin/disconnect", post(handle_admin_disconnect))
        .route("/admin/player", post(handle_admin_player))
        .route("/admin/cache", post(handle_admin_cache))
        .route("/admin/togematch", post(handle_admin_togematch))
        .route("/admin/cache/list", get(handle_admin_cache_list))
        .route(
            "/admin/cache/delete-many",
            post(handle_admin_cache_delete_many),
        )
        .route("/admin/cache/download", post(handle_admin_cache_download))
        .route("/admin/cache/{id}/file", get(handle_admin_cache_file))
        .route("/admin/cache/{id}/cover", get(handle_admin_cache_cover))
        .route("/admin/cache/{id}/delete", post(handle_admin_cache_delete))
        .route(
            "/admin/download",
            get(handle_admin_manual_download_current).post(handle_admin_manual_download_start),
        )
        .route(
            "/admin/download/{id}/pause",
            post(handle_admin_manual_download_pause),
        )
        .route(
            "/admin/download/{id}/resume",
            post(handle_admin_manual_download_resume),
        )
        .route(
            "/admin/download/{id}/stop",
            post(handle_admin_manual_download_stop),
        )
        .route(
            "/admin/download/{id}/cancel",
            post(handle_admin_manual_download_cancel),
        )
        .route(
            "/admin/download/{id}/file",
            get(handle_admin_manual_download_file),
        )
        .route_layer(middleware::from_fn(require_admin));
    Router::new()
        .route("/", get(handle_root))
        .route("/player", get(handle_player))
        .route("/ping", get(handle_ping))
        .route("/stream", get(handle_stream))
        .route("/ws", get(handle_ws))
        .route(
            "/admin/login",
            get(handle_admin_login).post(handle_admin_login_post),
        )
        .route("/static/{*path}", get(handle_static))
        .merge(admin)
        .layer(middleware::from_fn(security_headers))
        .with_state(state)
}

async fn security_headers(req: Request<Body>, next: Next) -> Response {
    let path = req.uri().path();
    let (ping, admin) = (path == "/ping", path.starts_with("/admin"));
    let mut response = next.run(req).await;
    let headers = response.headers_mut();
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("strict-origin-when-cross-origin"),
    );
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(
            "default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'self'; \
             media-src 'self'; img-src 'self' data:; font-src 'self'; form-action 'self'; \
             base-uri 'self'; frame-ancestors 'self'",
        ),
    );
    if ping {
        // Simple GET, so no preflight: Allow-Origin alone is what a cross-origin caller needs.
        headers.insert(
            header::ACCESS_CONTROL_ALLOW_ORIGIN,
            HeaderValue::from_static("*"),
        );
    } else {
        headers.insert(
            header::X_FRAME_OPTIONS,
            HeaderValue::from_static("SAMEORIGIN"),
        );
    }
    if admin {
        // Admin pages show tokens; keep them out of browser and proxy caches.
        headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    }
    response
}

/// Admin routes: cross-site POSTs are refused (SameSite=Lax is the primary guard), then
/// a valid session is required.
async fn require_admin(req: Request<Body>, next: Next) -> Response {
    if req.method() == Method::POST && !same_origin_post(req.headers()) {
        return reject_cross_site();
    }
    if !is_admin(req.headers()) {
        return Redirect::to("/admin/login").into_response();
    }
    next.run(req).await
}

async fn handle_static(Path(path): Path<String>) -> Response {
    let Some(file) = Asset::get(&format!("static/{path}")) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    // Everything under web/static/ is one of these; anything else downloads, never sniffed.
    let mime = match path.rsplit('.').next() {
        Some("css") => "text/css",
        Some("js") => "text/javascript",
        Some("woff2") => "font/woff2",
        Some("txt") => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    };
    ([(header::CONTENT_TYPE, mime)], file.data).into_response()
}

fn session_cookie(headers: &HeaderMap) -> Option<&str> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .find_map(|c| {
            c.trim()
                .strip_prefix(auth::SESSION_COOKIE)?
                .strip_prefix('=')
        })
}

fn is_admin(headers: &HeaderMap) -> bool {
    let cfg = config::get().read();
    session_cookie(headers).is_some_and(|t| auth::is_valid_session(&cfg.admin_password_hash, t))
}

fn is_request_secure(headers: &HeaderMap, peer: SocketAddr) -> bool {
    is_trusted_proxy(peer.ip())
        && headers
            .get("x-forwarded-proto")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|proto| proto.trim() == "https")
}

/// Forwarded headers are trusted only from a private or loopback peer: a local reverse
/// proxy (cloudflared, KDNX or its connector, whose tunnels default to 10.8.0.0/24 and
/// fd42::/64).
fn is_trusted_proxy(ip: IpAddr) -> bool {
    match ip.to_canonical() {
        IpAddr::V4(v4) => v4.is_loopback() || v4.is_private(),
        IpAddr::V6(v6) => v6.is_loopback() || v6.is_unique_local(),
    }
}

/// The key rate limits and connection caps count by. IPv6 counts per /64: one client
/// holds a whole prefix, so a per-address limit would limit nothing.
fn client_ip(headers: &HeaderMap, peer: SocketAddr) -> String {
    let ip = is_trusted_proxy(peer.ip())
        .then(|| forwarded_client_ip(headers))
        .flatten()
        .unwrap_or(peer.ip())
        .to_canonical();
    match ip {
        IpAddr::V6(v6) => {
            let s = v6.segments();
            format!("{:x}:{:x}:{:x}:{:x}::/64", s[0], s[1], s[2], s[3])
        }
        IpAddr::V4(v4) => v4.to_string(),
    }
}

/// The client in X-Forwarded-For, which KDNX sets and cloudflared appends to. Each proxy
/// appends the address it saw, so walk back from the right past local hops: the first
/// public entry is the one a trusted proxy added, anything left of it the client's claim.
fn forwarded_client_ip(headers: &HeaderMap) -> Option<IpAddr> {
    let hops = headers
        .get_all("x-forwarded-for")
        .iter()
        .rev()
        .flat_map(|v| v.to_str().unwrap_or_default().rsplit(','))
        .map_while(|hop| hop.trim().parse::<IpAddr>().ok());
    let mut client = None;
    for ip in hops {
        client = Some(ip);
        if !is_trusted_proxy(ip) {
            break;
        }
    }
    client
}

/// Origin (else Referer) must name this host.
fn same_origin_post(headers: &HeaderMap) -> bool {
    let get = |name| {
        headers
            .get(name)
            .and_then(|v: &HeaderValue| v.to_str().ok())
    };
    let Some(host) = get(header::HOST) else {
        return false;
    };
    get(header::ORIGIN)
        .or_else(|| get(header::REFERER))
        .and_then(host_from_absolute_url)
        .is_some_and(|h| h.eq_ignore_ascii_case(host))
}

fn host_from_absolute_url(url: &str) -> Option<&str> {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))?;
    rest.split(['/', '?', '#'])
        .next()
        .filter(|host| !host.is_empty())
}

fn reject_cross_site() -> Response {
    (StatusCode::FORBIDDEN, "cross-site request blocked").into_response()
}

fn render(state: &AppState, name: &str, ctx: &tera::Context) -> Response {
    match state.tera.render(name, ctx) {
        Ok(html) => Html(html).into_response(),
        Err(e) => {
            error!("Template {name}: {e}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

fn player_disabled() -> Option<Response> {
    let cfg = config::get().read();
    (!cfg.player_enabled || !cfg.is_authenticated())
        .then(|| (StatusCode::SERVICE_UNAVAILABLE, "player disabled").into_response())
}

async fn handle_root() -> Redirect {
    let cfg = config::get().read();
    Redirect::to(if !cfg.has_admin_password() {
        "/admin/login"
    } else if cfg.player_enabled && cfg.is_authenticated() {
        "/player"
    } else {
        "/admin"
    })
}

async fn handle_dashboard(State(state): State<AppState>) -> Response {
    let mut ctx = tera::Context::new();
    {
        let cfg = config::get().read();
        let authenticated = cfg.is_authenticated();
        ctx.insert("authenticated", &authenticated);
        ctx.insert("player_enabled", &(cfg.player_enabled && authenticated));
        ctx.insert("use_librespot", &config::use_librespot());
        let plan = match config::premium() {
            Some(true) => "premium",
            Some(false) => "free",
            None => "unknown",
        };
        ctx.insert("plan", plan);
        ctx.insert("cache_enabled", &cfg.cache_enabled);
        ctx.insert("audio_source", &cfg.audio_source);
        ctx.insert("audio_bitrate", &config::audio_bitrate());
        ctx.insert("low_data_streaming", &cfg.low_data_streaming);
        ctx.insert("normalization", &cfg.normalization);
        ctx.insert("togematch_api", &cfg.togematch_api);
    }

    let (used_bytes, max_bytes, entries) = state.preparer.cache.stats();
    ctx.insert(
        "cache_used_mb",
        &format!("{:.1}", used_bytes as f64 / (1024.0 * 1024.0)),
    );
    ctx.insert("cache_max_mb", &(max_bytes / (1024 * 1024)));
    ctx.insert("cache_entries", &entries);

    render(&state, "dashboard.html", &ctx)
}

async fn handle_player() -> Response {
    if let Some(off) = player_disabled() {
        return off;
    }
    Html(
        Asset::get("templates/player.html")
            .map(|f| f.data)
            .unwrap_or_default(),
    )
    .into_response()
}

async fn handle_ping(State(state): State<AppState>) -> Json<serde_json::Value> {
    let now = crate::now_unix();
    // Playback is only public while the player is on and Spotify is paired; otherwise a health check.
    {
        let cfg = config::get().read();
        if !cfg.player_enabled || !cfg.is_authenticated() {
            return Json(json!({ "status": "ok", "timestamp": now }));
        }
    }
    let current = state.current();
    let track = current.as_ref().and_then(|s| s.track.as_ref());
    Json(json!({
        "status": "ok",
        "timestamp": now,
        "is_playing": current.as_ref().is_some_and(|s| s.is_playing),
        "has_track": track.is_some(),
        "current_track": track,
    }))
}

async fn handle_stream(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Response {
    if let Some(off) = player_disabled() {
        return off;
    }
    let ip = client_ip(&headers, peer);
    if !with_peer(&ip, |p| take_slot(&mut p.streams)) {
        return (StatusCode::TOO_MANY_REQUESTS, "too many stream connections").into_response();
    }
    let Some((rx, prefix)) = state.stream_manager.add_client(MAX_TOTAL_STREAM_LISTENERS) else {
        with_peer(&ip, |p| p.streams -= 1);
        return (StatusCode::SERVICE_UNAVAILABLE, "too many listeners").into_response();
    };
    workers::rebroadcast(&state);
    if let Some(track) = state.current().and_then(|s| s.track) {
        with_peer(&ip, |p| {
            if p.last_track != track.id {
                info!("Server now streaming: {} - {}", track.artist, track.name);
                p.last_track = track.id;
            }
        });
    }

    // Cached headers + burst first so playback starts instantly; lagged pages are skipped.
    let shutdown = state.shutdown.clone();
    let body = futures_util::stream::iter(prefix)
        .chain(BroadcastStream::new(rx).filter_map(|r| std::future::ready(r.ok())))
        .map(Ok)
        .take_until(async move {
            tokio::select! {
                () = tokio::time::sleep(MAX_STREAM_CONNECTION_DURATION) => {}
                () = shutdown.cancelled() => {}
            }
        })
        .boxed();
    let body = ListenerStream {
        inner: body,
        state,
        ip,
    };
    (
        [
            (header::CONTENT_TYPE, stream::CONTENT_TYPE),
            (header::CACHE_CONTROL, "no-cache, no-store, must-revalidate"),
        ],
        Body::from_stream(body),
    )
        .into_response()
}

/// A `/stream` body; dropping it (client gone, or its pipe retired) releases the listener.
struct ListenerStream {
    inner: BoxStream<'static, io::Result<Bytes>>,
    state: AppState,
    ip: String,
}

impl Stream for ListenerStream {
    type Item = io::Result<Bytes>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.inner.poll_next_unpin(cx)
    }
}

impl Drop for ListenerStream {
    fn drop(&mut self) {
        self.state.stream_manager.remove_client();
        with_peer(&self.ip, |p| p.streams = p.streams.saturating_sub(1));
        workers::rebroadcast(&self.state);
    }
}

async fn handle_ws(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    ws: WebSocketUpgrade,
    headers: HeaderMap,
) -> Response {
    // Same gate as /player and /stream: track_update carries what is playing right now.
    if let Some(off) = player_disabled() {
        return off;
    }
    let ip = client_ip(&headers, peer);
    if state.ws_clients.read().len() >= MAX_TOTAL_WS_CLIENTS
        || !with_peer(&ip, |p| take_slot(&mut p.sockets))
    {
        return (StatusCode::TOO_MANY_REQUESTS, "too many connections").into_response();
    }
    let failed_ip = ip.clone();
    ws.max_message_size(MAX_WS_MESSAGE_SIZE)
        .max_frame_size(MAX_WS_MESSAGE_SIZE)
        .on_failed_upgrade(move |_| with_peer(&failed_ip, |p| p.sockets -= 1))
        .on_upgrade(move |socket| websocket::handle_socket(state, socket, ip))
}

async fn handle_admin_login(State(state): State<AppState>) -> Response {
    login_page(&state, first_login_step(), "", "")
}

fn first_login_step() -> &'static str {
    if config::get().read().has_admin_password() {
        "login"
    } else {
        "token"
    }
}

/// Errors render in place, not via a redirect, so no link can put its own text here.
/// `step` is "login", or "token" → "password" → "confirm" during first-time setup.
fn login_page(state: &AppState, step: &str, setup_token: &str, error: &str) -> Response {
    let mut ctx = tera::Context::new();
    ctx.insert("step", step);
    ctx.insert("setup_token", setup_token);
    ctx.insert("error", error);
    render(state, "login.html", &ctx)
}

#[derive(Deserialize)]
struct LoginForm {
    #[serde(default)]
    password: String,
    #[serde(default)]
    confirm: String,
    #[serde(default)]
    setup_token: String,
}

async fn handle_admin_login_post(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Form(form): Form<LoginForm>,
) -> Response {
    if !same_origin_post(&headers) {
        return reject_cross_site();
    }
    let ip = client_ip(&headers, peer);
    if !auth::register_login_attempt(&ip) {
        return login_page(
            &state,
            first_login_step(),
            "",
            "Too many attempts. Try again later",
        );
    }

    let admin_hash = config::get().read().admin_password_hash.clone();
    if admin_hash.is_empty() {
        // First-time setup: each step re-sends the token, and only the field it asked for.
        let token = form.setup_token.as_str();
        if !auth::is_setup_token_valid(token) {
            return login_page(
                &state,
                "token",
                "",
                "Invalid setup token. Check terminal output",
            );
        }
        // Only wrong tokens count toward the limit, so password typos cannot lock setup out.
        auth::clear_failed_attempts(&ip);
        if form.password.is_empty() && form.confirm.is_empty() {
            return login_page(&state, "password", token, "");
        }
        if form.confirm.is_empty() {
            if form.password.chars().count() < MIN_ADMIN_PASSWORD_LEN {
                return login_page(
                    &state,
                    "password",
                    token,
                    "Password must be at least 8 characters",
                );
            }
            // bcrypt ignores everything past 72 bytes.
            if form.password.len() > 72 {
                return login_page(
                    &state,
                    "password",
                    token,
                    "Password must be at most 72 bytes",
                );
            }
            let Some(hash) = auth::hash_password(form.password).await else {
                return login_page(&state, "password", token, "Failed to set password");
            };
            auth::set_pending_hash(hash);
            return login_page(&state, "confirm", token, "");
        }
        let hash = auth::take_pending_hash();
        if hash.is_empty() || !auth::verify_password(hash.clone(), form.confirm).await {
            return login_page(&state, "password", token, "Passwords do not match");
        }
        if config::update(|cfg| cfg.admin_password_hash = hash).is_err() {
            return login_page(&state, "password", token, "Failed to save password");
        }
        auth::clear_setup_token();
        info!("Admin password configured");
    } else if !auth::verify_password(admin_hash, form.password).await {
        return login_page(&state, "login", "", "Invalid password");
    }

    auth::clear_failed_attempts(&ip);
    info!("Admin logged in from {ip}");
    let token = auth::session_token(&config::get().read().admin_password_hash);
    let cookie = auth::session_cookie(is_request_secure(&headers, peer), &token);
    ([(header::SET_COOKIE, cookie)], Redirect::to("/admin")).into_response()
}

async fn handle_admin_pair_start(State(state): State<AppState>) -> Response {
    match state.spotify_client.request_device_code().await {
        Ok(device_auth) => Json(device_auth).into_response(),
        Err(e) => (StatusCode::BAD_GATEWAY, Json(json!({ "error": e }))).into_response(),
    }
}

#[derive(Deserialize)]
struct PollRequest {
    device_code: String,
}

async fn handle_admin_pair_poll(
    State(state): State<AppState>,
    Json(req): Json<PollRequest>,
) -> Json<serde_json::Value> {
    let paired = match state
        .spotify_client
        .poll_device_token(&req.device_code)
        .await
    {
        Ok(Ok(token)) => state.spotify_client.complete_pairing(&token).await,
        Ok(Err(pending)) => return Json(json!({ "status": pending })),
        Err(e) => Err(e),
    };
    Json(match paired {
        Ok(premium) => {
            info!("Spotify paired successfully (premium={premium})");
            json!({ "status": "paired" })
        }
        Err(e) => {
            error!("Spotify pairing failed: {e}");
            json!({ "status": "error", "message": e })
        }
    })
}

fn stop_player(state: &AppState) {
    state.stream_manager.stop();
    state.preparer.cache.clear_mem();
    // Sockets opened while the player was on would otherwise keep receiving
    // track_update; the upgrade gate only covers new connections.
    state.ws_clients.write().clear();
}

async fn handle_admin_disconnect(State(state): State<AppState>) -> Redirect {
    state.spotify_client.clear_session().await;
    state.preparer.reset_librespot().await;
    config::set_use_librespot(false);
    config::set_premium(None);
    if config::get().read().player_enabled {
        if let Err(e) = config::update(|cfg| cfg.player_enabled = false) {
            error!("Failed to disable player on disconnect: {e}");
        } else {
            info!("Player disabled");
        }
        stop_player(&state);
    }
    info!("Spotify disconnected");
    Redirect::to("/admin?n=disconnected")
}

#[derive(Deserialize)]
struct CacheForm {
    /// A checkbox: sent only when on.
    cache_enabled: Option<String>,
    /// Sent only when Premium + librespot are available.
    audio_source: Option<config::AudioSource>,
    cache_max_mb: u64,
    audio_bitrate: String,
    normalization: config::Normalization,
    /// A checkbox: sent only when on.
    low_data_streaming: Option<String>,
}

async fn handle_admin_cache(
    State(state): State<AppState>,
    Form(form): Form<CacheForm>,
) -> Redirect {
    if !config::AUDIO_BITRATES.contains(&form.audio_bitrate.as_str()) {
        return Redirect::to("/admin?n=bad_bitrate");
    }
    let mb = form.cache_max_mb.max(64);
    let cache_enabled = form.cache_enabled.is_some();
    let low_data_streaming = form.low_data_streaming.is_some();
    let prev = {
        let cfg = config::get().read();
        (
            cfg.cache_enabled,
            cfg.audio_source,
            cfg.cache_max_mb,
            cfg.audio_bitrate.clone(),
            cfg.normalization,
            cfg.low_data_streaming,
        )
    };
    let next_source = form.audio_source.unwrap_or(prev.1);
    let saved = config::update(|cfg| {
        cfg.cache_enabled = cache_enabled;
        cfg.audio_source = next_source;
        cfg.cache_max_mb = mb;
        cfg.audio_bitrate = form.audio_bitrate.clone();
        cfg.normalization = form.normalization;
        cfg.low_data_streaming = low_data_streaming;
    });
    if let Err(e) = saved {
        error!("Failed to save settings: {e}");
        return Redirect::to("/admin?n=save_failed");
    }
    if cache_enabled != prev.0 {
        info!(
            "Disk cache {}",
            if cache_enabled { "enabled" } else { "disabled" }
        );
    }
    if next_source != prev.1 {
        let label = match next_source {
            config::AudioSource::Spotify => "Spotify",
            config::AudioSource::Youtube => "YouTube",
        };
        info!("Audio source set to {label}");
    }
    if form.audio_bitrate != prev.3 {
        info!("Audio quality set to {}", form.audio_bitrate);
    }
    if form.normalization != prev.4 {
        let label = match form.normalization {
            config::Normalization::Off => "off",
            config::Normalization::Stream => "stream",
            config::Normalization::Cache => "cache",
        };
        info!("Normalization set to {label}");
    }
    if low_data_streaming != prev.5 {
        info!(
            "Low data streaming {}",
            if low_data_streaming {
                "enabled"
            } else {
                "disabled"
            }
        );
    }
    if mb != prev.2 {
        info!("Cache limit set to {mb} MB");
    }
    state.preparer.cache.set_max_mb(mb);
    Redirect::to("/admin?n=saved")
}

#[derive(Deserialize)]
struct TogematchForm {
    #[serde(default)]
    togematch_api: String,
}

fn is_valid_http_base_url(raw: &str) -> bool {
    reqwest::Url::parse(raw).is_ok_and(|u| {
        matches!(u.scheme(), "http" | "https")
            && u.has_host()
            && u.username().is_empty()
            && u.password().is_none()
            && u.query().is_none()
            && u.fragment().is_none()
    })
}

async fn handle_admin_togematch(Form(form): Form<TogematchForm>) -> Redirect {
    let togematch_api = form.togematch_api.trim().trim_end_matches('/').to_string();
    if !(togematch_api.is_empty() || is_valid_http_base_url(&togematch_api)) {
        warn!("Rejected invalid togematch_api address: {togematch_api:?}");
        return Redirect::to("/admin?n=togematch_failed");
    }
    let prev = config::get().read().togematch_api.clone();
    if let Err(e) = config::update(|cfg| cfg.togematch_api = togematch_api.clone()) {
        error!("Failed to save togematch_api: {e}");
        return Redirect::to("/admin?n=save_failed");
    }
    if togematch_api != prev {
        if togematch_api.is_empty() {
            info!("togematch-api cleared");
        } else {
            info!("togematch-api set to {togematch_api}");
        }
    }
    if togematch_api.is_empty() {
        return Redirect::to("/admin?n=saved");
    }
    if probe_togematch_api(&togematch_api).await {
        Redirect::to("/admin?n=togematch_ok")
    } else {
        Redirect::to("/admin?n=togematch_failed")
    }
}

/// Probes `base/match?q=` to verify reachability, ACLs, and that the target is togematch-api
/// without triggering any upstream lookup.
async fn probe_togematch_api(base: &str) -> bool {
    let resp = match crate::http()
        .get(format!("{base}/match"))
        .query(&[("q", "")])
        .timeout(Duration::from_secs(5))
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            warn!("togematch-api probe failed for {base}: {e}");
            return false;
        }
    };
    let status = resp.status();
    let body_json = resp.json::<serde_json::Value>().await.ok();
    let err_msg = body_json
        .as_ref()
        .and_then(|v| v.get("error"))
        .and_then(serde_json::Value::as_str);
    if status == StatusCode::BAD_REQUEST && err_msg == Some("q is empty") {
        true
    } else {
        warn!(
            "togematch-api probe unexpected response from {base}: HTTP {} ({err_msg:?})",
            status.as_u16()
        );
        false
    }
}

#[derive(Deserialize)]
struct PlayerBody {
    enabled: bool,
}

async fn handle_admin_player(
    State(state): State<AppState>,
    Json(body): Json<PlayerBody>,
) -> Response {
    if body.enabled && !config::get().read().is_authenticated() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "Pair with Spotify before turning the player on" })),
        )
            .into_response();
    }
    let prev_enabled = config::get().read().player_enabled;
    if let Err(e) = config::update(|cfg| cfg.player_enabled = body.enabled) {
        error!("Failed to save player_enabled: {e}");
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    if body.enabled {
        workers::start_stream_if_playing(&state);
    } else {
        stop_player(&state);
    }
    if body.enabled != prev_enabled {
        info!(
            "Player {}",
            if body.enabled { "enabled" } else { "disabled" }
        );
    }
    Json(json!({ "enabled": body.enabled })).into_response()
}

async fn handle_admin_cache_list(State(state): State<AppState>) -> Response {
    let items = state.preparer.cache.list();
    let (used, _, entries) = state.preparer.cache.stats();
    Json(json!({
        "items": items,
        "used_mb": format!("{:.1}", used as f64 / (1024.0 * 1024.0)),
        "entries": entries,
    }))
    .into_response()
}

#[derive(Deserialize)]
struct CacheFileQuery {
    #[serde(default)]
    inline: Option<String>,
}

/// Parse a single HTTP byte range (`bytes=start-end`, `bytes=start-`, or `bytes=-suffix`).
fn parse_byte_range(raw: &str, total: u64) -> Option<(u64, u64)> {
    if total == 0 {
        return None;
    }
    let spec = raw.trim().strip_prefix("bytes=")?;
    if spec.contains(',') {
        return None;
    }
    let (start_s, end_s) = spec.split_once('-')?;
    if start_s.is_empty() {
        let suffix: u64 = end_s.parse().ok()?;
        if suffix == 0 {
            return None;
        }
        let start = total.saturating_sub(suffix);
        return Some((start, total - 1));
    }
    let start: u64 = start_s.parse().ok()?;
    if start >= total {
        return None;
    }
    let end = if end_s.is_empty() {
        total - 1
    } else {
        end_s.parse::<u64>().ok()?.min(total - 1)
    };
    (start <= end).then_some((start, end))
}

async fn handle_admin_cache_file(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<CacheFileQuery>,
    req_headers: HeaderMap,
) -> Response {
    use tokio::io::{AsyncReadExt, AsyncSeekExt};

    let Some((path, file_name)) = state.preparer.cache.get_disk_entry(&id) else {
        return (StatusCode::NOT_FOUND, "cached track not found").into_response();
    };
    let mut file = match tokio::fs::File::open(&path).await {
        Ok(f) => f,
        Err(_) => return StatusCode::NOT_FOUND.into_response(),
    };
    let total = file.metadata().await.ok().map(|m| m.len());
    let kind = if query.inline.is_some() {
        "inline"
    } else {
        "attachment"
    };
    let disposition = content_disposition(kind, &file_name);

    if let Some(total) = total
        && let Some(range_val) = req_headers.get(header::RANGE).and_then(|v| v.to_str().ok())
    {
        let Some((start, end)) = parse_byte_range(range_val, total) else {
            let mut resp = StatusCode::RANGE_NOT_SATISFIABLE.into_response();
            if let Ok(val) = HeaderValue::from_str(&format!("bytes */{total}")) {
                resp.headers_mut().insert(header::CONTENT_RANGE, val);
            }
            return resp;
        };
        if file.seek(io::SeekFrom::Start(start)).await.is_err() {
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
        let slice_len = end - start + 1;
        let stream = tokio_util::io::ReaderStream::new(file.take(slice_len));
        let mut resp = Response::new(Body::from_stream(stream));
        *resp.status_mut() = StatusCode::PARTIAL_CONTENT;
        let headers = resp.headers_mut();
        headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("audio/ogg"));
        headers.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
        if let Ok(val) = HeaderValue::from_str(&disposition) {
            headers.insert(header::CONTENT_DISPOSITION, val);
        }
        if let Ok(val) = HeaderValue::from_str(&format!("bytes {start}-{end}/{total}")) {
            headers.insert(header::CONTENT_RANGE, val);
        }
        if let Ok(val) = HeaderValue::from_str(&slice_len.to_string()) {
            headers.insert(header::CONTENT_LENGTH, val);
        }
        return resp;
    }

    let stream = tokio_util::io::ReaderStream::new(file);
    let mut resp = Response::new(Body::from_stream(stream));
    let headers = resp.headers_mut();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("audio/ogg"));
    headers.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    if let Ok(val) = HeaderValue::from_str(&disposition) {
        headers.insert(header::CONTENT_DISPOSITION, val);
    }
    if let Some(size) = total
        && let Ok(val) = HeaderValue::from_str(&size.to_string())
    {
        headers.insert(header::CONTENT_LENGTH, val);
    }
    resp
}

async fn handle_admin_cache_cover(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Response {
    let Some((path, file_name)) = state.preparer.cache.get_disk_entry(&id) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let extracted = tokio::task::spawn_blocking(move || crate::audio::extract_ogg_cover(&path))
        .await
        .ok()
        .flatten();
    let Some((mime, bytes)) = extracted else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let ext = match mime {
        "image/png" => "png",
        "image/webp" => "webp",
        _ => "jpg",
    };
    let stem = std::path::Path::new(&file_name)
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "cover".into());
    let cover_name = format!("{stem}.{ext}");
    let disposition = content_disposition("inline", &cover_name);
    let len = bytes.len();
    let mut resp = Response::new(Body::from(bytes));
    let headers = resp.headers_mut();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(mime));
    if let Ok(val) = HeaderValue::from_str(&disposition) {
        headers.insert(header::CONTENT_DISPOSITION, val);
    }
    if let Ok(val) = HeaderValue::from_str(&len.to_string()) {
        headers.insert(header::CONTENT_LENGTH, val);
    }
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, max-age=3600"),
    );
    resp
}

async fn handle_admin_cache_delete(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Response {
    let label = state
        .preparer
        .cache
        .get_disk_entry(&id)
        .and_then(|(path, _)| {
            path.file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .filter(|s| !s.is_empty())
        })
        .unwrap_or_else(|| id.clone());
    let removed = state.preparer.cache.delete(&id);
    if removed {
        info!("Cached track deleted: {label}");
    }
    let (used, _, entries) = state.preparer.cache.stats();
    Json(json!({
        "ok": removed,
        "used_mb": format!("{:.1}", used as f64 / (1024.0 * 1024.0)),
        "entries": entries,
    }))
    .into_response()
}

#[derive(Deserialize)]
struct CacheDeleteManyBody {
    ids: Vec<String>,
}

async fn handle_admin_cache_delete_many(
    State(state): State<AppState>,
    Json(body): Json<CacheDeleteManyBody>,
) -> Response {
    let deleted = state.preparer.cache.delete_many(&body.ids);
    if deleted == 1 {
        info!("1 cached track deleted");
    } else if deleted > 1 {
        info!("{deleted} cached tracks deleted");
    }
    let (used, _, entries) = state.preparer.cache.stats();
    Json(json!({
        "deleted": deleted,
        "used_mb": format!("{:.1}", used as f64 / (1024.0 * 1024.0)),
        "entries": entries,
    }))
    .into_response()
}

#[derive(Deserialize)]
struct CacheDownloadForm {
    ids: String,
}

async fn handle_admin_cache_download(
    State(state): State<AppState>,
    Form(form): Form<CacheDownloadForm>,
) -> Response {
    let ids: Vec<String> = form
        .ids
        .lines()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect();
    let paths = state.preparer.cache.get_disk_paths(&ids);
    if paths.is_empty() {
        return (StatusCode::NOT_FOUND, "no cached tracks found").into_response();
    }
    if paths.len() == 1 {
        let path = &paths[0];
        let file_name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "track.ogg".into());
        let file = match tokio::fs::File::open(path).await {
            Ok(f) => f,
            Err(_) => return StatusCode::NOT_FOUND.into_response(),
        };
        let len = file.metadata().await.ok().map(|m| m.len());
        let disposition = content_disposition("attachment", &file_name);
        let stream = tokio_util::io::ReaderStream::new(file);
        let mut resp = Response::new(Body::from_stream(stream));
        let headers = resp.headers_mut();
        headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("audio/ogg"));
        if let Ok(val) = HeaderValue::from_str(&disposition) {
            headers.insert(header::CONTENT_DISPOSITION, val);
        }
        if let Some(size) = len
            && let Ok(val) = HeaderValue::from_str(&size.to_string())
        {
            headers.insert(header::CONTENT_LENGTH, val);
        }
        return resp;
    }

    let zip_path =
        std::env::temp_dir().join(format!("togewire-cache-zip-{}.zip", crate::random_hex(8)));
    let zip_out = zip_path.clone();
    let built = tokio::task::spawn_blocking(move || {
        crate::audio::manual_download::write_store_zip(&zip_out, &paths)
    })
    .await;
    if !matches!(built, Ok(Ok(()))) {
        let _ = tokio::fs::remove_file(&zip_path).await;
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            "failed to package archive",
        )
            .into_response();
    }

    let file = match tokio::fs::File::open(&zip_path).await {
        Ok(f) => f,
        Err(_) => {
            let _ = tokio::fs::remove_file(&zip_path).await;
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    let len = file.metadata().await.ok().map(|m| m.len());
    // Unlink immediately after opening so the temp zip vanishes as soon as streaming ends.
    let _ = tokio::fs::remove_file(&zip_path).await;

    let disposition = content_disposition("attachment", "togewire-cache.zip");
    let stream = tokio_util::io::ReaderStream::new(file);
    let mut resp = Response::new(Body::from_stream(stream));
    let headers = resp.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/zip"),
    );
    if let Ok(val) = HeaderValue::from_str(&disposition) {
        headers.insert(header::CONTENT_DISPOSITION, val);
    }
    if let Some(size) = len
        && let Ok(val) = HeaderValue::from_str(&size.to_string())
    {
        headers.insert(header::CONTENT_LENGTH, val);
    }
    resp
}

#[derive(Deserialize)]
struct ManualDownloadRequest {
    url: String,
    #[serde(default)]
    source: Option<ManualDownloadSource>,
    #[serde(default = "default_manual_download_quality")]
    quality: String,
    #[serde(default)]
    normalize: bool,
}

fn default_manual_download_quality() -> String {
    "max".into()
}

async fn handle_admin_manual_download_current(
    State(state): State<AppState>,
) -> Json<serde_json::Value> {
    Json(state.manual_downloads.list_status())
}

async fn handle_admin_manual_download_start(
    State(state): State<AppState>,
    Json(req): Json<ManualDownloadRequest>,
) -> Response {
    match state.manual_downloads.start(
        state.spotify_client.clone(),
        Arc::clone(&state.preparer),
        state.shutdown.clone(),
        &req.url,
        req.source,
        &req.quality,
        req.normalize,
    ) {
        Ok(id) => {
            let list = state.manual_downloads.list_status();
            Json(json!({ "id": id, "jobs": list["jobs"] })).into_response()
        }
        Err(msg) => (StatusCode::BAD_REQUEST, Json(json!({ "error": msg }))).into_response(),
    }
}

async fn handle_admin_manual_download_pause(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Response {
    if state.manual_downloads.pause(&id) {
        Json(state.manual_downloads.list_status()).into_response()
    } else {
        StatusCode::NOT_FOUND.into_response()
    }
}

async fn handle_admin_manual_download_resume(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Response {
    if state.manual_downloads.resume(&id) {
        Json(state.manual_downloads.list_status()).into_response()
    } else {
        StatusCode::NOT_FOUND.into_response()
    }
}

async fn handle_admin_manual_download_stop(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Response {
    if state.manual_downloads.stop(&id) {
        Json(state.manual_downloads.list_status()).into_response()
    } else {
        StatusCode::NOT_FOUND.into_response()
    }
}

async fn handle_admin_manual_download_cancel(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Response {
    state.manual_downloads.cancel(&id);
    Json(state.manual_downloads.list_status()).into_response()
}

async fn handle_admin_manual_download_file(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Response {
    let Some(ready) = state.manual_downloads.get_ready_file(&id) else {
        return (StatusCode::NOT_FOUND, "download not found").into_response();
    };
    let file = match tokio::fs::File::open(&ready.file_path).await {
        Ok(f) => f,
        Err(_) => {
            state.manual_downloads.cancel(&id);
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    let len = file.metadata().await.ok().map(|m| m.len());
    let disposition = content_disposition("attachment", &ready.download_name);
    let stream = tokio_util::io::ReaderStream::new(file);
    let mut resp = Response::new(Body::from_stream(stream));
    let headers = resp.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(ready.content_type),
    );
    if let Ok(val) = HeaderValue::from_str(&disposition) {
        headers.insert(header::CONTENT_DISPOSITION, val);
    }
    if let Some(size) = len
        && let Ok(val) = HeaderValue::from_str(&size.to_string())
    {
        headers.insert(header::CONTENT_LENGTH, val);
    }
    resp
}

/// RFC 6266 / 5987 Content-Disposition header with ASCII fallback and UTF-8 `filename*`.
fn content_disposition(kind: &str, name: &str) -> String {
    use std::fmt::Write;
    let ascii: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_graphic() && c != '"' && c != '\\' {
                c
            } else if c.is_whitespace() {
                ' '
            } else {
                '_'
            }
        })
        .collect();
    let mut encoded = String::with_capacity(name.len() * 2);
    for b in name.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                encoded.push(char::from(b));
            }
            _ => {
                let _ = write!(encoded, "%{b:02X}");
            }
        }
    }
    format!("{kind}; filename=\"{ascii}\"; filename*=UTF-8''{encoded}")
}

pub fn start_workers(state: &AppState) {
    tokio::spawn(workers::spotify_monitor_worker(state.clone()));
    crate::audio::ytdlp::keep_updated();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_ip_is_the_first_public_hop_from_the_right() {
        let xff = |value: &str, peer: &str| {
            let mut headers = HeaderMap::new();
            headers.insert("x-forwarded-for", value.parse().unwrap());
            client_ip(&headers, peer.parse().unwrap())
        };
        let (kdnx, connector) = ("127.0.0.1:1", "10.8.0.2:1");
        assert_eq!(xff("203.0.113.10", kdnx), "203.0.113.10");
        assert_eq!(xff("203.0.113.10", connector), "203.0.113.10");
        // cloudflared appends to whatever the client sent; so may a local hop after it.
        assert_eq!(xff("9.9.9.9, 203.0.113.10", kdnx), "203.0.113.10");
        assert_eq!(
            xff("9.9.9.9, 203.0.113.10, 127.0.0.1", kdnx),
            "203.0.113.10"
        );
        // A LAN client behind the proxy; garbage ends the walk at the last good hop.
        assert_eq!(xff("192.168.1.5", kdnx), "192.168.1.5");
        assert_eq!(xff("unknown, 203.0.113.10", kdnx), "203.0.113.10");
        assert_eq!(xff("unknown", kdnx), "127.0.0.1");
        // A public peer's headers are the client's own and prove nothing.
        assert_eq!(xff("203.0.113.10", "8.8.8.8:1"), "8.8.8.8");

        assert_eq!(xff("2001:db8:1:2:3:4:5:6", kdnx), "2001:db8:1:2::/64");
        assert_eq!(xff("1.1.1.1", "[::ffff:203.0.113.9]:1"), "203.0.113.9");
        assert_eq!(xff("203.0.113.10", "[::ffff:127.0.0.1]:1"), "203.0.113.10");
    }

    #[test]
    fn trusted_proxies_are_private_or_loopback() {
        for ip in [
            "127.0.0.1",
            "::1",
            "::ffff:127.0.0.1",
            "192.168.1.1",
            "::ffff:192.168.1.1",
            "10.0.0.1",
            "172.16.0.1",
            "fd00::1",
        ] {
            assert!(is_trusted_proxy(ip.parse().unwrap()), "{ip}");
        }
        for ip in ["203.0.113.1", "::ffff:203.0.113.1", "8.8.8.8"] {
            assert!(!is_trusted_proxy(ip.parse().unwrap()), "{ip}");
        }
    }

    #[test]
    fn byte_range_parser_handles_standard_ranges() {
        assert_eq!(parse_byte_range("bytes=0-99", 1000), Some((0, 99)));
        assert_eq!(parse_byte_range("bytes=500-", 1000), Some((500, 999)));
        assert_eq!(parse_byte_range("bytes=500-5000", 1000), Some((500, 999)));
        assert_eq!(parse_byte_range("bytes=-100", 1000), Some((900, 999)));
        assert_eq!(parse_byte_range("bytes=-5000", 1000), Some((0, 999)));
        assert_eq!(parse_byte_range("bytes=1000-", 1000), None);
        assert_eq!(parse_byte_range("bytes=50-10", 1000), None);
        assert_eq!(parse_byte_range("bytes=-0", 1000), None);
        assert_eq!(parse_byte_range("bytes=0-10, 20-30", 1000), None);
        assert_eq!(parse_byte_range("bytes=0-0", 0), None);
    }
}
