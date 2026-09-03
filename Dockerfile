# Stage 1: Build the optimized release binary
FROM rust:1.85-slim-bookworm AS builder

WORKDIR /usr/src/wsblast

# Install build dependencies required for ring
RUN apt-get update && apt-get install -y --no-install-recommends \
    build-essential \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# Copy dependency manifests first for build caching
COPY Cargo.toml Cargo.lock ./

# Copy complete source code
COPY src ./src
COPY examples ./examples

# Build the production release binary
RUN cargo build --release --locked

# Stage 2: Minimal runtime environment
FROM debian:bookworm-slim AS runtime

# Install CA certificates for secure wss:// TLS certificate validation
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# Copy binary from builder stage
COPY --from=builder /usr/src/wsblast/target/release/wsblast /usr/local/bin/wsblast

# Run as a non-privileged user for container security
USER 1000:1000

ENTRYPOINT ["wsblast"]
CMD ["--help"]
