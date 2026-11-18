#!/usr/bin/env bash
# Usage: ./scripts/release.sh 0.2.0
# Bumps the version, packages the macOS app, tags, pushes and publishes the GitHub release.
# The Release workflow then attaches the Windows and Linux builds.
set -euo pipefail
cd "$(dirname "$0")/.."
V=${1:?usage: release.sh <version>}
sed -i '' "s/^version = \".*\"/version = \"$V\"/" Cargo.toml
cargo test --release
./scripts/package-macos.sh
git add Cargo.toml Cargo.lock
git commit -m "Release v$V" || true
git tag "v$V"
git push origin main "v$V"
gh release create "v$V" dist/Insight-macOS-universal.zip --title "Insight v$V" --generate-notes
