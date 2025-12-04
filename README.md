# TOGEWIRE

Share your Spotify listening session with anyone via your own website.

## Demo

> [demo video](https://kurodaze.net/assets/togewiredemo.mp4)

## Requirements

- **CPU**: 1 vCPU
- **RAM**: ~70 MB + ~90 MB per yt-dlp process
- **Storage**: depends on cache limit, 240 tracks ~1 GB
- **ffmpeg** (required for audio processing; must be in PATH)

**Important:** This application is intended for local/residential deployment. YouTube may block requests from datacenter IP addresses.

**Note:** Listen-along audio streaming works best with Spotify Premium for ad-free playback.

## Deployment

### Cloudflare Tunnel
For secure and easy external access, you may use Cloudflare Tunnel to expose your locally-running TOGEWIRE server:

**Recommended domain format**: `https://togewire.domain.tld`

## Quick Setup

### Docker (recommended)

```bash
docker-compose up -d
```

Or run directly:
```bash
docker run -d -p 7093:7093 -v ./configs:/app/configs -v ./data:/app/data togewire
```

### Native

1. **Install dependencies**
   ```bash
   sudo apt update && sudo apt install ffmpeg
   ```

2. **Download and run**
   - Download the latest release from [GitHub Releases](https://github.com/kurodaze/togewire/releases)
   - run: `./togewire`

### Configuration

1. **Setup Spotify OAuth**
   - Visit [Spotify Developer Dashboard](https://developer.spotify.com/dashboard)
   - Create a new app
   - Set redirect URI to: `http://ip:7093/callback` or `https://togewire.domain.tld/callback`
   - Copy your Spotify credentials and configure via web interface

**Note:** Configuration is in `configs/config.json` (auto-generated on first run). See `configs/config.example.json` for reference.

### Usage

Visit `togewire.domain.tld/player` to see the current track and click "join" to listen along

### Embedding the Player

to embed the player in your website, see the iframe code at `web/togeplayer-iframe/iframe.html`.

## Architecture and features

TOGEWIRE is a Golang project built for high performance and low resource usage

- **Backend**: Gin web framework
- **WebSocket**: Gorilla WebSocket for real-time communication  
- **Audio Processing**: yt-dlp + ffmpeg for YouTube audio extraction
- **Audio Format**: Opus codec for optimal quality and storage efficiency
- **Frontend**: Vanilla JavaScript
- **Caching**: configurable File-based audio cache
- **Real-time sync**: WebSocket-based audio synchronization with Spotify
- **Track preparation**: Background preparation of upcoming tracks
- **Auto-updates**: Automatic yt-dlp management with self-healing on failures
- **Responsive UI**: Works on desktop and mobile devices

## Notice
- TOGEWIRE is an independent project and is **not affiliated with, endorsed by, or sponsored by Spotify, YouTube, or Google**.
- This project uses [yt-dlp](https://github.com/yt-dlp/yt-dlp) for YouTube audio extraction.
- Audio processing powered by [FFmpeg](https://ffmpeg.org/).

## License

This project is licensed under the GNU Affero General Public License v3.0 (AGPL-3.0).
See the `LICENSE` file in this repository for the full license text.
