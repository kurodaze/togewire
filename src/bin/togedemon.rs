use clap::Parser;
use std::io::{BufRead, Read};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};
use tracing::{debug, error, info, warn};

const RECONNECT_DELAY: Duration = Duration::from_secs(2);
/// Seconds of silence from Spotify before disconnecting the stream (falls back to cache).
const SILENCE_TIMEOUT_SECS: u64 = 5;
/// How many consecutive small encoded chunks before treating as silence.
/// With VBR Opus, silent frames shrink to under 100 bytes.
const SILENCE_CHUNKS_LIMIT: usize = 128;
const SILENT_CHUNK_BYTES: usize = 100;

#[derive(Parser)]
#[command(
    name = "togedemon",
    about = "Captures Spotify audio and streams it to Togewire"
)]
struct Args {
    /// Togewire server URL (e.g. <https://togewire.example.com>)
    #[arg(long)]
    server: String,

    /// Bearer token (shown in the Togewire admin dashboard under togedemon). Prefer the
    /// environment variable: other local users can read a process's arguments.
    #[arg(long, env = "TOGEDEMON_TOKEN", hide_env_values = true)]
    token: String,

    /// Opus bitrate. Lower values save bandwidth on slow connections.
    #[arg(
        long,
        default_value = "144k",
        value_parser = clap::builder::PossibleValuesParser::new(togewire::config::AUDIO_BITRATES)
    )]
    quality: String,
}

fn main() {
    rustls::crypto::aws_lc_rs::default_provider()
        .install_default()
        .expect("failed to install rustls CryptoProvider");

    let args = Args::parse();
    tracing_subscriber::fmt().with_target(false).init();

    info!(
        "Starting{} - server: {}, quality: {}",
        if cfg!(debug_assertions) { " [DEV]" } else { "" },
        args.server,
        args.quality
    );

    if args.server.starts_with("http://") {
        warn!("--server uses plain HTTP. Your token is sent unencrypted.");
    }

    togewire::require_bin("ffmpeg", "-version", "Install it and try again.");

    let server_playing = Arc::new(AtomicBool::new(false));
    {
        let playing = server_playing.clone();
        let events_url = format!("{}/togedemon/events", args.server.trim_end_matches('/'));
        let token = args.token.clone();
        thread::spawn(move || server_monitor(events_url, token, playing));
    }
    // Sleep up to `max`, waking early once the server reports playback.
    let wait_for_playback = |max: Duration| {
        let start = Instant::now();
        while start.elapsed() < max && !server_playing.load(Ordering::Relaxed) {
            thread::sleep(Duration::from_millis(250));
        }
    };

    let client = reqwest::blocking::Client::builder()
        .timeout(None)
        .redirect(reqwest::redirect::Policy::none())
        .https_only(args.server.starts_with("https://"))
        .build()
        .expect("failed to build HTTP client");

    let mut was_waiting = false;
    loop {
        let result = run_once(&args, &client);
        let waiting = matches!(result, RunResult::SpotifyNotPlaying(_));
        match result {
            RunResult::Streaming => {}
            RunResult::AuthError => {
                error!("Authentication failed - check your token. Retrying in 30s.");
                thread::sleep(Duration::from_secs(30));
            }
            RunResult::SilenceTimeout => {
                info!("Silence timeout - no audio from Spotify for {SILENCE_TIMEOUT_SECS}s.");
                wait_for_playback(RECONNECT_DELAY);
            }
            RunResult::SpotifyNotPlaying(msg) => {
                if !was_waiting {
                    info!("{msg}");
                }
                wait_for_playback(Duration::from_secs(30));
            }
            RunResult::Error(msg) => {
                error!(
                    "Error: {msg} - reconnecting in {}s",
                    RECONNECT_DELAY.as_secs()
                );
                thread::sleep(RECONNECT_DELAY);
            }
        }
        was_waiting = waiting;
    }
}

