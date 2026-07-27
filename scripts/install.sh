#!/usr/bin/env bash
# Install AURA binaries from GitHub Releases.
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/mezmo/aura/main/scripts/install.sh | bash
#
# Options (via environment variables):
#   AURA_VERSION          - Version to install (default: latest)
#   AURA_INSTALL_PATH     - Install directory (default: ~/.local/bin)
#   AURA_COMPONENT        - Which binary: "all", "server", "cli" (default: all)
#   AURA_REQUIRE_CHECKSUM - Fail (1) instead of warn (0) when a checksum is missing (default: 1)
#   AURA_INSTALL_METHOD   - Install method: "auto", "homebrew", "direct", "deb", "rpm" (default: auto)
#   AURA_CHECKSUMS        - Path to a local checksums.txt to use instead of fetching it
#
# "auto" prefers a native package (deb, then rpm) when the matching package
# manager is present and it can install as root without an interactive prompt
# (already root, or passwordless sudo); otherwise it falls back to Homebrew and
# finally a direct binary download. "deb" and "rpm" force a system package and
# install to /usr/bin, escalating with sudo if needed.

set -euo pipefail

REPO="mezmo/aura"
BREW_TAP="mezmo/tap"
VERSION="${AURA_VERSION:-latest}"
VERSION="${VERSION#v}"
INSTALL_PATH="${AURA_INSTALL_PATH:-${HOME}/.local/bin}"
COMPONENT="${AURA_COMPONENT:-all}"
REQUIRE_CHECKSUM="${AURA_REQUIRE_CHECKSUM:-1}"
INSTALL_METHOD="${AURA_INSTALL_METHOD:-auto}"
BASE_URL="https://github.com/${REPO}/releases"

# RPM release field baked into the package asset names; mirrors nfpm's default
# in scripts/build-packages.sh (which does not set `release`).
RPM_RELEASE=1

case "${COMPONENT}" in
    all|server|cli) ;;
    *)
        echo "Error: invalid AURA_COMPONENT '${COMPONENT}'. Supported: all, server, cli." >&2
        exit 1
        ;;
esac

case "${INSTALL_METHOD}" in
    auto|homebrew|direct|deb|rpm) ;;
    *)
        echo "Error: invalid AURA_INSTALL_METHOD '${INSTALL_METHOD}'. Supported: auto, homebrew, direct, deb, rpm." >&2
        exit 1
        ;;
esac

case "${REQUIRE_CHECKSUM}" in
    0|1) ;;
    *)
        echo "Error: AURA_REQUIRE_CHECKSUM must be 0 or 1." >&2
        exit 1
        ;;
esac

main() {
    detect_platform
    resolve_install_method

    case "${RESOLVED_METHOD}" in
        deb|rpm)  install_via_package "${RESOLVED_METHOD}" ;;
        homebrew) install_via_homebrew ;;
        direct)   install_via_direct ;;
    esac
}

targets() {
    case "${COMPONENT}" in
        cli)    echo "aura" ;;
        server) echo "aura-web-server" ;;
        all)    echo "aura aura-web-server" ;;
    esac
}

# Reason an active option is incompatible with a method, else empty.
method_conflict() {
    case "$1" in
        homebrew)
            [[ "${VERSION}" != latest ]] && { echo "Homebrew can't pin AURA_VERSION"; return; }
            [[ -n "${AURA_INSTALL_PATH:-}" ]] && { echo "Homebrew can't honor AURA_INSTALL_PATH"; return; }
            ;;
        deb|rpm)
            [[ -n "${AURA_INSTALL_PATH:-}" ]] && { echo "$1 packages install to /usr/bin, not AURA_INSTALL_PATH"; return; }
            ;;
    esac
    return 0
}

# Whether the host can attempt a method at all (privilege is checked separately).
hard_available() {
    case "$1" in
        deb|rpm)  [[ "${OS}" == linux ]] && package_manager_present "$1" ;;
        homebrew) command -v brew >/dev/null 2>&1 ;;
        direct)   true ;;
    esac
}

package_manager_present() {
    case "$1" in
        deb) command -v dpkg >/dev/null 2>&1 ;;
        rpm) command -v rpm >/dev/null 2>&1 || command -v dnf >/dev/null 2>&1 || command -v yum >/dev/null 2>&1 ;;
    esac
}

# Whether we can run an install command as root non-interactively.
can_escalate() {
    [[ "$(id -u)" -eq 0 ]] && return 0
    command -v sudo >/dev/null 2>&1 && sudo -n true >/dev/null 2>&1
}

