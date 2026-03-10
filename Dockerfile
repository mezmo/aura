# hadolint global ignore=DL3008
### 000 Rust
FROM rust:1.95 AS core

### 001 Runner
FROM core AS runner

RUN groupadd --gid 1000 aura \
  && useradd --uid 1000 --gid aura --shell /bin/bash --create-home aura

ENV PATH="${PATH}:/home/aura/.bin"
WORKDIR /home/aura

RUN <<EOR
  dpkg --add-architecture arm64
  apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates libssl3 curl nodejs npm \
    gcc-aarch64-linux-gnu libc6-dev-arm64-cross libssl-dev:arm64
  rm -rf /var/lib/apt/lists/*
EOR

# Install pinned MCP test fixture for STDIO integration tests.
# Verified at build time; tests invoke `mcp-server-everything` directly.
RUN npm install -g @modelcontextprotocol/server-everything@2026.1.26 \
  && command -v mcp-server-everything

USER 1000

RUN <<EOR
  rustup component add rustfmt clippy llvm-tools
  rustup component add --toolchain nightly-x86_64-unknown-linux-gnu rustfmt
EOR

### 0002 test
FROM runner AS test

# Copy workspace files
COPY --chown=aura:aura Cargo.toml Cargo.lock ./
COPY crates/ ./crates/
# needed for make operations
COPY .git ./.git
COPY .makefiles ./.makefiles
COPY .config.mk .config.mk ./
COPY Makefile Makefile ./

RUN <<EOR
  git config --global --add safe.directory /home/aura
  cargo install --locked --force cargo-nextest
  cargo install --locked grcov
EOR

# 003: linting & testing
FROM rust:1.95 AS release-lint-test

WORKDIR /usr/src/app

# Copy workspace files
COPY Cargo.toml Cargo.lock ./
COPY crates/ ./crates/

# Install rustfmt and clippy
# Run formatting check, tests, and linting
# Note: These commands will fail the build if any check fails
# --lib runs only unit tests (in src/), skips integration tests (in tests/ dirs)
# Integration tests require running servers and are executed separately via run_tests.sh
RUN <<EOR
  rustup component add rustfmt clippy
  cargo fmt --all -- --check
  cargo clippy --all-targets --all-features -- -D warnings
  cargo test --workspace --lib
EOR


# 004: release-build - Release compilation
FROM core AS release-build

WORKDIR /usr/src/app

# Copy workspace files
COPY Cargo.toml Cargo.lock ./
COPY crates/ ./crates/

# Build the web server and CLI binaries in release mode
RUN <<EOR
  cargo clean
  cargo build --release --bin aura-web-server
  cargo build --release -p aura-cli --bin aura-cli
EOR

# 005: release - Runtime stage with newer glibc
FROM debian:trixie-slim AS release

# Install runtime dependencies
# Create app user for security
RUN <<EOR
  apt-get update && apt-get install -y --no-install-recommends ca-certificates libssl3 curl
  rm -rf /var/lib/apt/lists/*
  useradd -r -s /bin/false appuser
EOR

# Create app directory, config directory, and skills directory
WORKDIR /app
RUN mkdir -p /app/config /app/skills && chown -R appuser:appuser /app

# Copy binaries from release-build stage
COPY --from=release-build /usr/src/app/target/release/aura-web-server /app/
COPY --from=release-build /usr/src/app/target/release/aura-cli /app/

# Switch to app user
USER appuser

# Expose port
EXPOSE 3030

# Set default environment variables
ENV HOST=0.0.0.0
ENV PORT=3030
ENV CONFIG_PATH=/app/config/config.toml
ENV AURA_SKILLS_DIR=/app/skills

# Health check
HEALTHCHECK --interval=30s --timeout=10s --start-period=5s --retries=3 \
  CMD curl -f http://localhost:3030/health || exit 1

# Run the web server
CMD ["./aura-web-server"]
