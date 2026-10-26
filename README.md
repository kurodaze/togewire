# togewire <sub><sup>(ˈtōɡāˌwī(ə)r)</sup></sub>

Self-hosted Spotify listen-along. Run `togewire` on a server, pair your Spotify account via `spotify.com/pair`, and anyone with the link can tune in live through a browser.

- **Listen-along or archive**: Public playback (`/player` and `/stream`) is disabled by default. Keep it off to cache/archive what you listen to, or flip the toggle in `/admin` to open the stream to listeners.
- **Native pairing**: Authorize directly via RFC 8628 (`spotify.com/pair`) in the admin dashboard, with no Spotify developer app or cookies required.
- **Audio pipeline**: Caches tracks via librespot (Spotify Premium) or yt-dlp/YouTube fallback (Free), encoded to Opus over an Ogg pipe with instant burst buffering. Optional ReplayGain 2.0 normalization via `rsgain`.
- **Live desktop capture (`togedemon`)**: Optional companion daemon for Linux desktops. Captures Spotify's audio stream directly via PulseAudio/PipeWire for zero-mismatch live playback (ideal for Free accounts).

---

## Quick Start

### 1. Run via Podman / Docker (Recommended)

togewire keeps its config and cache in `configs/` and `data/`, which must belong to the user it runs as: `1000:1000` in `compose.yaml` and below, or your own `id -u`:`id -g` if those differ. Create both folders before the first start, or Docker creates them owned by root.

```bash
mkdir -p configs data
podman compose up -d   # rootless Podman: uncomment userns_mode in compose.yaml
# or: docker compose up -d
```

Or run directly:
```bash
# Podman, rootless (SELinux compatible)
podman run -d --name togewire --init --user 1000:1000 --userns=keep-id -p 7093:7093 -v ./configs:/app/configs:Z -v ./data:/app/data:Z ghcr.io/kurodaze/togewire:latest

# Docker
docker run -d --name togewire --init --user 1000:1000 -p 7093:7093 -v ./configs:/app/configs -v ./data:/app/data ghcr.io/kurodaze/togewire:latest
```

