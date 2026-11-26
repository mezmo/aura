#!/bin/sh
# Install cargo-hakari at <dest> for the platform this script runs on, after
# checking the release archive against its pinned SHA-256.
# Usage: install-cargo-hakari.sh <version> <dest> <sha256 linux amd64> <sha256 linux arm64> <sha256 darwin>
set -eu

version=$1
dest=$2
case "$(uname -s)/$(uname -m)" in
  Darwin/*) target=universal-apple-darwin; sha=$5 ;;
  Linux/aarch64 | Linux/arm64) target=aarch64-unknown-linux-gnu; sha=$4 ;;
  Linux/x86_64) target=x86_64-unknown-linux-gnu; sha=$3 ;;
  *) echo "cargo-hakari: unsupported platform $(uname -s)/$(uname -m)" >&2; exit 1 ;;
esac

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

echo "Downloading cargo-hakari $version for $target"
curl -LsSf -o "$tmp/archive.tar.gz" \
  "https://github.com/guppy-rs/guppy/releases/download/cargo-hakari-$version/cargo-hakari-$version-$target.tar.gz"

if command -v sha256sum >/dev/null; then
  actual=$(sha256sum "$tmp/archive.tar.gz" | cut -d' ' -f1)
else
  actual=$(shasum -a 256 "$tmp/archive.tar.gz" | cut -d' ' -f1)
fi
if [ "$actual" != "$sha" ]; then
  echo "cargo-hakari: checksum mismatch for $target: got $actual" >&2
  exit 1
fi

tar xzf "$tmp/archive.tar.gz" -C "$tmp" cargo-hakari
chmod +x "$tmp/cargo-hakari"
mkdir -p "$(dirname "$dest")"
mv "$tmp/cargo-hakari" "$dest"
