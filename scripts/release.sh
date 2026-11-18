#!/usr/bin/env bash
# Usage: ./scripts/release.sh 0.2.0
# Bumps the version and pushes. The Release workflow then builds macOS, Windows and
# Linux, tags v<version> and publishes the GitHub release automatically.
set -euo pipefail
cd "$(dirname "$0")/.."
V=${1:?usage: release.sh <version>}
sed -i.bak "s/^version = \".*\"/version = \"$V\"/" Cargo.toml && rm Cargo.toml.bak
cargo test --release
git add Cargo.toml Cargo.lock
git commit -m "Release v$V"
git push origin main
echo "Pushed. Follow the release at: $(gh repo view --json url -q .url)/actions/workflows/release.yml"
