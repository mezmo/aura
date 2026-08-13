# hadolint global ignore=DL3008
# Compiler cache for cargo invocations that run outside the layer cache
# (mounted-workspace builds, the in-container coverage compile). Opt-in via
# RUSTC_WRAPPER / RUSTC_WORKSPACE_WRAPPER; inert when those are unset.
ARG SCCACHE_VERSION=0.16.0
ARG SCCACHE_SHA256_AMD64=aec995a83ad3dff3d14b6314e08858b7b73d35ca85a5bcf3d3a9ec07dee35588
ARG SCCACHE_SHA256_ARM64=f73a5c39f96bb6ebb89cc7915cf182260d4cbf30765322c5e793d0fe8bd80784

# Zig + cargo-zigbuild give the runner a host-arch-agnostic cross linker, so the
# packaged linux/amd64 and linux/arm64 binaries build on either an amd64 or an
# arm64 runner. cargo-zigbuild 0.23 pairs with zig 0.16.
ARG ZIG_VERSION=0.16.0
ARG ZIG_SHA256_AMD64=70e49664a74374b48b51e6f3fdfbf437f6395d42509050588bd49abe52ba3d00
ARG ZIG_SHA256_ARM64=ea4b09bfb22ec6f6c6ceac57ab63efb6b46e17ab08d21f69f3a48b38e1534f17
ARG CARGO_ZIGBUILD_VERSION=0.23.0
ARG CARGO_ZIGBUILD_SHA256_AMD64=c636e4f72b6f40a40ddf0414c8c6056f78b87eea3be0edf01f08d65fa028a373
ARG CARGO_ZIGBUILD_SHA256_ARM64=5917d5416884cba0f23c2653016f7f2df2ec04e74eb6b259598fecc066f8c429

# nfpm builds the .deb/.rpm release packages from the cross-compiled binaries.
ARG NFPM_VERSION=2.47.0
ARG NFPM_SHA256_AMD64=0660ca602b2d2d2ae4781a06c692b3eeb9d437ffea05b831d76e41f4a3188783
ARG NFPM_SHA256_ARM64=1c0f5f2999b9a974bfb04fdb0cc3306096de530ac5dbb25d739cc5f5219c919c

# The cloudsmith CLI publishes the .deb/.rpm packages.
ARG CLOUDSMITH_VERSION=1.21.0
ARG CLOUDSMITH_SHA256_AMD64=e3729f8fc58e44ae9f7f50af197ab9d99b4b70551b7cbe83cf423d58aadc390b
ARG CLOUDSMITH_SHA256_ARM64=50c3fd0d7486eb9577bd713240c04f3d9d75a9da424ea944a51b105e13e15901

### 000 Chef
FROM lukemathwalker/cargo-chef:latest-rust-1.95@sha256:00c3c07c51d092325df88f0df2d626cd4302e12933f179ba154509cc314d6c2a AS chef

