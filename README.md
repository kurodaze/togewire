# togewire <sub><sup>(ˈtōɡāˌwī(ə)r)</sup></sub>

Self-hosted Spotify listen-along. Run `togewire` on a server, pair your Spotify account via `spotify.com/pair`, and anyone with the link can tune in live through a browser.

- **Listen-along or archive**: Public playback (`/player` and `/stream`) and disk caching are both off by default. Turn on **Player** to stream (preparing tracks in memory when disk caching is off), turn on **Cache played tracks to disk** to archive what you play and manage it via **View cache**, or keep both off to use Direct download without polling Spotify.
- **Native pairing**: Authorize via RFC 8628 (`spotify.com/pair`) in `/admin`, with no Spotify developer app required.
- **Audio & tagging**: Fetches tracks via librespot (Spotify Premium) or yt-dlp/YouTube (Free/unpaired), encodes to Opus over Ogg with burst buffering, embeds clean metadata and 1:1 cover art into every file regardless of Spotify auth, and optionally applies ReplayGain 2.0 normalization via `rsgain`.
- **Direct download**: Download Spotify or YouTube tracks, albums, and playlists from `/admin` (even without pairing Spotify) with pause/resume/early stop, background completion, and automatic metadata and cover art tagging.

---

## Quick Start

### 1. Run via Podman / Docker (Recommended)

togewire keeps its config and cache in `configs/` and `data/`, which must belong to the user it runs as: `1000:1000` in `compose.yaml`, or your own `id -u`:`id -g` if those differ. Create both folders before the first start, or Docker creates them owned by root.

```bash
mkdir -p configs data
podman compose up -d   # rootless Podman: uncomment userns_mode in compose.yaml
# or: docker compose up -d
```

`compose.dev.yaml` builds the image from the checkout instead, with the faster-to-build dev profile.

