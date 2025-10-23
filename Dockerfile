# Build stage
FROM golang:1.24-alpine AS builder

# Install build dependencies
RUN apk add --no-cache git

WORKDIR /build

# Copy go mod files
COPY go.mod go.sum ./
RUN go mod download

# Copy source code
COPY . .

# Build the binary
RUN CGO_ENABLED=0 go build -ldflags="-w -s" -o togewire ./cmd/togewire/main.go

# Runtime stage
FROM alpine:latest

# Install ffmpeg and ca-certificates
RUN apk add --no-cache \
    ffmpeg \
    ca-certificates \
    tzdata

WORKDIR /app

# Copy the binary from builder
COPY --from=builder /build/togewire .

# Create necessary directories
RUN mkdir -p /app/configs /app/data/youtube_cache

# Copy config example (optional, can be mounted as volume)
COPY configs/config.example.json /app/configs/

# Expose the default port
EXPOSE 7093

# Set the entrypoint
ENTRYPOINT ["/app/togewire"]

# Default command arguments (can be overridden)
CMD ["--host", "0.0.0.0", "--port", "7093"]
