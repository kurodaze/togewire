# Bump the Rust and Alpine versions together; ffmpeg, rsgain and deno come from Alpine's packages.
FROM rust:1.99-alpine3.24 AS builder
# `dev` for a faster, unoptimized debug build (see compose.dev.yaml).
ARG PROFILE=release
WORKDIR /src
RUN apk add --no-cache build-base
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY web ./web
RUN cargo build --profile "$PROFILE" --locked --bin togewire && \
    cp "target/$([ "$PROFILE" = dev ] && echo debug || echo "$PROFILE")/togewire" /togewire

FROM alpine:3.24
# Deno is packaged here because its releases are glibc-only; yt-dlp fetches its musl build.
RUN apk add --no-cache ca-certificates tzdata ffmpeg rsgain deno && \
    addgroup -g 1000 togewire && \
    adduser -u 1000 -G togewire -s /bin/sh -D togewire

WORKDIR /app
COPY --from=builder /togewire /app/togewire
COPY LICENSE /app/LICENSE
RUN mkdir -p /app/configs /app/data && chown -R togewire:togewire /app

USER togewire
EXPOSE 7093
HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
    CMD wget -qO /dev/null http://127.0.0.1:7093/ping || exit 1

VOLUME ["/app/configs", "/app/data"]
ENTRYPOINT ["/app/togewire"]
CMD ["--host", "0.0.0.0", "--port", "7093"]