/// Mirror the server's `is_playing` from its SSE feed, reconnecting forever.
fn server_monitor(events_url: String, token: String, playing: Arc<AtomicBool>) {
    // Per-read timeout: the server sends an SSE keep-alive every 15s, so a minute of
    // silence is a dead connection, not an idle one.
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_mins(1))
        .redirect(reqwest::redirect::Policy::none())
        .https_only(events_url.starts_with("https://"))
        .build()
        .unwrap_or_else(|_| reqwest::blocking::Client::new());

    let mut connected = false;
    loop {
        match client
            .get(&events_url)
            .header("Authorization", format!("Bearer {token}"))
            .header("Accept", "text/event-stream")
            .send()
        {
            Ok(resp) if resp.status().is_success() => {
                if !connected {
                    connected = true;
                    debug!("Server sync established (SSE)");
                }
                let reader = std::io::BufReader::new(resp);
                let mut data_buf = String::new();
                for line in reader.lines() {
                    let Ok(line) = line else { break };
                    if let Some(data) = line.strip_prefix("data:") {
                        data_buf = data.trim().to_string();
                    } else if line.is_empty() && !data_buf.is_empty() {
                        if let Ok(json) = serde_json::from_str::<serde_json::Value>(&data_buf) {
                            let is_playing = json
                                .get("is_playing")
                                .and_then(serde_json::Value::as_bool)
                                .unwrap_or(false);
                            playing.store(is_playing, Ordering::Relaxed);
                        }
                        data_buf.clear();
                    }
                }
                debug!("Server SSE connection lost, reconnecting...");
            }
            Ok(resp) => {
                debug!("Server SSE returned {}, retrying...", resp.status());
            }
            Err(e) => {
                debug!("Server SSE connection failed: {e}");
            }
        }
        thread::sleep(Duration::from_secs(2));
    }
}

enum RunResult {
    AuthError,
    SilenceTimeout,
    Streaming,
    SpotifyNotPlaying(String),
    Error(String),
}

/// Spotify's PA sink-input index: by name/PID, else via its `PipeWire` node id.
fn find_spotify_stream() -> Result<u32, String> {
    if let Ok(result) = find_spotify_sink_input(None) {
        return Ok(result);
    }

    let pw_node_id = find_spotify_pw_node().ok_or("Spotify audio stream not found")?;
    debug!("Found Spotify PipeWire node #{pw_node_id}");

    find_spotify_sink_input(Some(pw_node_id))
        .map_err(|_| format!("Found PW node #{pw_node_id} but no matching sink-input in pactl"))
}

/// Collect PIDs of running Spotify processes via /proc.
fn get_spotify_pids() -> Vec<String> {
    let mut pids = Vec::new();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return pids;
    };
    for entry in entries.flatten() {
        let fname = entry.file_name();
        let fname = fname.to_string_lossy();
        if fname.parse::<u32>().is_err() {
            continue;
        }
        if let Ok(target) = std::fs::read_link(entry.path().join("exe"))
            && target.to_string_lossy().to_lowercase().contains("spotify")
        {
            pids.push(fname.to_string());
        }
    }
    pids
}