### 001 Core
# sentencepiece-sys builds its C++ library from source.
FROM chef AS core
RUN apt-get update && apt-get install -y --no-install-recommends \
    cmake \
  && rm -rf /var/lib/apt/lists/*

### 002 Sccache
# Download once; runner and test-tools COPY the binary from here.
FROM core AS sccache-dl
ARG SCCACHE_VERSION
ARG SCCACHE_SHA256_AMD64
ARG SCCACHE_SHA256_ARM64
RUN <<EOR
  set -e
  case "$(uname -m)" in
    x86_64)  scc_arch=x86_64;  scc_sha=${SCCACHE_SHA256_AMD64};;
    aarch64) scc_arch=aarch64; scc_sha=${SCCACHE_SHA256_ARM64};;
    *) echo "unsupported build arch: $(uname -m)" >&2; exit 1;;
  esac
  curl -fsSL -o /tmp/sccache.tar.gz "https://github.com/mozilla/sccache/releases/download/v${SCCACHE_VERSION}/sccache-v${SCCACHE_VERSION}-${scc_arch}-unknown-linux-musl.tar.gz"
  printf '%s  /tmp/sccache.tar.gz\n' "${scc_sha}" > /tmp/sccache.tar.gz.sha256
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
    ca-certificates libssl3 curl nodejs npm xz-utils
  rm -rf /var/lib/apt/lists/*
EOR

# Zig toolchain + cargo-zigbuild: the cross linker for the packaged binaries.
# Pinned to the host arch so the runner image builds on amd64 or arm64.
ARG ZIG_VERSION
ARG ZIG_SHA256_AMD64
ARG ZIG_SHA256_ARM64
ARG CARGO_ZIGBUILD_VERSION
ARG CARGO_ZIGBUILD_SHA256_AMD64
ARG CARGO_ZIGBUILD_SHA256_ARM64
RUN <<EOR
  set -e
  case "$(uname -m)" in
    x86_64)  zig_arch=x86_64;  zig_sha=${ZIG_SHA256_AMD64}; cz_target=x86_64-unknown-linux-gnu;  cz_sha=${CARGO_ZIGBUILD_SHA256_AMD64};;
    aarch64) zig_arch=aarch64; zig_sha=${ZIG_SHA256_ARM64}; cz_target=aarch64-unknown-linux-gnu; cz_sha=${CARGO_ZIGBUILD_SHA256_ARM64};;
    *) echo "unsupported build arch: $(uname -m)" >&2; exit 1;;
  esac
  curl -fsSL -o /tmp/zig.tar.xz "https://ziglang.org/download/${ZIG_VERSION}/zig-${zig_arch}-linux-${ZIG_VERSION}.tar.xz"
  printf '%s  /tmp/zig.tar.xz\n' "${zig_sha}" > /tmp/zig.tar.xz.sha256
  sha256sum -c /tmp/zig.tar.xz.sha256
  rm /tmp/zig.tar.xz.sha256
  mkdir -p /opt/zig
  tar -xJf /tmp/zig.tar.xz --strip-components=1 -C /opt/zig
  ln -s /opt/zig/zig /usr/local/bin/zig
  rm /tmp/zig.tar.xz
  zig version
  curl -fsSL -o /tmp/cargo-zigbuild.tar.xz \
    "https://github.com/rust-cross/cargo-zigbuild/releases/download/v${CARGO_ZIGBUILD_VERSION}/cargo-zigbuild-${cz_target}.tar.xz"
  printf '%s  /tmp/cargo-zigbuild.tar.xz\n' "${cz_sha}" > /tmp/cargo-zigbuild.tar.xz.sha256
  sha256sum -c /tmp/cargo-zigbuild.tar.xz.sha256
  rm /tmp/cargo-zigbuild.tar.xz.sha256
  tar -xJf /tmp/cargo-zigbuild.tar.xz --strip-components=1 -C /usr/local/bin --wildcards '*/cargo-zigbuild'
  chmod +x /usr/local/bin/cargo-zigbuild
  rm /tmp/cargo-zigbuild.tar.xz
EOR

# Pinned STDIO integration fixture.
RUN npm install -g @modelcontextprotocol/server-everything@2026.1.26 \
  && command -v mcp-server-everything

# nfpm packages release binaries into .deb/.rpm without dpkg/rpmbuild or root.
ARG NFPM_VERSION
ARG NFPM_SHA256_AMD64
ARG NFPM_SHA256_ARM64
RUN <<EOR
  set -e
  case "$(uname -m)" in
    x86_64)  nfpm_arch=x86_64; nfpm_sha=${NFPM_SHA256_AMD64};;
    aarch64) nfpm_arch=arm64;  nfpm_sha=${NFPM_SHA256_ARM64};;
    *) echo "unsupported build arch: $(uname -m)" >&2; exit 1;;
  esac
  curl -fsSL -o /tmp/nfpm.tar.gz "https://github.com/goreleaser/nfpm/releases/download/v${NFPM_VERSION}/nfpm_${NFPM_VERSION}_Linux_${nfpm_arch}.tar.gz"
  printf '%s  /tmp/nfpm.tar.gz\n' "${nfpm_sha}" > /tmp/nfpm.tar.gz.sha256
  sha256sum -c /tmp/nfpm.tar.gz.sha256
  tar -xzf /tmp/nfpm.tar.gz -C /usr/local/bin nfpm
  rm /tmp/nfpm.tar.gz /tmp/nfpm.tar.gz.sha256
  nfpm --version
EOR

