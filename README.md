# TOGEWIRE

Share your Spotify listening session with anyone via your own website.

## Demo

> [demo video](https://main.kurodaze.pages.dev/assets/togewiredemo.mp4)

## Requirements

- **CPU**: 1 vCPU
- **RAM**: ~55 MB + ~90 MB per ytdlp process
- **Storage**: depends on cache limit, 240 tracks ≈ 1 GB
- **ffmpeg** (required for audio processing; must be in PATH)
- **Golang 1.21+** *(only required if building from source)

**Important:** This application is intended for local/residential deployment. YouTube may block requests from datacenter IP addresses.

**Note:** Listen-along audio streaming works best with Spotify Premium for ad-free playback.

## Deployment

### Cloudflare Tunnel
For secure and easy external access, you may use Cloudflare Tunnel to expose your locally-running TOGEWIRE server:

**Recommended domain format**: `https://togewire.domain.tld`

## Quick Setup

### Docker

```bash
docker-compose up -d
```

Or build and run directly:
```bash
docker build -t togewire .
docker run -d -p 7093:7093 -v ./configs:/app/configs -v ./data:/app/data togewire
```

### Native

1. **Install ffmpeg**
   ```bash
   # Ubuntu/Debian
   sudo apt update && sudo apt install ffmpeg
   
   # Windows
   winget install ffmpeg
   
   # macOS
   brew install ffmpeg
   ```

2. **Run**
   ```bash
   go build -o togewire cmd/togewire/main.go
   ./togewire
   ```

3. **Configure**
   - Visit [Spotify Developer Dashboard](https://developer.spotify.com/dashboard)
   - Create a new app
   - Set redirect URI to: `http://ip:7093/callback` or `https://togewire.domain.tld/callback`
   - copy your Spotify credentials
   - Authenticate with Spotify

### Configuration
- All configuration settings are stored in `configs/config.json` (auto-generated on first run)
- An example configuration is available in `configs/config.example.json`
- To update your configuration, delete the `config.json` file and restart the server to generate a new one.

### Usage

Visit `togewire.domain.tld/player` to see the current track and click "join" to listen along

### Embedding the Player

to embed the player in your website, see the iframe code at `web/togeplayer-iframe/iframe.html`.

## Architecture and features

TOGEWIRE is built in Golang for high performance and low resource usage:

- **Backend**: Go with Gin web framework
- **WebSocket**: Gorilla WebSocket for real-time communication  
- **Audio Processing**: yt-dlp + ffmpeg for YouTube audio extraction
- **Spotify Integration**: Web API with OAuth2 authentication
- **Frontend**: Vanilla JavaScript with WebSocket streaming
- **Caching**: File-based audio cache with configurable size limits

- **Real-time sync**: WebSocket-based audio synchronization with Spotify
- **Smart caching**: Intelligent audio file caching with cleanup
- **Track preparation**: Background preparation of upcoming tracks
- **Audio streaming**: Chunked audio streaming over WebSocket
- **Auto-updates**: Automatic yt-dlp management with self-healing on failures
- **Responsive UI**: Works on desktop and mobile devices

## Notice
- TOGEWIRE is an independent project and is **not affiliated with, endorsed by, or sponsored by Spotify, YouTube, or Google**.
- This project uses [yt-dlp](https://github.com/yt-dlp/yt-dlp) for YouTube audio extraction.
- Audio processing powered by [FFmpeg](https://ffmpeg.org/).

## License

This project is licensed under the GNU Affero General Public License v3.0 (AGPL-3.0).
See the `LICENSE` file in this repository for the full license text.