/// Parse `pactl list sink-inputs` and return the index matching Spotify.
fn find_spotify_sink_input(pw_node_id: Option<u32>) -> Result<u32, String> {
    let out = Command::new("pactl")
        .args(["list", "sink-inputs"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .map_err(|e| format!("pactl not found: {e}"))?;

    let text = String::from_utf8_lossy(&out.stdout);
    let spotify_pids = get_spotify_pids();
    let pw_id_str = pw_node_id.map(|id| id.to_string());

    for block in text.split("Sink Input #").skip(1) {
        let is_spotify = block.lines().any(|l| {
            let lc = l.to_lowercase();
            if (lc.contains("application.name") || lc.contains("application.process.binary"))
                && lc.contains("spotify")
            {
                return true;
            }
            // `key = "value"` property lines.
            let value = l.rsplit('=').next().unwrap_or("").trim().trim_matches('"');
            (lc.contains("application.process.id") && spotify_pids.iter().any(|p| p == value))
                || (lc.contains("object.id") && pw_id_str.as_deref() == Some(value))
        });

        if !is_spotify {
            continue;
        }

        let idx: u32 = block
            .lines()
            .next()
            .and_then(|l| l.trim().parse().ok())
            .ok_or_else(|| "Could not parse sink-input index".to_string())?;

        return Ok(idx);
    }

    Err("Spotify sink-input not found".into())
}

/// Find Spotify's stream node in `PipeWire` via `pw-dump`.
fn find_spotify_pw_node() -> Option<u32> {
    let spotify_pids = get_spotify_pids();

    let out = Command::new("pw-dump")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()?;

    let objects: Vec<serde_json::Value> = serde_json::from_slice(&out.stdout).ok()?;

    // Pass 1: find Spotify client IDs
    let mut spotify_client_ids = std::collections::HashSet::new();
    for obj in &objects {
        if pw_type(obj) != Some("PipeWire:Interface:Client") {
            continue;
        }
        if let Some(props) = pw_props(obj)
            && is_spotify_props(props, &spotify_pids)
            && let Some(id) = obj.get("id").and_then(serde_json::Value::as_u64)
        {
            spotify_client_ids.insert(id);
        }
    }

    // Pass 2: find Stream/Output nodes belonging to those clients
    for obj in &objects {
        if pw_type(obj) != Some("PipeWire:Interface:Node") {
            continue;
        }
        let Some(props) = pw_props(obj) else { continue };

        let media_class = pw_str(props, "media.class");
        if !media_class.is_empty() && !media_class.contains("Stream/Output") {
            continue;
        }

        let client_id = pw_u64(props, "client.id");
        let client_match = spotify_client_ids.contains(&client_id);

        if (is_spotify_props(props, &spotify_pids) || client_match)
            && let Some(id) = obj.get("id").and_then(serde_json::Value::as_u64)
        {
            return u32::try_from(id).ok();
        }
    }

    None
}

fn pw_type(obj: &serde_json::Value) -> Option<&str> {
    obj.get("type").and_then(|t| t.as_str())
}

fn pw_props(obj: &serde_json::Value) -> Option<&serde_json::Value> {
    obj.get("info").and_then(|i| i.get("props"))
}

fn pw_str<'a>(props: &'a serde_json::Value, key: &str) -> &'a str {
    props
        .get(key)
        .and_then(serde_json::Value::as_str)
        .unwrap_or("")
}

fn pw_u64(props: &serde_json::Value, key: &str) -> u64 {
    props.get(key).map_or(u64::MAX, |v| {
        v.as_u64()
            .unwrap_or_else(|| v.as_str().and_then(|s| s.parse().ok()).unwrap_or(u64::MAX))
    })
}

fn is_spotify_props(props: &serde_json::Value, spotify_pids: &[String]) -> bool {
    let binary = pw_str(props, "application.process.binary").to_lowercase();
    let name = pw_str(props, "application.name").to_lowercase();
    if binary.contains("spotify") || name.contains("spotify") {
        return true;
    }
    let pid = pw_u64(props, "application.process.id").to_string();
    spotify_pids.contains(&pid)
}