To give the container YouTube cookies (required for VPS/datacenter IPs, or for yt-dlp on Free accounts and fallback), stream them into the data volume:
```bash
docker exec -i togewire sh -c 'cat > /app/data/cookies.txt && chmod 600 /app/data/cookies.txt' < path/to/cookies.txt
# or with Podman:
podman exec -i togewire sh -c 'cat > /app/data/cookies.txt && chmod 600 /app/data/cookies.txt' < path/to/cookies.txt
```
togewire automatically detects `cookies.txt` (or any `*cookie*.txt`) in `data/`, including when added or replaced while running. See [Are you a bot?](https://github.com/kurodaze/togematch#are-you-a-bot) in the togematch README for how to export cookies from a spare YouTube account.

### 2. Or Install Native Binary

Download pre-built static `togewire` / `togedemon` binaries (`-x86_64` / `-aarch64`) from [GitHub Releases](https://github.com/kurodaze/togewire/releases) and check them with `sha256sum -c --ignore-missing SHA256SUMS`, or build both from source:

```bash
# Runtime: ffmpeg, plus rsgain if normalization is on. yt-dlp, and deno unless one is on
# PATH, are downloaded into data/bin/ on first use; yt-dlp is kept current. Building needs Rust.
sudo apt install ffmpeg rsgain
git clone https://github.com/kurodaze/togewire.git
cd togewire
./scripts/build.sh --release
./togewire --port 7093 --host 0.0.0.0
```

To run it as a systemd user service, with the checkout at `~/togewire`:

```sh
systemctl --user link ~/togewire/togewire.service
loginctl enable-linger "$USER"   # keeps it running after logout and starts it at boot
systemctl --user enable --now togewire
journalctl --user -u togewire -f
```

| Flag | Default | Description |
|------|---------|-------------|
| `--port` | `7093` | HTTP port to listen on |
| `--host` | `0.0.0.0` | IP address to bind to |
| `--no-librespot` | `false` | Skip Spotify native stream and cache all tracks via yt-dlp |
| `--cookies` | *(auto)* | YouTube cookies file for yt-dlp (required for VPS/datacenter IPs; see [how to export](https://github.com/kurodaze/togematch#are-you-a-bot)) |

### 3. Setup & Pair

1. Copy the one-time setup token printed to the terminal logs on first run.
2. Open `http://localhost:7093/admin/login`, enter the token, and set your admin password.
3. In the admin dashboard, click **Pair with Spotify** and confirm authorization in the Spotify tab.
4. When you're ready for listeners, turn on the **Player** switch at the top of `/admin` to enable `/player`.

---

## togedemon (Optional Live Capture)

`togedemon` runs on your Linux desktop where Spotify is playing. It isolates Spotify's audio via PulseAudio/PipeWire and streams it directly to your togewire server with minimal latency (bypassing YouTube matching on Free accounts). togedemon is not containerized but it can work with a togewire server container. It needs `ffmpeg`, `pactl` and `parec` (`pulseaudio-utils`; PipeWire via `pipewire-pulse`) on the desktop.

Live audio takes priority while connected. togewire continues pre-caching tracks in the background so the cache stays warm for seamless fallback if `togedemon` disconnects.

```bash
TOGEDEMON_TOKEN=<token> ./togedemon --server https://togewire.example.com
```

| Flag | Default | Description |
|------|---------|-------------|
| `--server` | *(required)* | togewire server URL |
| `--token` / `TOGEDEMON_TOKEN` | *(required)* | Bearer token from the togedemon section in `/admin`; prefer the env var, since other local users can read process arguments |
| `--quality` | `144k` | Opus bitrate (`112k`, `144k`, `192k`, `256k`) |

---

## Configuration

Place togewire behind a reverse proxy (such as [KDNX](https://github.com/kurodaze/kdnx) or a Cloudflare Tunnel) for HTTPS. `X-Forwarded-For` and `X-Forwarded-Proto` are trusted from loopback/private peers only (cloudflared, KDNX or a KDNX connector); the client is the first public address from the right of `X-Forwarded-For`, so proxies chained on the same host or LAN still resolve to the real visitor. Login rate limits count per IPv4 address or IPv6 /64.

Key options in `configs/config.json` (also adjustable in `/admin`):
- `player_enabled` (`false`): Enable public player at `/player` and `/stream`.
- `cache_max_mb` (`2048`): Maximum disk cache size in `data/track_cache/`. *(To use an external drive or NFS share, symlink it: `ln -s /mnt/nas/music data/track_cache`)*.
- `audio_bitrate` (`144k`): Opus VBR bitrate for caching tracks (`112k`, `144k`, `192k`, `256k`); YouTube's own Opus is kept as it is.
- `low_data_streaming` (`false`): Streams 112k copies for tracks cached above High (> 144k).
- `normalization` (`off`): `stream` normalizes streams in memory without touching cached files; `cache` tags newly cached files with ReplayGain 2.0 without re-encoding.
- `togematch_api_url` (empty): optional [togematch-api](https://github.com/kurodaze/togematch) on your network, e.g. `http://192.168.1.10:7094`. It picks the YouTube video for yt-dlp tracks and keeps its matches, shared with your other apps and fixable by hand.
- `cover_art` (`640`): `640` embeds the largest cover Spotify's metadata lists; `original` embeds the cover as uploaded (often 1400 to 3000 px, a few hundred KB more per track) and falls back to 640 (config file only).
- `log_level` (`info`): `trace`, `debug`, `info`, `warn` or `error` (config file only).

### Audio Pipeline & Quality Architecture

- **Spotify Sourcing**: Librespot automatically requests the highest available stream from Spotify:
  - togewire takes the best format Spotify's key server grants the account: the bit-perfect FLAC stream (up to 24-bit) with **Lossless**, else 320 kbps Vorbis, else lower on plans capped lower.
  - Lossless comes with Premium in most markets, and only with Premium Platinum where Spotify sells Lite, Standard and Platinum plans.
  - A format Spotify refuses the account is skipped for the rest of the session.
- **Spotify as Source of Truth for Metadata & Cover Art**:
  - Track title, every credited artist, album name, album artist, release year, track number, and disc number are **always sourced directly from Spotify** (Spotify's track metadata API and librespot, with Connect state as a fallback).
  - The official album cover is fetched directly from Spotify's CDN at 640px, or as uploaded with `cover_art` set to `original`, with Spotify's public oEmbed (300px) as a fallback when it's missing or fails to download. It's embedded as downloaded, never re-encoded.
  - Even when audio is fetched via `yt-dlp` (e.g. Free accounts), all YouTube video titles, channel names, descriptions, and video thumbnails are completely ignored and discarded. The audio stream is remuxed/encoded with official Spotify metadata and cover art embedded into the `.ogg` Vorbis comments.
- **Always Cached as Opus**: Tracks are cached as Opus VBR (`.ogg`) for optimal quality and storage efficiency. Sourcing directly from lossless FLAC avoids generational compression loss.
- **Cache Quality Tiers**:
  - `112k`: Normal (112 kbps Opus VBR)
  - `144k`: High (144 kbps Opus VBR), **recommended and the default**
  - `192k`: Very High (192 kbps Opus VBR)
  - `256k`: Max (256 kbps Opus VBR)
- **Opus VBR**: Bitrates are average targets (`-vbr on`); complex tracks scale higher dynamically.
- **Low-Data Streaming** (`low_data_streaming`): Tracks cached at > 144k stream from a 112k in-memory copy (`/dev/shm`, up to 3 copies, ~12 MB RAM), transcoded once and reused by seeks and replays; extra CPU per transcode, and copies stay in RAM to avoid disk wear. With `normalization` set to `stream`, the copy is encoded from normalized audio: same RAM as either feature alone.
- **yt-dlp Stream Copy**: YouTube's native ~160 kbps Opus audio streams (format 251) are remuxed directly with Spotify metadata and cover art without re-encoding.
- **Audio Normalization**: In `/admin`, choose between **Off**, **Stream only** (`stream`), or **Cache to disk** (`cache`):
  - **Stream only**: Normalizes in memory via `rsgain` (−14 LUFS / −1 dBTP) without modifying cached files; the next queue track is pre-normalized in the background. Almost no CPU, extra RAM.
  - **Cache to disk**: Tags newly cached files with ReplayGain 2.0 via `rsgain` without re-encoding. Lossless and fully reversible.

---

## Disclaimer & Acknowledgements

togewire is **not affiliated with Spotify or YouTube**.

togewire uses [FFmpeg](https://ffmpeg.org/) for audio processing, [librespot](https://github.com/librespot-org/librespot) for Spotify-native track fetch, [yt-dlp](https://github.com/yt-dlp/yt-dlp) with [Deno](https://deno.com/) as a YouTube matcher, and [rsgain](https://github.com/complexlogic/rsgain) for ReplayGain 2.0 loudness tagging.

## License

Copyright (C) 2026 KuroDaze. Licensed under the GNU General Public License v3.0 or later, see [LICENSE](LICENSE).

Plus Jakarta Sans (bundled under `web/static/fonts/`) is licensed separately under the SIL Open Font License - see `web/static/fonts/OFL.txt`.
