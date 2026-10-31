pub mod auth;
pub mod stream;
pub mod websocket;
pub mod workers;

use axum::{
    Form, Json, Router,
    body::{Body, Bytes},
    extract::{ConnectInfo, Path, State, ws::Utf8Bytes, ws::WebSocketUpgrade},
    http::{HeaderMap, HeaderValue, Method, Request, StatusCode, header},
    middleware::{self, Next},
    response::{Html, IntoResponse, Redirect, Response, sse},
    routing::{get, post},
};
use futures::{StreamExt, TryStreamExt, stream::BoxStream};
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
use subtle::ConstantTimeEq;
use tera::Tera;
use tokio::sync::{broadcast, mpsc};
use tokio_stream::wrappers::BroadcastStream;
use tokio_util::io::StreamReader;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use crate::audio::TrackPreparer;
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

/// Real-time event pushed to togedemon via SSE.
#[derive(Clone, serde::Serialize)]
pub struct TogedemonEvent {
    pub is_playing: bool,
}

impl TogedemonEvent {
    fn new(s: Option<&SpotifyState>) -> Self {
        Self {
            is_playing: config::get().read().player_enabled
                && s.is_some_and(|s| s.track.is_some() && s.is_playing),
        }
    }
}

#[derive(Clone)]
pub struct AppState {
    /// Latest Spotify poll; `None` while nothing is playing.
    pub current: Arc<RwLock<Option<SpotifyState>>>,
    pub spotify_client: SpotifyClient,
    pub stream_manager: StreamManager,
    pub preparer: Arc<TrackPreparer>,
    pub ws_clients: Arc<RwLock<HashMap<u64, mpsc::Sender<Utf8Bytes>>>>,
    pub tera: Arc<Tera>,
    pub shutdown: CancellationToken,
    /// Cancels the connected togedemon; `Some` exactly while one is live.
    pub togedemon_cancel: Arc<Mutex<Option<CancellationToken>>>,
    pub togedemon_events: broadcast::Sender<TogedemonEvent>,
}