fn run_once(args: &Args, client: &reqwest::blocking::Client) -> RunResult {
    let sink_input_idx = match find_spotify_stream() {
        Ok(idx) => idx,
        Err(e) => return RunResult::SpotifyNotPlaying(format!("Spotify detection failed: {e}")),
    };
    info!(
        "Encoding at {} Opus - sink-input #{sink_input_idx}",
        args.quality
    );

    // ffmpeg reads PCM from the capture thread on stdin and writes Ogg/Opus to stdout.
    let mut ffmpeg = match Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error"])
        // Zero-latency input parsing of raw PCM
        .args([
            "-fflags",
            "nobuffer",
            "-flags",
            "low_delay",
            "-probesize",
            "32",
        ])
        .args([
            "-analyzeduration",
            "0",
            "-f",
            "f32le",
            "-ar",
            "44100",
            "-ac",
            "2",
        ])
        .args(["-i", "pipe:0"])
        // No audio filter: Spotify already normalizes to -14 LUFS.
        .args([
            "-c:a",
            "libopus",
            "-b:a",
            &args.quality,
            "-application",
            "audio",
        ])
        .args([
            "-ar",
            "48000",
            "-vbr",
            "constrained",
            "-compression_level",
            "10",
        ])
        .args(["-frame_duration", "20", "-page_duration", "20000"])
        .args(["-f", "ogg", "-flush_packets", "1", "pipe:1"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => return RunResult::Error(format!("Failed to spawn ffmpeg: {e}")),
    };
    let (Some(mut ffmpeg_in), Some(ffmpeg_out)) = (ffmpeg.stdin.take(), ffmpeg.stdout.take())
    else {
        let _ = ffmpeg.kill();
        let _ = ffmpeg.wait();
        return RunResult::Error("ffmpeg pipes unavailable".into());
    };

    let push_url = format!("{}/togedemon/push", args.server.trim_end_matches('/'));
    info!("Streaming to {push_url}");

    // Capture thread: parec's --monitor-stream isolates Spotify from other apps. The stream
    // id can change on track transitions, ending parec; run_once() then just restarts.
    let capture = thread::spawn(move || {
        let Ok(mut parec) = Command::new("parec")
            .arg(format!("--monitor-stream={sink_input_idx}"))
            .args([
                "--format=float32le",
                "--rate=44100",
                "--channels=2",
                "--latency-msec=50",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .inspect_err(|e| error!("Failed to spawn parec: {e}"))
        else {
            return;
        };
        if let Some(mut out) = parec.stdout.take() {
            // Ends when parec exits or ffmpeg is killed; dropping ffmpeg_in lets it flush.
            let _ = std::io::copy(&mut out, &mut ffmpeg_in);
        }
        let _ = parec.kill();
        let _ = parec.wait();
    });

    let silenced = Arc::new(AtomicBool::new(false));
    let body = SilenceCutoff {
        inner: ffmpeg_out,
        silent_chunks: 0,
        last_audio: Instant::now(),
        silenced: silenced.clone(),
    };
    let response = client
        .post(&push_url)
        .header("Authorization", format!("Bearer {}", args.token))
        .header("Content-Type", "audio/ogg")
        .body(reqwest::blocking::Body::new(body))
        .send();

    let _ = ffmpeg.kill();
    let _ = ffmpeg.wait();
    let _ = capture.join();

    if silenced.load(Ordering::Relaxed) {
        return RunResult::SilenceTimeout;
    }
    match response {
        Ok(resp) if resp.status() == reqwest::StatusCode::UNAUTHORIZED => RunResult::AuthError,
        Ok(resp) if !resp.status().is_success() => {
            RunResult::Error(format!("Server returned HTTP {}", resp.status()))
        }
        Ok(_) => RunResult::Streaming,
        Err(e) => RunResult::Error(format!("HTTP error: {e}")),
    }
}

/// ffmpeg's Ogg output as the request body, ending it (EOF) once Spotify has been silent
/// for `SILENCE_TIMEOUT_SECS`, so the server falls back to its cache.
struct SilenceCutoff {
    inner: std::process::ChildStdout,
    silent_chunks: usize,
    last_audio: Instant,
    silenced: Arc<AtomicBool>,
}

impl Read for SilenceCutoff {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        if n >= SILENT_CHUNK_BYTES {
            self.silent_chunks = 0;
            self.last_audio = Instant::now();
        } else if n > 0 {
            self.silent_chunks += 1;
            if self.silent_chunks > SILENCE_CHUNKS_LIMIT
                && self.last_audio.elapsed().as_secs() >= SILENCE_TIMEOUT_SECS
            {
                self.silenced.store(true, Ordering::Relaxed);
                return Ok(0);
            }
        }
        Ok(n)
    }
}