resolve_install_method() {
    if [[ "${INSTALL_METHOD}" != auto ]]; then
        local conflict
        conflict="$(method_conflict "${INSTALL_METHOD}")"
        [[ -z "${conflict}" ]] || { echo "Error: ${conflict}." >&2; exit 1; }
        hard_available "${INSTALL_METHOD}" || { echo "Error: ${INSTALL_METHOD} install is not available on this host." >&2; exit 1; }
        RESOLVED_METHOD="${INSTALL_METHOD}"
        return
    fi

    local method conflict noted=""
    for method in deb rpm homebrew direct; do
        hard_available "${method}" || continue
        # A native package in auto needs root without a prompt.
        [[ "${method}" == deb || "${method}" == rpm ]] && ! can_escalate && continue
        conflict="$(method_conflict "${method}")"
        if [[ -n "${conflict}" ]]; then
            [[ -n "${noted}" ]] || { echo "Note: ${conflict}; using a direct install instead." >&2; noted=1; }
            continue
        fi
        RESOLVED_METHOD="${method}"
        return
    done
    RESOLVED_METHOD=direct
}

install_via_homebrew() {
    local target
    for target in $(targets); do
        echo "Installing ${target} via Homebrew (${BREW_TAP}/${target})"
        # brew install won't upgrade an already-installed formula.
        if brew ls --versions "${BREW_TAP}/${target}" >/dev/null 2>&1; then
            brew upgrade "${BREW_TAP}/${target}" || return 1
        else
            brew install "${BREW_TAP}/${target}" || return 1
        fi
    done
}

install_via_direct() {
    detect_downloader
    resolve_version

    echo "Installing AURA ${VERSION} (${OS}/${ARCH}) to ${INSTALL_PATH}"
    mkdir -p "${INSTALL_PATH}"

    # Stage inside INSTALL_PATH so the final move is a same-filesystem rename
    # (atomic) rather than a cross-device copy from /tmp.
    # Intentionally global: the EXIT trap fires after this function returns, so a
    # function-local variable would be out of scope (and unbound under set -u).
    tmpdir=$(mktemp -d "${INSTALL_PATH}/.aura-install.XXXXXX")
    trap 'rm -rf "${tmpdir}"' EXIT

    fetch_checksums "${tmpdir}"

    # Prepare every binary before committing any, so a mid-way failure leaves
    # nothing in INSTALL_PATH.
    local target
    for target in $(targets); do
        prepare_binary "${tmpdir}" "${target}" || return 1
    done
    for target in $(targets); do
        commit_binary "${tmpdir}" "${target}" || return 1
    done

    echo ""
    echo "Installed to ${INSTALL_PATH}"
    if [[ ":${PATH}:" != *":${INSTALL_PATH}:"* ]]; then
        echo ""
        echo "Add to your PATH:"
        echo "  export PATH=\"${INSTALL_PATH}:\${PATH}\""
    fi
}

install_via_package() {
    local format="$1"

    local sudo=""
    if [[ "$(id -u)" -ne 0 ]]; then
        if command -v sudo >/dev/null 2>&1; then
            sudo="sudo"
        else
            echo "Error: installing ${format} packages requires root; re-run as root or install sudo." >&2
            exit 1
        fi
    fi

    detect_downloader
    resolve_version

    echo "Installing AURA ${VERSION} (${OS}/${ARCH}) via ${format} package(s)"

    tmpdir=$(mktemp -d)
    trap 'rm -rf "${tmpdir}"' EXIT

    fetch_checksums "${tmpdir}"

    # Download and verify every package before installing any, so a mid-way
    # failure leaves nothing installed.
    local target asset paths=()
    for target in $(targets); do
        asset="$(package_asset "${format}" "${target}")"
        fetch_asset "${tmpdir}" "${asset}" || return 1
        paths+=("${tmpdir}/${asset}")
    done

    install_packages "${format}" "${sudo}" "${paths[@]}"

    echo ""
    echo "Installed AURA ${VERSION} to /usr/bin"
}

# Release asset filename for one package, mirroring nfpm's output names in
# scripts/build-packages.sh (deb uses the Debian arch, rpm the RPM arch).
package_asset() {
    local format="$1" name="$2"
    case "${format}" in
        deb) printf '%s_%s_%s.deb' "${name}" "${VERSION}" "${ARCH}" ;;
        rpm) printf '%s-%s-%s.%s.rpm' "${name}" "${VERSION}" "${RPM_RELEASE}" "$(rpm_arch)" ;;
    esac
}

rpm_arch() {
    case "${ARCH}" in
        amd64) echo "x86_64" ;;
        arm64) echo "aarch64" ;;
    esac
}

install_packages() {
    local format="$1" sudo="$2"
    shift 2
    case "${format}" in
        deb)
            if ! command -v dpkg >/dev/null 2>&1; then
                echo "Error: dpkg not found; cannot install .deb packages." >&2
                exit 1
            fi
            ${sudo} dpkg -i "$@"
            ;;
        rpm)
            if command -v dnf >/dev/null 2>&1; then
                ${sudo} dnf install -y "$@"
            elif command -v yum >/dev/null 2>&1; then
                ${sudo} yum install -y "$@"
            elif command -v rpm >/dev/null 2>&1; then
                ${sudo} rpm -Uvh "$@"
            else
                echo "Error: need dnf, yum, or rpm to install .rpm packages." >&2
                exit 1
            fi
            ;;
    esac
}

