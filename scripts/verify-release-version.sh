#!/usr/bin/env bash
# Assert the version being released is the one the artifacts were built from.
#
# Runs as the exec plugin's verifyReleaseCmd, the last step before any prepare
# or publish side effect.
#
# Usage: verify-release-version.sh <version>
#   NEXT_RELEASE_VERSION - previewed version to compare against
set -euo pipefail

ACTUAL="${1:-}"
[ -n "${ACTUAL}" ] || { echo "usage: $0 <version>" >&2; exit 1; }

EXPECTED="${NEXT_RELEASE_VERSION:-}"
if [ -z "${EXPECTED}" ]; then
  echo "warning: NEXT_RELEASE_VERSION unset; nothing to check ${ACTUAL} against" >&2
  exit 0
fi

if [ "${EXPECTED}" != "${ACTUAL}" ]; then
  echo "error: release version drifted from the version the artifacts were built from" >&2
  echo "  built:     ${EXPECTED}" >&2
  echo "  releasing: ${ACTUAL}" >&2
  exit 1
fi

echo "Releasing ${ACTUAL}, matching the artifacts already built"
