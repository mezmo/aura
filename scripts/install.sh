#!/usr/bin/env bash
# Install AURA binaries from GitHub Releases.
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/mezmo/aura/main/scripts/install.sh | bash
#
# Options (via environment variables):
#   AURA_VERSION          - Version to install (default: latest)
#   AURA_INSTALL          - Install directory (default: ~/.local/bin)
#   AURA_COMPONENT        - Which binary: "all", "server", "cli" (default: all)
#   AURA_REQUIRE_CHECKSUM - Fail (1) instead of warn (0) when a checksum is missing (default: 0)
#   AURA_NO_BREW          - Skip Homebrew and download directly on macOS (default: 0)

set -euo pipefail

REPO="mezmo/aura"
BREW_TAP="mezmo/tap/aura"
VERSION="${AURA_VERSION:-latest}"
VERSION="${VERSION#v}"
INSTALL_DIR="${AURA_INSTALL:-${HOME}/.local/bin}"
COMPONENT="${AURA_COMPONENT:-all}"
REQUIRE_CHECKSUM="${AURA_REQUIRE_CHECKSUM:-0}"
NO_BREW="${AURA_NO_BREW:-0}"
BASE_URL="https://github.com/${REPO}/releases"

case "${COMPONENT}" in
    all|server|cli) ;;
    *)
        echo "Error: invalid AURA_COMPONENT '${COMPONENT}'. Supported: all, server, cli."
        exit 1
        ;;
esac

main() {
    detect_platform

    if [[ "${OS}" == "darwin" ]] && should_use_homebrew; then
        install_via_homebrew
        return
    fi

    detect_downloader
    resolve_version

    echo "Installing AURA ${VERSION} (${OS}/${ARCH}) to ${INSTALL_DIR}"
    mkdir -p "${INSTALL_DIR}"

    tmpdir=$(mktemp -d)
    trap 'rm -rf "${tmpdir}"' EXIT

    if [[ -n "${AURA_CHECKSUMS:-}" ]]; then
        cp "${AURA_CHECKSUMS}" "${tmpdir}/checksums.txt"
    else
        local checksums_url="${BASE_URL}/download/v${VERSION}/checksums.txt"
        fetch "${tmpdir}/checksums.txt" "${checksums_url}" 2>/dev/null || true
    fi

    if [[ "${REQUIRE_CHECKSUM}" == 1 && ! -s "${tmpdir}/checksums.txt" ]]; then
        echo "Error: AURA_REQUIRE_CHECKSUM is set but checksums.txt could not be fetched."
        exit 1
    fi

    if [[ "${COMPONENT}" != "cli" ]]; then
        install_binary "${tmpdir}" "aura-web-server"
    fi
    if [[ "${COMPONENT}" != "server" ]]; then
        install_binary "${tmpdir}" "aura"
    fi

    echo ""
    echo "Installed to ${INSTALL_DIR}"
    if [[ ":${PATH}:" != *":${INSTALL_DIR}:"* ]]; then
        echo ""
        echo "Add to your PATH:"
        echo "  export PATH=\"${INSTALL_DIR}:\${PATH}\""
    fi
}

# On macOS, prefer the Homebrew tap when `brew` is available.
should_use_homebrew() {
    [[ "${NO_BREW}" != 1 ]] || return 1
    command -v brew >/dev/null 2>&1 || return 1

    if [[ "${VERSION}" != "latest" ]]; then
        echo "Note: AURA_VERSION is set; Homebrew can't pin versions, downloading directly."
        return 1
    fi
    if [[ "${COMPONENT}" != "all" ]]; then
        echo "Note: AURA_COMPONENT is '${COMPONENT}'; Homebrew installs both binaries, downloading directly."
        return 1
    fi
    return 0
}

install_via_homebrew() {
    echo "Installing AURA via Homebrew (${BREW_TAP})"
    brew install "${BREW_TAP}"
}

detect_platform() {
    OS=$(uname -s | tr '[:upper:]' '[:lower:]')
    case "${OS}" in
        linux) ;;
        darwin) ;;
        *)
            echo "Error: unsupported OS '${OS}'. Supported: linux, darwin."
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

detect_downloader() {
    if command -v curl >/dev/null 2>&1; then
        DOWNLOADER="curl"
    elif command -v wget >/dev/null 2>&1; then
        DOWNLOADER="wget"
    else
        echo "Error: need curl or wget installed."
        exit 1
    fi
}

fetch() {
    local dest="$1" url="$2"
    case "${DOWNLOADER}" in
        curl) curl -fsSL -o "${dest}" "${url}" ;;
        wget) wget -q -O "${dest}" "${url}" ;;
    esac
}

resolve_latest_url() {
    case "${DOWNLOADER}" in
        curl)
            curl -fsSLI -o /dev/null -w '%{url_effective}' "${BASE_URL}/latest"
            ;;
        wget)
            wget -S --spider "${BASE_URL}/latest" 2>&1 \
                | awk 'tolower($1) == "location:" { print $2 }' | tail -1
            ;;
    esac
}

resolve_version() {
    if [[ "${VERSION}" == "latest" ]]; then
        local url
        url=$(resolve_latest_url 2>/dev/null | tr -d '\r')
        VERSION="${url##*/}"
        VERSION="${VERSION#v}"
        if [[ -z "${VERSION}" ]]; then
            echo "Error: could not determine latest version."
            exit 1
        fi
    fi
}

download() {
    local dest="$1" name="$2"
    local url="${BASE_URL}/download/v${VERSION}/${name}"
    if ! fetch "${dest}" "${url}"; then
        echo "Error: failed to download ${url}"
        return 1
    fi
}

verify_checksum() {
    local file="$1" asset_name="$2" checksums="$3"
    if [[ ! -s "${checksums}" ]]; then
        if [[ "${REQUIRE_CHECKSUM}" == 1 ]]; then
            echo "Error: no checksums file and AURA_REQUIRE_CHECKSUM is set."
            exit 1
        fi
        echo "  Warning: no checksums file, skipping verification"
        return 0
    fi
    local expected
    expected=$(grep "  ${asset_name}\$" "${checksums}" | cut -d' ' -f1 || true)
    if [[ -z "${expected}" ]]; then
        if [[ "${REQUIRE_CHECKSUM}" == 1 ]]; then
            echo "Error: no checksum for ${asset_name} and AURA_REQUIRE_CHECKSUM is set."
            exit 1
        fi
        echo "  Warning: no checksum for ${asset_name}, skipping verification"
        return 0
    fi
    local actual
    actual=$(sha256sum "${file}" | cut -d' ' -f1)
    if [[ "${actual}" != "${expected}" ]]; then
        echo "Error: checksum mismatch for ${asset_name}"
        echo "  expected: ${expected}"
        echo "  actual:   ${actual}"
        exit 1
    fi
    echo "  Verified checksum: OK"
}

install_binary() {
    local tmpdir="$1" binary="$2"
    local asset_name="${binary}-${OS}-${ARCH}"

    echo "  Downloading ${asset_name}..."
    download "${tmpdir}/${asset_name}" "${asset_name}"
    verify_checksum "${tmpdir}/${asset_name}" "${asset_name}" "${tmpdir}/checksums.txt"

    chmod +x "${tmpdir}/${asset_name}"
    mv "${tmpdir}/${asset_name}" "${INSTALL_DIR}/${binary}"
    echo "  Installed: ${INSTALL_DIR}/${binary}"
}

main
