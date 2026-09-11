#!/usr/bin/env bash
# Exit 10 when HEAD is missing the latest release tag on main. Channel
# branches derive their version from the tags they can reach, so one that has
# not taken the main → channel back-merge releases from an older baseline.
# See docs/design/release-channels.md.
#
# Any other non-zero exit is a failure to evaluate, not a verdict.
#
# Usage: check-release-baseline.sh [main-ref]

set -euo pipefail

STALE=10
main_ref="${1:-main}"

if ! main_sha="$(git rev-parse --verify --quiet "${main_ref}^{commit}")"; then
  echo "error: ${main_ref} does not resolve to a commit" >&2
  exit 1
fi

# Prerelease tags are excluded so this is the last stable release alone.
latest_stable="$(git tag --merged "$main_sha" --list 'v*' |
  sed -n 's/^v\([0-9][0-9.]*\)$/\1/p' | sort -V | tail -n 1)"

if [ -z "$latest_stable" ]; then
  echo "no stable tag reachable from ${main_ref}; nothing to be behind" >&2
  exit 0
fi

if git merge-base --is-ancestor "v${latest_stable}" HEAD; then
  exit 0
fi

echo "HEAD is missing v${latest_stable}, the latest release on ${main_ref}." >&2
echo "Merge the sync pull request, or delete beta once it is promoted." >&2
exit "$STALE"
