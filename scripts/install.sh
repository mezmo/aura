#!/usr/bin/env bash
# Install AURA from the package repository or from GitHub Releases.
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
# finally a direct binary download. "deb" and "rpm" register the AURA package
# repository and install through apt/dnf/yum/zypper, escalating with sudo if
# needed, so later upgrades come from the package manager.
#
# AURA_REQUIRE_CHECKSUM and AURA_CHECKSUMS apply to the "direct" method only;
# package installs are verified by the repository's GPG signatures instead.

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

# Package repository, and the fingerprint-named URL of its signing key.
PACKAGE_REPO_URL="https://dl.cloudsmith.io/public/${REPO}"
PACKAGE_KEY_URL="${PACKAGE_REPO_URL}/gpg.05C8AD333177EB1F.key"
APT_SOURCE_FILE="/etc/apt/sources.list.d/mezmo-aura.list"
APT_KEYRING_DIR="/usr/share/keyrings"
YUM_REPO_FILE="/etc/yum.repos.d/mezmo-aura.repo"
ZYPP_REPO_FILE="/etc/zypp/repos.d/mezmo-aura.repo"

# Apt source suite used when the host's own is not indexed.
DEB_FALLBACK_DISTRO="debian"
DEB_FALLBACK_CODENAME="bookworm"

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

# A repository install needs a resolving package manager, not bare dpkg or rpm.
package_manager_present() {
    case "$1" in
        deb) command -v apt-get >/dev/null 2>&1 ;;
        rpm) [[ -n "$(rpm_manager)" ]] ;;
    esac
}

# The RPM-family package manager to drive, preferring the distribution's native tool.
rpm_manager() {
    local manager
    for manager in zypper dnf microdnf yum; do
        if command -v "${manager}" >/dev/null 2>&1; then
            printf '%s' "${manager}"
            return
        fi
    done
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

    local pin=""
    [[ "${VERSION}" != latest ]] && pin=" ${VERSION}"
    echo "Installing AURA${pin} (${OS}/${ARCH}) from the ${format} package repository"

    case "${format}" in
        deb) register_deb_repo "${sudo}" || return 1 ;;
        rpm) register_rpm_repo "${sudo}" || return 1 ;;
    esac

    install_from_repo "${format}" "${sudo}" || return 1

    echo ""
    echo "Installed AURA to /usr/bin"
    echo "Upgrades now come from your package manager."
}

# Installs the signing key and echoes its path for signed-by, which accepts
# either a dearmored keyring or an ASCII-armored key.
install_apt_key() {
    local sudo="$1" armored keyring
    armored="$(mktemp)"
    if ! fetch "${armored}" "${PACKAGE_KEY_URL}"; then
        rm -f "${armored}"
        echo "Error: failed to fetch the repository signing key from ${PACKAGE_KEY_URL}" >&2
        return 1
    fi

    ${sudo} mkdir -p "${APT_KEYRING_DIR}"
    if command -v gpg >/dev/null 2>&1; then
        keyring="${APT_KEYRING_DIR}/mezmo-aura-archive-keyring.gpg"
        gpg --dearmor <"${armored}" | ${sudo} tee "${keyring}" >/dev/null
    else
        keyring="${APT_KEYRING_DIR}/mezmo-aura-archive-keyring.asc"
        ${sudo} tee "${keyring}" <"${armored}" >/dev/null
    fi
    rm -f "${armored}"
    ${sudo} chmod 0644 "${keyring}"

    printf '%s' "${keyring}"
}

