#!/usr/bin/env bash
# Bump the AURA Homebrew tap formulae to a released version and push to main,
# taking each sha256 from the release checksums file.
# --dry-run rewrites in a scratch clone and tests the push without sending it.
#
# Usage: bump-homebrew-tap.sh [--dry-run] <version>
#   DRY_RUN=1             - same as --dry-run
#   CHECKSUMS_FILE        - checksums path (default: dist/checksums.txt)
#   GH_TOKEN/GITHUB_TOKEN - tap push token
set -euo pipefail

USAGE="usage: $0 [--dry-run] <version>"

DRY_RUN="${DRY_RUN:-0}"
VERSION=""
while [ $# -gt 0 ]; do
  case "$1" in
    --dry-run) DRY_RUN=1; shift ;;
    -h|--help) echo "${USAGE}" >&2; exit 0 ;;
    *) VERSION="$1"; shift ;;
  esac
done
VERSION="${VERSION#v}"
[ -n "${VERSION}" ] || { echo "${USAGE}" >&2; exit 1; }

TAP_REPO="mezmo/homebrew-tap"
CHECKSUMS="${CHECKSUMS_FILE:-dist/checksums.txt}"
TOKEN="${GH_TOKEN:-${GITHUB_TOKEN:-}}"

HAVE_CHECKSUMS=0
if [ -f "${CHECKSUMS}" ]; then
  CHECKSUMS="$(cd "$(dirname "${CHECKSUMS}")" && pwd)/$(basename "${CHECKSUMS}")"
  HAVE_CHECKSUMS=1
elif [ "${DRY_RUN}" = 1 ]; then
  echo "warning: checksums file not found: ${CHECKSUMS} (dry run — validating version bump only, not hashes)" >&2
else
  echo "error: checksums file not found: ${CHECKSUMS}" >&2; exit 1
fi

if [ "${DRY_RUN}" != 1 ]; then
  [ -n "${TOKEN}" ] || { echo "error: GH_TOKEN or GITHUB_TOKEN required" >&2; exit 1; }
fi

WORKDIR="$(mktemp -d)"
trap 'rm -rf "${WORKDIR}"' EXIT

if [ -n "${TOKEN}" ]; then
  CLONE_URL="https://x-access-token:${TOKEN}@github.com/${TAP_REPO}.git"
else
  CLONE_URL="https://github.com/${TAP_REPO}.git"
fi
git clone --depth 1 "${CLONE_URL}" "${WORKDIR}/tap"
cd "${WORKDIR}/tap"

for f in Formula/*.rb; do
  awk -v ver="${VERSION}" -v ck="${CHECKSUMS}" -v have="${HAVE_CHECKSUMS}" '
    BEGIN { if (have) while ((getline l < ck) > 0) { n = split(l, a, /  +/); sums[a[2]] = a[1] } }
    /^[[:space:]]*version "/ { sub(/"[^"]+"/, "\"" ver "\""); print; next }
    /^[[:space:]]*url "/ {
      m = split($0, p, "/"); asset = p[m]; sub(/".*/, "", asset); pend = asset; print; next
    }
    pend != "" && /sha256 "/ {
      if (!have) { pend = ""; print; next }
      if (!(pend in sums)) { print "error: no checksum for " pend > "/dev/stderr"; exit 2 }
      sub(/"[0-9a-f]+"/, "\"" sums[pend] "\""); pend = ""; print; next
    }
    { print }
  ' "${f}" > "${f}.tmp"
  mv "${f}.tmp" "${f}"
done

if git diff --quiet; then
  echo "Formulae already at v${VERSION}; nothing to do"
  exit 0
fi

git commit -qam "bump aura formulae to v${VERSION}"

if [ "${DRY_RUN}" != 1 ]; then
  git push origin HEAD:main
  exit 0
fi

echo "== dry run: proposed changes to ${TAP_REPO} for v${VERSION} =="
git --no-pager show --format= HEAD
# --dry-run negotiates the push without sending it, to prove the token can push.
echo "== dry run: testing push to ${TAP_REPO} main (no refs updated) =="
if git push --dry-run origin HEAD:main; then
  echo "== dry run: push check passed; nothing pushed =="
else
  echo "error: push check failed — token cannot push to ${TAP_REPO} main" >&2
  exit 1
fi