impl AppState {
    #[must_use]
    pub fn togedemon_live(&self) -> bool {
        self.togedemon_cancel.lock().is_some()
    }

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
        spotify_client,
        ws_clients: Arc::default(),
        tera: Arc::new(load_templates()),
        shutdown: CancellationToken::new(),
        togedemon_cancel: Arc::default(),
        togedemon_events: broadcast::channel(16).0,
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
        .route("/admin/cache/clear", post(handle_admin_cache_clear))
        .route_layer(middleware::from_fn(require_admin));
    Router::new()
        .route("/", get(handle_root))
        .route("/player", get(handle_player))
        .route("/ping", get(handle_ping))
        .route("/stream", get(handle_stream))
        .route("/ws", get(handle_ws))
        .route("/togedemon/push", post(handle_togedemon_push))
        .route("/togedemon/events", get(handle_togedemon_events))
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

fn valid_togedemon_token(headers: &HeaderMap) -> bool {
    let provided = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("");
    let cfg = config::get().read();
    !cfg.togedemon_token.is_empty()
        && bool::from(provided.as_bytes().ct_eq(cfg.togedemon_token.as_bytes()))
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
    (!config::get().read().player_enabled)
        .then(|| (StatusCode::SERVICE_UNAVAILABLE, "player disabled").into_response())
}

async fn handle_root() -> Redirect {
    let cfg = config::get().read();
    Redirect::to(if !cfg.has_admin_password() {
        "/admin/login"
    } else if cfg.player_enabled {
        "/player"
    } else {
        "/admin"
    })
}

async fn handle_dashboard(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Response {
    let mut ctx = tera::Context::new();
    {
        let cfg = config::get().read();
        ctx.insert("authenticated", &cfg.is_authenticated());
        ctx.insert("player_enabled", &cfg.player_enabled);
        ctx.insert("use_librespot", &config::use_librespot());
        let plan = match config::premium() {
            Some(true) => "premium",
            Some(false) => "free",
            None => "unknown",
        };
        ctx.insert("plan", plan);
        ctx.insert("togedemon_token", &cfg.togedemon_token);
        ctx.insert("audio_bitrate", &cfg.audio_bitrate);
        ctx.insert("low_data_streaming", &cfg.low_data_streaming);
        ctx.insert("normalization", &cfg.normalization);
        ctx.insert("togematch_api", &cfg.togematch_api);
    }
    let togedemon_bin = if cfg!(debug_assertions) {
        "togedemon-dev"
    } else {
        "togedemon"
    };
    ctx.insert("togedemon_bin", togedemon_bin);

    let (used_bytes, max_bytes, entries) = state.preparer.cache.stats();
    ctx.insert(
        "cache_used_mb",
        &format!("{:.1}", used_bytes as f64 / (1024.0 * 1024.0)),
    );
    ctx.insert("cache_max_mb", &(max_bytes / (1024 * 1024)));
    ctx.insert("cache_entries", &entries);

    // The URL the admin actually typed, for the copyable togedemon command.
    let proto = if is_request_secure(&headers, peer) {
        "https"
    } else {
        "http"
    };
    let host = headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("localhost:7093");
    ctx.insert("server_url", &format!("{proto}://{host}"));

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
    // Playback is only public while the player is on; otherwise this stays a health check.
    if !config::get().read().player_enabled {
        return Json(json!({ "status": "ok", "timestamp": now }));
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
    let body = futures::stream::iter(prefix)
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

impl futures::Stream for ListenerStream {
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

/// GET /togedemon/events - SSE feed of track updates for togedemon.
async fn handle_togedemon_events(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if !valid_togedemon_token(&headers) {
        return (StatusCode::UNAUTHORIZED, "invalid token").into_response();
    }
    let initial = TogedemonEvent::new(state.current.read().as_ref());
    let live = BroadcastStream::new(state.togedemon_events.subscribe())
        .filter_map(|r| std::future::ready(r.ok()));
    let shutdown = state.shutdown.clone();
    let events = futures::stream::once(std::future::ready(initial))
        .chain(live)
        .map(|e| {
            let data = serde_json::to_string(&e).unwrap_or_default();
            Ok::<_, std::convert::Infallible>(
                sse::Event::default().event("track_update").data(data),
            )
        })
        .take_until(async move { shutdown.cancelled().await });
    sse::Sse::new(events)
        .keep_alive(sse::KeepAlive::default())
        .into_response()
}

/// POST /togedemon/push - a raw Ogg/Opus stream from the togedemon binary.
async fn handle_togedemon_push(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    if !valid_togedemon_token(&headers) {
        warn!("Togedemon push rejected: bad token from {}", peer.ip());
        return (StatusCode::UNAUTHORIZED, "invalid token").into_response();
    }
    if let Some(off) = player_disabled() {
        return off;
    }
    let ip = client_ip(&headers, peer);

    // The newest togedemon wins. Tokens are swapped and cancelled under one lock, so
    // "cancelled" always means "no longer the live one".
    let cancel = CancellationToken::new();
    {
        let mut slot = state.togedemon_cancel.lock();
        if let Some(old) = slot.replace(cancel.clone()) {
            info!("Togedemon from {ip} takes over from the previous connection");
            old.cancel();
        }
    }
    info!("Togedemon connected from {ip}");
    let pipe = state.stream_manager.play_live();
    workers::rebroadcast(&state);

    let reader = StreamReader::new(body.into_data_stream().map_err(io::Error::other));
    tokio::select! {
        () = cancel.cancelled() => {}
        res = stream::pump_ogg(reader, 0, false, &pipe) => {
            if let Err(e) = res {
                debug!("Togedemon stream from {ip} ended: {e}");
            }
        }
    }

    {
        let mut slot = state.togedemon_cancel.lock();
        if cancel.is_cancelled() {
            debug!("Togedemon from {ip} superseded");
            return StatusCode::OK.into_response();
        }
        *slot = None;
    }
    info!("Togedemon disconnected from {ip}");
    state.stream_manager.stop();
    // Fall back to the cache: the current track if cached, else quiet until prepared.
    if let Some(current) = state.current()
        && let Some(track) = &current.track
    {
        workers::switch_track(&state, &current, track);
    }
    StatusCode::OK.into_response()
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
    } else if !auth::verify_password(admin_hash, form.password).await {
        return login_page(&state, "login", "", "Invalid password");
    }

    auth::clear_failed_attempts(&ip);
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

async fn handle_admin_disconnect(State(state): State<AppState>) -> Redirect {
    state.spotify_client.clear_session().await;
    state.preparer.reset_librespot().await;
    config::set_use_librespot(false);
    config::set_premium(None);
    info!("Spotify disconnected by admin");
    Redirect::to("/admin?n=disconnected")
}

#[derive(Deserialize)]
struct CacheForm {
    cache_max_mb: u64,
    audio_bitrate: String,
    normalization: config::Normalization,
    /// A checkbox: sent only when on.
    low_data_streaming: Option<String>,
    #[serde(default)]
    togematch_api: String,
}

async fn handle_admin_cache(
    State(state): State<AppState>,
    Form(form): Form<CacheForm>,
) -> Redirect {
    if !config::AUDIO_BITRATES.contains(&form.audio_bitrate.as_str()) {
        return Redirect::to("/admin?n=bad_bitrate");
    }
    let togematch_api = form.togematch_api.trim().trim_end_matches('/').to_string();
    if !(togematch_api.is_empty()
        || togematch_api.starts_with("http://")
        || togematch_api.starts_with("https://"))
    {
        return Redirect::to("/admin?n=bad_togematch_api");
    }
    let mb = form.cache_max_mb.max(64);
    let saved = config::update(|cfg| {
        cfg.cache_max_mb = mb;
        cfg.audio_bitrate = form.audio_bitrate;
        cfg.normalization = form.normalization;
        cfg.low_data_streaming = form.low_data_streaming.is_some();
        cfg.togematch_api = togematch_api;
    });
    if let Err(e) = saved {
        error!("Failed to save cache settings: {e}");
        return Redirect::to("/admin?n=save_failed");
    }
    state.preparer.cache.set_max_mb(mb);
    Redirect::to("/admin?n=saved")
}

#[derive(Deserialize)]
struct PlayerBody {
    enabled: bool,
}

async fn handle_admin_player(
    State(state): State<AppState>,
    Json(body): Json<PlayerBody>,
) -> Response {
    if let Err(e) = config::update(|cfg| cfg.player_enabled = body.enabled) {
        error!("Failed to save player_enabled: {e}");
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    if body.enabled {
        workers::start_stream_if_playing(&state);
    } else {
        if let Some(togedemon) = state.togedemon_cancel.lock().take() {
            togedemon.cancel();
        }
        state.stream_manager.stop();
        // Sockets opened while the player was on would otherwise keep receiving
        // track_update; the upgrade gate only covers new connections.
        state.ws_clients.write().clear();
    }
    let _ = state
        .togedemon_events
        .send(TogedemonEvent::new(state.current().as_ref()));
    info!(
        "Player {}",
        if body.enabled { "enabled" } else { "disabled" }
    );
    Json(json!({ "enabled": body.enabled })).into_response()
}

async fn handle_admin_cache_clear(State(state): State<AppState>) -> Redirect {
    state.preparer.cache.clear();
    info!("Track cache cleared via admin");
    Redirect::to("/admin?n=cleared")
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
}
