#!/usr/bin/env bash
# Build Debian (.deb) and RPM (.rpm) packages from the release binaries in dist/.
#
# Consumes the Linux binaries produced by `make build-binaries-linux`
# (dist/<binary>-linux-<arch>) and emits one package per binary, per arch, per
# format into DIST_DIR. Uses nfpm, so no dpkg/rpmbuild toolchain or root
# privilege is required.
#
# Usage: build-packages.sh [version]
#   version    - package version (default: workspace version from Cargo.toml)
#
# Environment:
#   DIST_DIR   - directory holding the binaries and receiving packages (default: dist)
#   PACKAGERS  - space-separated formats to build (default: "deb rpm")
#   ARCHS      - space-separated architectures to build (default: "amd64 arm64")
#   NFPM       - nfpm executable to use (default: nfpm on PATH)
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"

DIST_DIR="${DIST_DIR:-${PROJECT_ROOT}/dist}"
PACKAGERS="${PACKAGERS:-deb rpm}"
ARCHS="${ARCHS:-amd64 arm64}"
NFPM="${NFPM:-nfpm}"

MAINTAINER="Mezmo <support@mezmo.com>"
LICENSE="Apache-2.0"
HOMEPAGE="https://github.com/mezmo/aura"

# Each entry: "<package name>|<binary basename>|<description>". The binary
# basename combines with the platform suffix to locate the source file
# (dist/<basename>-linux-<arch>) and names the installed executable
# (/usr/bin/<basename>).
PACKAGES=(
    "aura|aura|AURA interactive terminal client for chat completions with tool execution"
    "aura-web-server|aura-web-server|AURA OpenAI-compatible web server for composing Rig.rs AI agents with MCP tools and RAG pipelines"
)

resolve_version() {
    local version="${1:-}"
    if [ -z "${version}" ]; then
        version="$(awk '
            /^\[workspace\.package\]/ { in_section = 1; next }
            /^\[/ { in_section = 0 }
            in_section && /^version[[:space:]]*=/ { print $2; exit }
        ' FS='"' "${PROJECT_ROOT}/Cargo.toml")"
    fi
    version="${version#v}"
    if [ -z "${version}" ]; then
        echo "error: could not determine version" >&2
        exit 1
    fi
    printf '%s' "${version}"
}

require_nfpm() {
    if ! command -v "${NFPM}" >/dev/null 2>&1; then
        echo "error: nfpm not found (set NFPM or install from https://nfpm.goreleaser.com)" >&2
        exit 1
    fi
}

# Write an nfpm config for one package/arch pair to stdout.
write_config() {
    local name="$1" binary="$2" description="$3" arch="$4" version="$5" src="$6"
    cat <<EOF
name: "${name}"
arch: "${arch}"
version: "${version}"
version_schema: none
maintainer: "${MAINTAINER}"
description: "${description}"
license: "${LICENSE}"
homepage: "${HOMEPAGE}"
contents:
  - src: "${src}"
    dst: "/usr/bin/${binary}"
    file_info:
      mode: 0755
EOF
}

main() {
    local version
    version="$(resolve_version "${1:-}")"
    require_nfpm

    echo "Building packages for AURA ${version}"

    # Global (not local) so the EXIT trap can still see it once main returns.
    config="$(mktemp)"
    trap 'rm -f "${config}"' EXIT

    local entry name binary description arch src packager
    for entry in "${PACKAGES[@]}"; do
        IFS='|' read -r name binary description <<<"${entry}"
        for arch in ${ARCHS}; do
            src="${DIST_DIR}/${binary}-linux-${arch}"
            if [ ! -f "${src}" ]; then
                echo "error: missing binary: ${src}" >&2
                exit 1
            fi
            write_config "${name}" "${binary}" "${description}" "${arch}" "${version}" "${src}" >"${config}"
            for packager in ${PACKAGERS}; do
                echo "  ${name} ${arch} -> ${packager}"
                "${NFPM}" package --config "${config}" --packager "${packager}" --target "${DIST_DIR}/"
            done
        done
    done

    echo ""
    echo "Packages written to ${DIST_DIR}"
}

main "$@"