# HTTPS needs TLS trust and, on apt older than 1.5, a separate transport.
ensure_apt_transport() {
    local sudo="$1" missing=()
    [[ -e /etc/ssl/certs/ca-certificates.crt ]] || missing+=("ca-certificates")
    [[ -e /usr/lib/apt/methods/https ]] || missing+=("apt-transport-https")
    [[ ${#missing[@]} -eq 0 ]] && return 0

    echo "  Installing apt prerequisites: ${missing[*]}"
    # Unscoped: these come from the distribution's own repositories.
    ${sudo} apt-get update -qq || true
    if ! ${sudo} apt-get install -y "${missing[@]}"; then
        echo "Error: could not install ${missing[*]}; install them and re-run." >&2
        return 1
    fi
}

register_deb_repo() {
    local sudo="$1" distro codename keyring
    ensure_apt_transport "${sudo}" || return 1

    read -r distro codename <<<"$(deb_suite)"

    keyring="$(install_apt_key "${sudo}")" || return 1

    echo "  Configuring ${APT_SOURCE_FILE} for ${distro} ${codename}"
    printf 'deb [signed-by=%s] %s/deb/%s %s main\n' \
        "${keyring}" "${PACKAGE_REPO_URL}" "${distro}" "${codename}" \
        | ${sudo} tee "${APT_SOURCE_FILE}" >/dev/null
    ${sudo} chmod 0644 "${APT_SOURCE_FILE}"

    # Refresh only this source, so an unrelated broken entry cannot fail the install.
    ${sudo} apt-get update \
        -o Dir::Etc::sourcelist="${APT_SOURCE_FILE}" \
        -o Dir::Etc::sourceparts="-" \
        -o APT::Get::List-Cleanup="0"
}

# The distro and codename for the apt source, derived from /etc/os-release.
deb_suite() {
    local id="" codename="" ubuntu_codename="" id_like=""
    if [[ -r /etc/os-release ]]; then
        # shellcheck disable=SC1091
        . /etc/os-release
        id="${ID:-}"
        codename="${VERSION_CODENAME:-}"
        ubuntu_codename="${UBUNTU_CODENAME:-}"
        id_like="${ID_LIKE:-}"
    fi

    local distro="" suite=""
    if [[ "${id}" == debian || "${id}" == ubuntu ]] && [[ -n "${codename}" ]]; then
        distro="${id}"
        suite="${codename}"
    elif [[ -n "${ubuntu_codename}" ]]; then
        distro="ubuntu"
        suite="${ubuntu_codename}"
    elif [[ -n "${codename}" && "${id_like}" == *ubuntu* ]]; then
        distro="ubuntu"
        suite="${codename}"
    elif [[ -n "${codename}" && "${id_like}" == *debian* ]]; then
        distro="debian"
        suite="${codename}"
    fi

    if [[ -z "${distro}" ]]; then
        echo "Note: could not detect the distribution; using ${DEB_FALLBACK_DISTRO} ${DEB_FALLBACK_CODENAME} (packages are identical)." >&2
        printf '%s %s' "${DEB_FALLBACK_DISTRO}" "${DEB_FALLBACK_CODENAME}"
        return
    fi

    if url_missing "${PACKAGE_REPO_URL}/deb/${distro}/dists/${suite}/Release"; then
        echo "Note: ${distro} ${suite} is not indexed; using ${DEB_FALLBACK_DISTRO} ${DEB_FALLBACK_CODENAME} (packages are identical)." >&2
        printf '%s %s' "${DEB_FALLBACK_DISTRO}" "${DEB_FALLBACK_CODENAME}"
        return
    fi

    printf '%s %s' "${distro}" "${suite}"
}

register_rpm_repo() {
    local sudo="$1" manager repo_file
    manager="$(rpm_manager)"
    repo_file="$(rpm_repo_file "${manager}")"

    echo "  Configuring ${repo_file} for ${manager}"
    ${sudo} mkdir -p "$(dirname "${repo_file}")"
    ${sudo} tee "${repo_file}" >/dev/null <<EOF
[mezmo-aura]
name=mezmo-aura
baseurl=${PACKAGE_REPO_URL}/rpm/any-distro/any-version/\$basearch
repo_gpgcheck=1
gpgcheck=1
enabled=1
autorefresh=1
gpgkey=${PACKAGE_KEY_URL}
sslverify=1
metadata_expire=300
type=rpm-md
EOF
    ${sudo} chmod 0644 "${repo_file}"

    # zypper would otherwise prompt to trust the key and hang a piped install.
    if [[ "${manager}" == zypper ]]; then
        ${sudo} zypper --gpg-auto-import-keys --non-interactive refresh mezmo-aura
    fi
}

# zypper keeps its repositories outside the yum tree.
rpm_repo_file() {
    case "$1" in
        zypper) printf '%s' "${ZYPP_REPO_FILE}" ;;
        *)      printf '%s' "${YUM_REPO_FILE}" ;;
    esac
}

install_from_repo() {
    local format="$1" sudo="$2" manager target
    case "${format}" in
        deb) manager="apt-get" ;;
        rpm) manager="$(rpm_manager)" ;;
    esac

    local packages=()
    for target in $(targets); do
        packages+=("$(package_spec "${manager}" "${target}")")
    done

    case "${manager}" in
        apt-get) ${sudo} apt-get install -y "${packages[@]}" ;;
        zypper)  ${sudo} zypper --non-interactive install "${packages[@]}" ;;
        *)       ${sudo} "${manager}" install -y "${packages[@]}" ;;
    esac
}

# Version-pinned package name: apt and zypper take name=version, dnf name-version.
package_spec() {
    local manager="$1" name="$2"
    [[ "${VERSION}" == latest ]] && { printf '%s' "${name}"; return; }
    case "${manager}" in
        apt-get|zypper) printf '%s=%s' "${name}" "${VERSION}" ;;
        *)              printf '%s-%s' "${name}" "${VERSION}" ;;
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

# True only when the server reports the URL absent; a transport failure is not
# a negative answer.
url_missing() {
    local url="$1" status rc=0
    case "${DOWNLOADER}" in
        curl)
            status="$(curl -sIL --connect-timeout 10 -o /dev/null -w '%{http_code}' "${url}" 2>/dev/null)"
            [[ "${status}" == 404 ]]
            ;;
        wget)
            # wget exits 8 when the server answered with an error status.
            wget -q --spider --timeout=10 --tries=1 "${url}" 2>/dev/null || rc=$?
            [[ "${rc}" -eq 8 ]]
            ;;
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
