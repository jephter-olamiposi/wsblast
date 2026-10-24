# Stage 1: Build the optimized release binary
FROM rust:1.88-slim-bookworm AS builder

WORKDIR /usr/src/wsblast

# ring (the rustls crypto provider) needs a C toolchain
RUN apt-get update && apt-get install -y --no-install-recommends \
    build-essential \
    && rm -rf /var/lib/apt/lists/*

# Cargo.toml declares a [[bench]] target, so Cargo needs more than src/ to load
# the manifest; .dockerignore trims the context.
COPY . .

RUN cargo build --release --locked --bin wsblast

# Stage 2: Minimal runtime environment
FROM debian:bookworm-slim AS runtime

# rustls verifies wss:// servers against the system CA bundle
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# Copy binary from builder stage
COPY --from=builder /usr/src/wsblast/target/release/wsblast /usr/local/bin/wsblast

# Run as a non-privileged user for container security
USER 1000:1000

ENTRYPOINT ["wsblast"]
CMD ["--help"]
