# Build stage
FROM rust:bookworm AS builder

WORKDIR /app
COPY . .
RUN cargo build --release

# Final run stage
FROM debian:bookworm-slim AS runner

# Install OpenSSL (required for TLS connections)
RUN apt-get update && apt-get install -y \
    ca-certificates \
    libssl3 \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app
COPY --from=builder /app/target/release/jetxv1 /app/jetxv1

# Koyeb requires the app to listen on PORT, but this is a WebSocket client
# We'll just run the WebSocket monitor
CMD ["/app/jetxv1"]