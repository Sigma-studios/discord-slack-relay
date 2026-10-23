# ── build ────────────────────────────────────────────────────────────────
FROM rust:1.95-slim AS build
WORKDIR /app
RUN apt-get update && apt-get install -y --no-install-recommends pkg-config && rm -rf /var/lib/apt/lists/*
# Cache deps.
COPY Cargo.toml Cargo.lock* ./
RUN mkdir src && echo "fn main() {}" > src/main.rs && cargo build --release || true
# Real sources.
COPY migrations ./migrations
COPY src ./src
RUN touch src/main.rs && cargo build --release

# ── runtime ────────────────────────────────────────────────────────────────
FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/*
COPY --from=build /app/target/release/discord-slack-relay /usr/local/bin/relay
# Data (SQLite + heartbeat) lives on a façade volume mounted at /data.
VOLUME ["/data"]
ENV HEARTBEAT_FILE=/data/heartbeat
# Healthcheck: heartbeat file touched within the last 60s.
HEALTHCHECK --interval=30s --timeout=5s --start-period=20s --retries=3 \
  CMD test "$(( $(date +%s) - $(stat -c %Y "$HEARTBEAT_FILE" 2>/dev/null || echo 0) ))" -lt 60
ENTRYPOINT ["/usr/local/bin/relay"]
