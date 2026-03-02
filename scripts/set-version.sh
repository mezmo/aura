#!/bin/bash
# Replacement for `cargo set-version` from cargo-edit
# cargo-edit has dependency issues with Rust edition2024 requirements
# This script updates version in workspace Cargo.toml files

set -e

VERSION="$1"

if [ -z "$VERSION" ]; then
    echo "Usage: $0 <version>"
    exit 1
fi

echo "Setting version to $VERSION"

# Update workspace Cargo.toml
if [ -f "Cargo.toml" ]; then
    # Use sed to update version in [workspace.package] section
    # Match: version = "x.y.z" and replace with new version
    if grep -q '\[workspace\.package\]' Cargo.toml; then
        # For workspace Cargo.toml with [workspace.package]
        sed -i.bak -E "s/^(version = \")[^\"]+(\")$/\1${VERSION}\2/" Cargo.toml
        rm -f Cargo.toml.bak
        echo "Updated Cargo.toml"
    fi
fi

# Update all crate Cargo.toml files that have their own version
for toml in crates/*/Cargo.toml; do
    if [ -f "$toml" ]; then
        # Only update if it has a version field (not workspace = true)
        if grep -q '^version = "[0-9]' "$toml"; then
            sed -i.bak -E "s/^(version = \")[^\"]+(\")$/\1${VERSION}\2/" "$toml"
            rm -f "$toml.bak"
            echo "Updated $toml"
        fi
    fi
done

echo "Version update complete"
