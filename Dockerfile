# Build stage
FROM --platform=$BUILDPLATFORM golang:1.25.5-alpine3.23 AS builder

ARG TARGETOS=linux
ARG TARGETARCH=amd64

WORKDIR /build

# Copy go mod files
COPY go.mod go.sum ./
RUN go mod download

# Copy source code
COPY . .

# Build the binary for the requested target
RUN CGO_ENABLED=0 GOOS=$TARGETOS GOARCH=$TARGETARCH go build \
    -ldflags="-w -s" \
    -trimpath \
    -gcflags="all=-l" \
    -o togewire ./cmd/togewire/main.go

# Runtime stage
FROM alpine:3.23

# Install runtime dependencies
RUN apk add --no-cache \
    ffmpeg \
    ca-certificates

WORKDIR /app

# Copy the binary from builder
COPY --from=builder /build/togewire .

# Create necessary directories
RUN mkdir -p /app/configs /app/data/youtube_cache /app/bin

# Copy config example (optional, can be mounted as volume)
COPY configs/config.example.json /app/configs/

# Expose the default port
EXPOSE 7093

# Set the entrypoint
ENTRYPOINT ["/app/togewire"]

# Default command arguments (can be overridden)
CMD ["--host", "0.0.0.0", "--port", "7093"]