fetch_checksums() {
    local tmpdir="$1"
    if [[ -n "${AURA_CHECKSUMS:-}" ]]; then
        cp "${AURA_CHECKSUMS}" "${tmpdir}/checksums.txt"
    else
        fetch "${tmpdir}/checksums.txt" "${BASE_URL}/download/v${VERSION}/checksums.txt" 2>/dev/null || true
    fi
    if [[ "${REQUIRE_CHECKSUM}" == 1 && ! -s "${tmpdir}/checksums.txt" ]]; then
        echo "Error: AURA_REQUIRE_CHECKSUM is set but checksums.txt could not be fetched." >&2
        exit 1
    fi
}

detect_platform() {
    OS=$(uname -s | tr '[:upper:]' '[:lower:]')
    case "${OS}" in
        linux) ;;
        darwin) ;;
        *)
            echo "Error: unsupported OS '${OS}'. Supported: linux, darwin." >&2
            exit 1
            ;;
    esac

    ARCH=$(uname -m)
    case "${ARCH}" in
        x86_64)  ARCH="amd64" ;;
        aarch64) ARCH="arm64" ;;
        arm64)   ARCH="arm64" ;;
        *)
            echo "Error: unsupported architecture '${ARCH}'. Supported: x86_64, aarch64." >&2
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
        echo "Error: need curl or wget installed." >&2
        exit 1
    fi
}

fetch() {
    local dest="$1" url="$2"
    case "${DOWNLOADER}" in
        curl) curl -fsSL --connect-timeout 10 --retry 3 -o "${dest}" "${url}" ;;
        wget) wget -q --timeout=10 --tries=3 -O "${dest}" "${url}" ;;
    esac
}

resolve_latest_url() {
    case "${DOWNLOADER}" in
        curl)
            curl -fsSLI --connect-timeout 10 --retry 3 -o /dev/null -w '%{url_effective}' "${BASE_URL}/latest"
            ;;
        wget)
            wget -S --spider --timeout=10 --tries=3 "${BASE_URL}/latest" 2>&1 \
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
            echo "Error: could not determine latest version." >&2
            exit 1
        fi
    fi
}

download() {
    local dest="$1" name="$2"
    local url="${BASE_URL}/download/v${VERSION}/${name}"
    if ! fetch "${dest}" "${url}"; then
        echo "Error: failed to download ${url}" >&2
        return 1
    fi
}

sha256_file() {
    local file="$1"

    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "${file}" | awk '{print $1}'
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "${file}" | awk '{print $1}'
    elif command -v openssl >/dev/null 2>&1; then
        openssl dgst -sha256 "${file}" | awk '{print $NF}'
    else
        echo "Error: no SHA-256 utility found." >&2
        return 1
    fi
}

verify_checksum() {
    local file="$1" asset_name="$2" checksums="$3"
    if [[ ! -s "${checksums}" ]]; then
        if [[ "${REQUIRE_CHECKSUM}" == 1 ]]; then
            echo "Error: no checksums file and AURA_REQUIRE_CHECKSUM is set." >&2
            exit 1
        fi
        echo "  Warning: no checksums file, skipping verification" >&2
        return 0
    fi
    # Match the asset in either checksum format: "hash  name" (text) or
    # "hash *name" (binary). Exact field match avoids superstring collisions.
    local expected
    expected=$(awk -v name="${asset_name}" '
        $2 == name || $2 == "*" name { print $1; exit }
    ' "${checksums}")
    if [[ ! "${expected}" =~ ^[0-9a-fA-F]{64}$ ]]; then
        if [[ "${REQUIRE_CHECKSUM}" == 1 ]]; then
            echo "Error: no valid checksum for ${asset_name} and AURA_REQUIRE_CHECKSUM is set." >&2
            exit 1
        fi
        echo "  Warning: no valid checksum for ${asset_name}, skipping verification" >&2
        return 0
    fi
    local actual
    actual=$(sha256_file "${file}") || exit 1
    if [[ "${actual}" != "${expected}" ]]; then
        echo "Error: checksum mismatch for ${asset_name}" >&2
        echo "  expected: ${expected}" >&2
        echo "  actual:   ${actual}" >&2
        exit 1
    fi
    echo "  Verified checksum: OK"
}

binary_asset() {
    echo "${1}-${OS}-${ARCH}"
}

fetch_asset() {
    local tmpdir="$1" asset="$2"
    echo "  Downloading ${asset}..."
    download "${tmpdir}/${asset}" "${asset}" || return 1
    verify_checksum "${tmpdir}/${asset}" "${asset}" "${tmpdir}/checksums.txt"
}

prepare_binary() {
    local tmpdir="$1" binary="$2" asset
    asset="$(binary_asset "${binary}")"

    fetch_asset "${tmpdir}" "${asset}" || return 1
    chmod 0755 "${tmpdir}/${asset}" || return 1
}

commit_binary() {
    local tmpdir="$1" binary="$2" asset
    asset="$(binary_asset "${binary}")"

    mv "${tmpdir}/${asset}" "${INSTALL_PATH}/${binary}" || return 1
    echo "  Installed: ${INSTALL_PATH}/${binary}"
}

main
