# Stage 1: base - Testing and linting (used by Jenkins)
FROM rustlang/rust:nightly AS base

WORKDIR /usr/src/app

# Copy workspace files
COPY Cargo.toml Cargo.lock ./
COPY crates/ ./crates/

# Install rustfmt and clippy
RUN rustup component add rustfmt clippy

# Run formatting check, tests, and linting
# Note: These commands will fail the build if any check fails
# --lib runs only unit tests (in src/), skips integration tests (in tests/ dirs)
# Integration tests require running servers and are executed separately via run_tests.sh
RUN cargo fmt --all -- --check
RUN cargo test --workspace --lib
RUN cargo clippy --all-targets --all-features -- -D warnings

# Stage 2: release-build - Release compilation
FROM rustlang/rust:nightly AS release-build

WORKDIR /usr/src/app

# Copy workspace files
COPY Cargo.toml Cargo.lock ./
COPY crates/ ./crates/

# Build the web server binary in release mode
RUN cargo clean
RUN cargo build --release --bin aura-web-server

# Stage 3: release - Runtime stage with newer glibc
FROM debian:trixie-slim

# Install runtime dependencies
RUN apt-get update && apt-get install -y \
    ca-certificates \
    libssl3 \
    curl \
    && rm -rf /var/lib/apt/lists/*

# Create app user for security
RUN useradd -r -s /bin/false appuser

# Create app directory and config directory
WORKDIR /app
RUN mkdir -p /app/config && chown -R appuser:appuser /app

# Copy binary from release-build stage
COPY --from=release-build /usr/src/app/target/release/aura-web-server /app/

# Switch to app user
USER appuser

# Expose port
EXPOSE 3030

# Set default environment variables
ENV HOST=0.0.0.0
ENV PORT=3030
ENV CONFIG_PATH=/app/config/config.toml

# Health check
HEALTHCHECK --interval=30s --timeout=10s --start-period=5s --retries=3 \
    CMD curl -f http://localhost:3030/health || exit 1

# Run the web server
CMD ["./aura-web-server"]