To give the container YouTube cookies (required for VPS/datacenter IPs, or for yt-dlp on Free accounts and fallback), stream them into the data volume:
```bash
docker exec -i togewire sh -c 'cat > /app/data/cookies.txt && chmod 600 /app/data/cookies.txt' < path/to/cookies.txt
# or with Podman:
podman exec -i togewire sh -c 'cat > /app/data/cookies.txt && chmod 600 /app/data/cookies.txt' < path/to/cookies.txt
```
togewire automatically detects `cookies.txt` (or any `*cookie*.txt`) in `data/`, including when added or replaced while running. See [Are you a bot?](https://github.com/kurodaze/togematch#are-you-a-bot) in the togematch README for how to export cookies from a spare YouTube account.

### 2. Or Install Native Binary

Download a static `togewire` binary (`-x86_64` / `-aarch64`) from [GitHub Releases](https://github.com/kurodaze/togewire/releases) and check it with `sha256sum -c --ignore-missing SHA256SUMS`, or build it from source:

```bash
# Runtime: ffmpeg, plus rsgain if normalization is on. yt-dlp, and deno unless one is on
# PATH, are downloaded into data/bin/ on first use; yt-dlp is kept current. Building needs Rust.
sudo apt install ffmpeg rsgain
git clone https://github.com/kurodaze/togewire.git
cd togewire
./scripts/build.sh --release   # a static ./togewire, see --help
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
3. In the admin dashboard, click **Pair Spotify** in the header and confirm authorization in the Spotify tab.
4. When you're ready for listeners, turn on the **Player** switch at the top of `/admin` to enable `/player`.

---

## Configuration

Place togewire behind a reverse proxy (such as [KDNX](https://github.com/kurodaze/kdnx) or a Cloudflare Tunnel) for HTTPS. `X-Forwarded-For` and `X-Forwarded-Proto` are trusted from loopback/private peers only (cloudflared, KDNX or a KDNX connector); the client is the first public address from the right of `X-Forwarded-For`, so proxies chained on the same host or LAN still resolve to the real visitor. Login rate limits count per IPv4 address or IPv6 /64.

Key options in `configs/config.json` (also adjustable in `/admin`):
- `player_enabled` (`false`): Enable public player at `/player` and `/stream`.
- `cache_enabled` (`false`): Save tracks played on Spotify to disk in `data/track_cache/`. When off, the player prepares tracks in memory (`/dev/shm`) only; when both `player_enabled` and `cache_enabled` are `false`, Spotify playback is not polled.
- `audio_source` (`spotify`): Source for played/cached tracks when paired with Premium (`spotify` or `youtube`; Direct download has its own independent source selector).
- `cache_max_mb` (`2048`): Maximum disk cache size in `data/track_cache/`. *(To use an external drive or NFS share, symlink it: `ln -s /mnt/nas/music data/track_cache`)*. Use **View cache** in `/admin` to browse, search, filter by quality/source, inspect cover art, and download (single `.ogg` or `.zip`) or delete cached tracks (including Shift+click range selection).
- `audio_bitrate` (`144k`): Opus VBR bitrate for newly prepared tracks (`112k`, `144k`, `192k`); YouTube's own Opus is kept as it is.
- `low_data_streaming` (`false`): Streams 112k copies for any track prepared above 112k (including 144k, 192k, and YouTube's ~160k Opus).
- `normalization` (`off`): `stream` normalizes streams in memory without touching cached files; `cache` tags newly cached files with ReplayGain 2.0 without re-encoding.
- `togematch_api` (empty): optional [togematch-api](https://github.com/kurodaze/togematch) on your network, e.g. `http://192.168.1.10:7094` (or `http://host.docker.internal:7094` if togewire runs in Docker and togematch-api is on the host or in an independent container publishing `7094:7094`; or `http://togematch-api:7094` if sharing a Docker network). It picks the YouTube video for yt-dlp tracks and keeps its matches.
- `cover_art` (`640`): `640` embeds the largest cover Spotify's metadata lists; `1280` fetches the original uploaded cover and downscales it to at most 1280 px; `original` embeds the cover as uploaded (often 1400 to 3000 px, a few hundred KB more per track) and falls back to 640 (config file only).
- `log_level` (`info`): `trace`, `debug`, `info`, `warn` or `error` (config file only).

### Audio, Metadata & Cover Art

- **Spotify audio (`librespot`)**: Requests the highest format Spotify's key server grants the account: bit-perfect FLAC (up to 24-bit) on Lossless-eligible Premium plans, else 320 kbps Ogg Vorbis, else lower. Formats refused by the plan are skipped for the rest of the session.
- **YouTube audio (`yt-dlp`)**: Remuxes YouTube's native Opus stream without re-encoding, discarding YouTube's own title, uploader, and thumbnail when catalogue metadata is available.
- **Opus caching**: Cached tracks are stored as Opus VBR (`.ogg`, `-vbr on`) at `audio_bitrate` (`112k` Normal, `144k` High default, `192k` Very High). Sourcing from lossless FLAC avoids generational loss.
- **Low-data & normalization**: `low_data_streaming` keeps up to 3 transcoded 112k copies in `/dev/shm` (~12 MB RAM) for any track cached above `112k`. `normalization = "stream"` runs `rsgain` (−14 LUFS / −1 dBTP) in memory and pre-normalizes the next queued track; `"cache"` writes lossless ReplayGain 2.0 tags to newly cached files.
- **Metadata & cover art (works with or without Spotify auth)**: Every cached or downloaded file is tagged with title, credited artists, album, album artist, release year, track/disc numbers, and embedded 1:1 front cover art (`METADATA_BLOCK_PICTURE`):
  - **Spotify tracks**: Always use official Spotify metadata and 1:1 album art (spclient metadata API when paired, or Spotify's public embed page + oEmbed when unpaired; upgraded per `cover_art`).
  - **YouTube tracks**: Matched against `togematch-api` (when configured, skipped for official `"Provided to YouTube by"` Art Tracks), Spotify (when paired), and public Deezer / iTunes Search (when unpaired) for canonical metadata and high-res 1:1 album art. For YouTube-only uploads (bootlegs, remixes), symmetric outer black bars are trimmed and the thumbnail is center-cropped to a 1:1 JPEG.

### Direct Download (`/admin`)

Available in `/admin` even without pairing Spotify:

- **Accepted links**: Spotify tracks, albums, and playlists (`open.spotify.com/{track,album,playlist}/...` or `spotify:{track,album,playlist}:...`), plus YouTube and YouTube Music videos, albums, and playlists (`youtube.com/watch`, `youtu.be`, `youtube.com/playlist`, `music.youtube.com/browse`).
- **Source & quality**: With Spotify Premium paired (`librespot` enabled), choose **Spotify** or **YouTube** for Spotify links (independent of `audio_source`), and pick **Max** (raw FLAC or 320 kbps Ogg Vorbis, no re-encoding) or an Opus VBR tier (`112k`–`192k`). For YouTube links or when unpaired/Free, audio comes from `yt-dlp` (`bestaudio/best`, using `togematch-api` when configured) and native Opus is remuxed losslessly.
- **Tagging & normalization**: Every file gets full metadata and 1:1 cover art as described above (regardless of Spotify auth), plus an optional **Normalize** toggle for ReplayGain 2.0 (−14 LUFS / −1 dBTP) via `rsgain`.
- **Queue & staging**: Runs sequentially (up to 20 jobs, 500 tracks per album/playlist) with **Pause**/**Resume**, **Stop** (packages tracks finished so far), and **Remove**/**Cancel**/**Dismiss**. Multi-track jobs are packaged into an uncompressed (`STORE`) `.zip` in `data/track_cache/manual-<id>.ytwork/` (so large downloads use `data/` rather than RAM/`/tmp`). Ready files stay downloadable after closing the browser until dismissed, after 48 hours, or on restart.

---

## Disclaimer & Acknowledgements

togewire is **not affiliated with Spotify or YouTube**.

togewire uses [FFmpeg](https://ffmpeg.org/) for audio processing, [librespot](https://github.com/librespot-org/librespot) for Spotify-native track fetch, [yt-dlp](https://github.com/yt-dlp/yt-dlp) with [Deno](https://deno.com/) as a YouTube matcher, and [rsgain](https://github.com/complexlogic/rsgain) for ReplayGain 2.0 loudness tagging.

## License

Copyright (C) 2026 KuroDaze. Licensed under the GNU General Public License v3.0 or later, see [LICENSE](LICENSE).

Plus Jakarta Sans (bundled under `web/static/fonts/`) is licensed separately under the SIL Open Font License - see `web/static/fonts/OFL.txt`.
