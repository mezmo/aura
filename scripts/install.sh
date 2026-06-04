#!/usr/bin/env bash
# Install AURA binaries from GitHub Releases.
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/mezmo/aura/main/scripts/install.sh | bash
#
# Options (via environment variables):
#   AURA_VERSION   - Version to install (default: latest)
#   AURA_INSTALL   - Install directory (default: ~/.local/bin)
#   AURA_COMPONENT - Which binary: "all", "server", "cli" (default: all)

set -euo pipefail

REPO="mezmo/aura"
VERSION="${AURA_VERSION:-latest}"
INSTALL_DIR="${AURA_INSTALL:-${HOME}/.local/bin}"
COMPONENT="${AURA_COMPONENT:-all}"
GITHUB_API="https://api.github.com"

main() {
    detect_platform
    resolve_version
    fetch_release_metadata

    echo "Installing AURA ${VERSION} (${OS}/${ARCH}) to ${INSTALL_DIR}"
    mkdir -p "${INSTALL_DIR}"

    local tmpdir
    tmpdir=$(mktemp -d)
    trap 'rm -rf "${tmpdir}"' EXIT

    if [[ "${COMPONENT}" != "cli" ]]; then
        install_binary "${tmpdir}" "aura-web-server"
    fi
    if [[ "${COMPONENT}" != "server" ]]; then
        install_binary "${tmpdir}" "aura-cli"
    fi

    echo ""
    echo "Installed to ${INSTALL_DIR}"
    if [[ ":${PATH}:" != *":${INSTALL_DIR}:"* ]]; then
        echo ""
        echo "Add to your PATH:"
        echo "  export PATH=\"${INSTALL_DIR}:\${PATH}\""
    fi
}

detect_platform() {
    OS=$(uname -s | tr '[:upper:]' '[:lower:]')
    case "${OS}" in
        linux) ;;
        *)
            echo "Error: unsupported OS '${OS}'. Only linux is currently available."
            exit 1
            ;;
    esac

    ARCH=$(uname -m)
    case "${ARCH}" in
        x86_64)  ARCH="amd64" ;;
        aarch64) ARCH="arm64" ;;
        arm64)   ARCH="arm64" ;;
        *)
            echo "Error: unsupported architecture '${ARCH}'. Supported: x86_64, aarch64."
            exit 1
            ;;
    esac
}

resolve_version() {
    if [[ "${VERSION}" == "latest" ]]; then
        VERSION=$(curl -fsSL -H "Accept: application/vnd.github.v3+json" \
            "${GITHUB_API}/repos/${REPO}/releases/latest" \
            | grep -o '"tag_name" *: *"[^"]*"' | head -1 | sed 's/.*"v\([^"]*\)".*/\1/')
        if [[ -z "${VERSION}" ]]; then
            echo "Error: could not determine latest version."
            exit 1
        fi
    fi
}

RELEASE_JSON=""

fetch_release_metadata() {
    RELEASE_JSON=$(curl -fsSL -H "Accept: application/vnd.github.v3+json" \
        "${GITHUB_API}/repos/${REPO}/releases/tags/v${VERSION}")
}

get_asset_digest() {
    local asset_name="$1"
    echo "${RELEASE_JSON}" \
        | grep -o "\"name\" *: *\"${asset_name}\"[^}]*\"digest\" *: *\"sha256:[a-f0-9]*\"" \
        | grep -o 'sha256:[a-f0-9]*' \
        | cut -d: -f2
}

download() {
    local dest="$1" name="$2"
    local url="https://github.com/${REPO}/releases/download/v${VERSION}/${name}"
    if ! curl -fsSL -o "${dest}" "${url}"; then
        echo "Error: failed to download ${url}"
        exit 1
    fi
}

install_binary() {
    local tmpdir="$1" binary="$2"
    local asset_name="${binary}-${OS}-${ARCH}"

    echo "  Downloading ${asset_name}..."
    download "${tmpdir}/${asset_name}" "${asset_name}"

    local expected
    expected=$(get_asset_digest "${asset_name}")
    if [[ -n "${expected}" ]]; then
        local actual
        actual=$(sha256sum "${tmpdir}/${asset_name}" | cut -d' ' -f1)
        if [[ "${actual}" != "${expected}" ]]; then
            echo "Error: checksum mismatch for ${asset_name}"
            echo "  expected: ${expected}"
            echo "  actual:   ${actual}"
            exit 1
        fi
        echo "  Verified checksum: OK"
    else
        echo "  Warning: no digest found for ${asset_name}, skipping verification"
    fi

    chmod +x "${tmpdir}/${asset_name}"
    mv "${tmpdir}/${asset_name}" "${INSTALL_DIR}/${binary}"
    echo "  Installed: ${INSTALL_DIR}/${binary}"
}

main
