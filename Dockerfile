# hadolint global ignore=DL3008
# Compiler cache for cargo invocations that run outside the layer cache
# (mounted-workspace builds, the in-container coverage compile). Opt-in via
# RUSTC_WRAPPER / RUSTC_WORKSPACE_WRAPPER; inert when those are unset.
ARG SCCACHE_VERSION=0.16.0
ARG SCCACHE_SHA256=aec995a83ad3dff3d14b6314e08858b7b73d35ca85a5bcf3d3a9ec07dee35588

### 000 Chef
FROM lukemathwalker/cargo-chef:latest-rust-1.95@sha256:00c3c07c51d092325df88f0df2d626cd4302e12933f179ba154509cc314d6c2a AS chef

### 001 Core
# sentencepiece-sys builds its C++ library from source.
FROM chef AS core
RUN apt-get update && apt-get install -y --no-install-recommends \
    cmake g++ g++-aarch64-linux-gnu \
  && rm -rf /var/lib/apt/lists/*

### 002 Sccache
# Download once; runner and test-tools COPY the binary from here.
FROM core AS sccache-dl
ARG SCCACHE_VERSION
ARG SCCACHE_SHA256
RUN <<EOR
  set -e
  curl -fsSL -o /tmp/sccache.tar.gz "https://github.com/mozilla/sccache/releases/download/v${SCCACHE_VERSION}/sccache-v${SCCACHE_VERSION}-x86_64-unknown-linux-musl.tar.gz"
  printf '%s  /tmp/sccache.tar.gz\n' "${SCCACHE_SHA256}" > /tmp/sccache.tar.gz.sha256
  sha256sum -c /tmp/sccache.tar.gz.sha256
  tar -xzf /tmp/sccache.tar.gz --strip-components=1 -C /usr/local/bin --wildcards '*/sccache'
  rm /tmp/sccache.tar.gz /tmp/sccache.tar.gz.sha256
  sccache --version
EOR

### 003 Planner
# Manifests are the dependency cache key; source is copied after cook.
FROM core AS planner
WORKDIR /usr/src/app
COPY Cargo.toml Cargo.lock ./
COPY crates/aura/Cargo.toml         crates/aura/Cargo.toml
COPY crates/aura-cli/Cargo.toml     crates/aura-cli/Cargo.toml
COPY crates/aura-config/Cargo.toml  crates/aura-config/Cargo.toml
COPY crates/aura-events/Cargo.toml  crates/aura-events/Cargo.toml
COPY crates/aura-telemetry/Cargo.toml crates/aura-telemetry/Cargo.toml
COPY crates/aura-telemetry-derive/Cargo.toml crates/aura-telemetry-derive/Cargo.toml
COPY crates/aura-test-utils/Cargo.toml crates/aura-test-utils/Cargo.toml
COPY crates/aura-web-server/Cargo.toml crates/aura-web-server/Cargo.toml
# cargo chef needs a target file for every workspace manifest.
RUN for crate in aura aura-cli aura-config aura-events aura-telemetry aura-telemetry-derive aura-test-utils aura-web-server; do \
      mkdir -p "crates/$crate/src" && \
      printf 'pub fn _chef_stub() {}\n' > "crates/$crate/src/lib.rs"; \
    done && \
    for crate in aura-cli aura-web-server; do \
      printf 'fn main() {}\n' > "crates/$crate/src/main.rs"; \
    done
RUN cargo chef prepare --recipe-path recipe.json

# Separate cooks isolate the debug/coverage and release fingerprints.

### 004 Cook-debug
# The single dependency cook behind every CI compile on change requests.
FROM core AS cook-debug
WORKDIR /usr/src/app
COPY --from=planner /usr/src/app/recipe.json recipe.json
ENV RUSTFLAGS="--allow=warnings -Cinstrument-coverage"
ENV CARGO_TARGET_DIR=/usr/src/app/target
RUN cargo chef cook --workspace --all-targets --features integration --recipe-path recipe.json

### 005 Runner
# Mounted-workspace Cargo commands need the native dependencies from core.
FROM core AS runner

RUN groupadd --gid 1000 aura \
  && useradd --uid 1000 --gid aura --shell /bin/bash --create-home aura

ENV PATH="${PATH}:/home/aura/.bin"
WORKDIR /home/aura

