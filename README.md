# TOGEWIRE

A high-performance, self-hosted service that enables real-time Spotify listening sessions via a web interface. Synchronize playback with guests using YouTube audio sources, optimized for low latency and minimal resource usage.

> [View Demo Video](https://kurodaze.net/assets/togewiredemo.mp4)

## Features

- **Real-time Synchronization**: WebSocket-based state management ensures listeners hear exactly what you hear.
- **Efficient Audio**: Opus codec streaming with configurable file-based caching.
- **Smart Pre-fetching**: Background processing of upcoming tracks using `yt-dlp` and `ffmpeg`.
- **Resilient**: Automatic `yt-dlp` management and self-healing capabilities.

## Prerequisites

- **System**: 1 vCPU, ~70MB RAM plus ~90MB per active download (maximum 2 concurrent).
- **Network**: Residential IP recommended to avoid YouTube datacenter blocks.
- **Software**: `ffmpeg` (required for native deployment).
- **Spotify Premium**: Recommended for uninterrupted playback synchronization.

## Installation

### Docker (Recommended)

```bash
docker-compose up -d
```

*Alternatively, run directly:*

```bash
docker run -d -p 7093:7093 \
  -v ./configs:/app/configs \
  -v ./data:/app/data \
  togewire
```

### Manual Deployment

1.  Install `ffmpeg`.
2.  Download the latest binary from [GitHub Releases](https://github.com/kurodaze/togewire/releases).
3.  Run `./togewire`.

## Configuration

1.  Create an app in the [Spotify Developer Dashboard](https://developer.spotify.com/dashboard).
2.  Set the Redirect URI to `http://<your-ip>:7093/callback` or your domain (e.g., `https://togewire.domain.tld/callback`).
3.  Launch Togewire and input your credentials via the web interface.
4.  *(Optional)* Use Cloudflare Tunnel for secure external access.

## Usage

- **Player**: Navigate to `/player` to join the session.
- **Embed**: Refer to `web/togeplayer-iframe/iframe.html` for iframe implementation details.

## Disclaimer

TOGEWIRE is an independent project and is **not affiliated with, endorsed by, or sponsored by Spotify, YouTube, or Google**. This project utilizes [yt-dlp](https://github.com/yt-dlp/yt-dlp) for content retrieval and [FFmpeg](https://ffmpeg.org/) for audio processing.

## License

Licensed under the GNU Affero General Public License v3.0 (AGPL-3.0). See `LICENSE` for details.
