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
BASE_URL="https://github.com/${REPO}/releases"

main() {
    detect_platform
    resolve_version

    echo "Installing AURA ${VERSION} (${OS}/${ARCH}) to ${INSTALL_DIR}"
    mkdir -p "${INSTALL_DIR}"

    tmpdir=$(mktemp -d)
    trap 'rm -rf "${tmpdir}"' EXIT

    if [[ -n "${AURA_CHECKSUMS:-}" ]]; then
        cp "${AURA_CHECKSUMS}" "${tmpdir}/checksums.txt"
    else
        local checksums_url="${BASE_URL}/download/v${VERSION}/checksums.txt"
        curl -fsSL -o "${tmpdir}/checksums.txt" "${checksums_url}" 2>/dev/null || true
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
        local location
        location=$(curl -fsSI "${BASE_URL}/latest" 2>/dev/null \
            | grep -i '^location:' | tail -1 | tr -d '\r')
        VERSION="${location##*/}"
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
    if ! curl -fsSL -o "${dest}" "${url}"; then
        echo "Error: failed to download ${url}"
        return 1
    fi
}

verify_checksum() {
    local file="$1" asset_name="$2" checksums="$3"
    if [[ ! -f "${checksums}" ]]; then
        echo "  Warning: no checksums file, skipping verification"
        return 0
    fi
    local expected
    expected=$(grep "  ${asset_name}\$" "${checksums}" | cut -d' ' -f1 || true)
    if [[ -z "${expected}" ]]; then
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