RUN <<EOR
  set -e
  apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates libssl3 curl nodejs npm \
    libc6-dev-arm64-cross
  rm -rf /var/lib/apt/lists/*
EOR

# Pinned STDIO integration fixture.
RUN npm install -g @modelcontextprotocol/server-everything@2026.1.26 \
  && command -v mcp-server-everything

COPY --from=sccache-dl /usr/local/bin/sccache /usr/local/bin/sccache

USER 1000

RUN <<EOR
  set -e
  rustup component add rustfmt clippy llvm-tools
  rustup component add --toolchain nightly-x86_64-unknown-linux-gnu rustfmt
EOR

### 006 Test-tools
# Install tools before source COPY so source changes keep this layer cached.
FROM cook-debug AS test-tools

USER root
# cook-debug writes these paths as root.
RUN chown -R 1000:1000 /usr/local/cargo /usr/src/app

COPY --from=sccache-dl /usr/local/bin/sccache /usr/local/bin/sccache

USER 1000
# Prebuilt tool binaries — nothing compiles from source in the PR image.
# ARG defaults cover direct docker builds; make build-images passes the
# pins from .makefiles/rust.mk, the source of truth.
ARG NEXTEST_VERSION=0.9.133
ARG GRCOV_VERSION=v0.10.7
RUN <<EOR
  set -e
  curl -fsSL -o /tmp/nextest.tar.gz "https://get.nexte.st/${NEXTEST_VERSION}/linux"
  tar -xzf /tmp/nextest.tar.gz -C /usr/local/cargo/bin
  curl -fsSL -o /tmp/grcov.tar.bz2 "https://github.com/mozilla/grcov/releases/download/${GRCOV_VERSION}/grcov-x86_64-unknown-linux-gnu.tar.bz2"
  tar -xjf /tmp/grcov.tar.bz2 -C /usr/local/cargo/bin
  rm -f /tmp/nextest.tar.gz /tmp/grcov.tar.bz2
  chmod +x /usr/local/cargo/bin/cargo-nextest /usr/local/cargo/bin/grcov
  cargo nextest --version
  grcov --version
EOR

### 007 Test
# Keep this setup in sync with runner.
FROM test-tools AS test

USER root
RUN groupadd --gid 1000 aura && useradd --uid 1000 --gid aura --shell /bin/bash --create-home aura

RUN <<EOR
  set -e
  apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates libssl3 curl nodejs npm
  rm -rf /var/lib/apt/lists/*
EOR

# Pinned STDIO integration fixture.
RUN npm install -g @modelcontextprotocol/server-everything@2026.1.26 \
  && command -v mcp-server-everything

USER aura

# grcov reads llvm-tools' profdata; lint + nightly rustfmt are runner-only.
RUN rustup component add llvm-tools

WORKDIR /home/aura
# No .git: worktree pointer files are invalid inside the image.
COPY --chown=aura:aura Cargo.toml Cargo.lock ./
COPY crates/ ./crates/
COPY .makefiles ./.makefiles
COPY .config.mk ./
COPY Makefile ./

### 008 Lint-test
# Local-dev target only; CI lints through the runner image and tests through compose.
FROM core AS lint-test

WORKDIR /usr/src/app
COPY Cargo.toml Cargo.lock ./
COPY crates/ ./crates/

# --lib skips integration tests, which require compose services.
RUN <<EOR
  set -e
  rustup component add rustfmt clippy
  cargo fmt --all -- --check
  cargo clippy --all-targets --all-features -- -D warnings
  cargo test --workspace --lib
EOR

### 009 Debug-build
# Server binary for the integration lane. --workspace keeps the cook's
# feature union, and the inherited RUSTFLAGS keep its fingerprints, so
# only workspace crates compile here.
FROM cook-debug AS debug-build
WORKDIR /usr/src/app
COPY Cargo.toml Cargo.lock ./
COPY crates/ ./crates/

RUN cargo build --workspace --bin aura-web-server

### 010 Cook-release
FROM core AS cook-release
WORKDIR /usr/src/app
COPY --from=planner /usr/src/app/recipe.json recipe.json
ENV CARGO_TARGET_DIR=/usr/src/app/target
RUN cargo chef cook --release --bin aura-web-server --recipe-path recipe.json \
 && cargo chef cook --release -p aura-cli --bin aura --recipe-path recipe.json

### 011 Release-build
# Source changes only recompile workspace crates.
FROM cook-release AS release-build
WORKDIR /usr/src/app
COPY Cargo.toml Cargo.lock ./
COPY crates/ ./crates/

RUN <<EOR
  set -e
  cargo build --release --bin aura-web-server
  cargo build --release -p aura-cli --bin aura
EOR

### 012 Runtime
# Shared runtime shell; server and release differ only in the binaries they carry.
FROM debian:trixie-slim AS runtime

RUN <<EOR
  set -e
  apt-get update && apt-get install -y --no-install-recommends ca-certificates libssl3 curl
  rm -rf /var/lib/apt/lists/*
  useradd -r -s /bin/false appuser
EOR

WORKDIR /app
RUN mkdir -p /app/config /app/skills && chown -R appuser:appuser /app

USER appuser
EXPOSE 3030

ENV HOST=0.0.0.0
ENV PORT=3030
ENV CONFIG_PATH=/app/config/config.toml

HEALTHCHECK --interval=30s --timeout=10s --start-period=5s --retries=3 \
  CMD curl -f http://localhost:3030/health || exit 1

CMD ["./aura-web-server"]

### 013 Server
# Integration-lane server image (debug profile).
FROM runtime AS server

COPY --from=debug-build /usr/src/app/target/debug/aura-web-server /app/

### 014 Release
# Published image. Must stay the final stage: the feature-build and
# post-merge publish lanes build the default target.
FROM runtime AS release

COPY --from=release-build /usr/src/app/target/release/aura-web-server /app/
COPY --from=release-build /usr/src/app/target/release/aura /app/
