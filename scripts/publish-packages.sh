#!/usr/bin/env bash
# Publish the Debian (.deb) and RPM (.rpm) packages in dist/ to Cloudsmith.
#
# Uploads wait for server-side synchronisation, so a package Cloudsmith rejects
# fails this script. --dry-run uploads nothing; the server still authenticates,
# so it verifies the key and the target repository.
#
# Usage: publish-packages.sh [--dry-run]
#   DRY_RUN=1          - same as --dry-run
#
# Environment:
#   CLOUDSMITH_API_KEY - API key with write access to the repository (required)
#   CLOUDSMITH_REPO    - owner/repository to publish to (default: mezmo/aura)
#   CLOUDSMITH_DISTRO  - distribution/version coordinates (default: any-distro/any-version)
#   DIST_DIR           - directory holding the packages (default: dist)
#   PACKAGERS          - space-separated formats to publish (default: "deb rpm")
#   CLOUDSMITH         - cloudsmith executable to use (default: cloudsmith on PATH)
set -euo pipefail

USAGE="usage: $0 [--dry-run]"

DRY_RUN="${DRY_RUN:-0}"
while [ $# -gt 0 ]; do
    case "$1" in
        --dry-run) DRY_RUN=1; shift ;;
        -h|--help) echo "${USAGE}" >&2; exit 0 ;;
        *) echo "${USAGE}" >&2; exit 1 ;;
    esac
done

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"

DIST_DIR="${DIST_DIR:-${PROJECT_ROOT}/dist}"
PACKAGERS="${PACKAGERS:-deb rpm}"
CLOUDSMITH="${CLOUDSMITH:-cloudsmith}"
CLOUDSMITH_REPO="${CLOUDSMITH_REPO:-mezmo/aura}"
CLOUDSMITH_DISTRO="${CLOUDSMITH_DISTRO:-any-distro/any-version}"

require_cloudsmith() {
    if ! command -v "${CLOUDSMITH}" >/dev/null 2>&1; then
        echo "error: cloudsmith not found (set CLOUDSMITH or install from https://docs.cloudsmith.com/developer/cli)" >&2
        exit 1
    fi
    if [ -z "${CLOUDSMITH_API_KEY:-}" ]; then
        echo "error: CLOUDSMITH_API_KEY is required" >&2
        exit 1
    fi
}

main() {
    local target="${CLOUDSMITH_REPO}/${CLOUDSMITH_DISTRO}"

    shopt -s nullglob
    local packager packages=()
    for packager in ${PACKAGERS}; do
        packages+=("${DIST_DIR}"/*."${packager}")
    done

    if [ "${#packages[@]}" -eq 0 ]; then
        echo "error: no packages found in ${DIST_DIR}" >&2
        exit 1
    fi

    require_cloudsmith

    if [ "${DRY_RUN}" = 1 ]; then
        echo "Validating ${#packages[@]} package(s) against ${target} (dry run)"
    else
        echo "Publishing ${#packages[@]} package(s) from ${DIST_DIR} to ${target}"
    fi

    local file
    for file in "${packages[@]}"; do
        packager="${file##*.}"
        echo "  ${packager} $(basename "${file}")"
        if [ "${DRY_RUN}" = 1 ]; then
            "${CLOUDSMITH}" push "${packager}" --dry-run "${target}" "${file}"
        else
            "${CLOUDSMITH}" push "${packager}" "${target}" "${file}"
        fi
    done

    echo ""
    if [ "${DRY_RUN}" = 1 ]; then
        echo "Validated ${#packages[@]} package(s) against ${target}"
    else
        echo "Published ${#packages[@]} package(s) to ${target}"
    fi
}

main "$@"