# A PyInstaller bundle: the executable resolves its payload beside its real
# path, so the directory is unpacked whole and only the entrypoint linked.
ARG CLOUDSMITH_VERSION
ARG CLOUDSMITH_SHA256_AMD64
ARG CLOUDSMITH_SHA256_ARM64
RUN <<EOR
  set -e
  case "$(uname -m)" in
    x86_64)  cs_arch=x86_64;  cs_sha=${CLOUDSMITH_SHA256_AMD64};;
    aarch64) cs_arch=aarch64; cs_sha=${CLOUDSMITH_SHA256_ARM64};;
    *) echo "unsupported build arch: $(uname -m)" >&2; exit 1;;
  esac
  curl -fsSL -o /tmp/cloudsmith.tar.gz "https://github.com/cloudsmith-io/cloudsmith-cli/releases/download/v${CLOUDSMITH_VERSION}/cloudsmith-${CLOUDSMITH_VERSION}-linux-${cs_arch}-gnu.tar.gz"
  printf '%s  /tmp/cloudsmith.tar.gz\n' "${cs_sha}" > /tmp/cloudsmith.tar.gz.sha256
  sha256sum -c /tmp/cloudsmith.tar.gz.sha256
  mkdir -p /opt/cloudsmith
  tar -xzf /tmp/cloudsmith.tar.gz --strip-components=1 -C /opt/cloudsmith
  ln -s /opt/cloudsmith/cloudsmith /usr/local/bin/cloudsmith
  rm /tmp/cloudsmith.tar.gz /tmp/cloudsmith.tar.gz.sha256
  cloudsmith --version
EOR

COPY --from=sccache-dl /usr/local/bin/sccache /usr/local/bin/sccache

USER 1000

RUN <<EOR
  set -e
  rustup component add rustfmt clippy llvm-tools
  rustup component add --toolchain nightly rustfmt
  rustup target add x86_64-unknown-linux-gnu aarch64-unknown-linux-gnu
EOR

### 006 Test-tools
# Install tools before source COPY so source changes keep this layer cached.
FROM cook-debug AS test-tools

USER 0
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

USER 0
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

USER 1000

# grcov reads llvm-tools' profdata. clippy rides this image's cook-debug
# deps for CI lint; rustfmt stays runner-only (fmt-check doesn't compile,
# so it gains nothing from these cached deps).
RUN rustup component add llvm-tools clippy

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

RUN cargo build --workspace --bin aura

### 010 Cook-release
FROM core AS cook-release
WORKDIR /usr/src/app
COPY --from=planner /usr/src/app/recipe.json recipe.json
ENV CARGO_TARGET_DIR=/usr/src/app/target
RUN cargo chef cook --release -p aura-cli --bin aura --recipe-path recipe.json \
 && cargo chef cook --release --bin aura-web-server --recipe-path recipe.json

### 011 Release-build
# Source changes only recompile workspace crates.
FROM cook-release AS release-build
WORKDIR /usr/src/app
COPY Cargo.toml Cargo.lock ./
COPY crates/ ./crates/

RUN <<EOR
  set -e
  cargo build --release -p aura-cli --bin aura
  cargo build --release --bin aura-web-server
EOR

### 012 Runtime
# Shared runtime shell; server and release differ only in the binaries they carry.
FROM debian:trixie-slim AS runtime

RUN <<EOR
  set -e
  apt-get update && apt-get install -y --no-install-recommends ca-certificates libssl3 curl
  rm -rf /var/lib/apt/lists/*
  useradd -r -u 1000 -s /bin/false appuser
EOR

WORKDIR /app
RUN mkdir -p /app/config /app/skills && chown -R appuser:appuser /app

USER 1000
EXPOSE 3030

ENV HOST=0.0.0.0
ENV PORT=3030
ENV CONFIG_PATH=/app/config/config.toml

HEALTHCHECK --interval=30s --timeout=10s --start-period=5s --retries=3 \
  CMD ["/bin/sh", "-c", "curl -f http://localhost:3030/health || exit 1"]

CMD ["./aura", "webserver"]

### 013 Server
# Integration-lane server image (debug profile).
FROM runtime AS server

COPY --from=debug-build /usr/src/app/target/debug/aura /app/

### 014 Release
# Published image. Must stay the final stage: the feature-build and
# post-merge publish lanes build the default target. aura-web-server is the
# deprecated shim, carried until it is retired.
FROM runtime AS release

COPY --from=release-build /usr/src/app/target/release/aura /app/
COPY --from=release-build /usr/src/app/target/release/aura-web-server /app/
