#!/usr/bin/env bash
# Build a universal (Apple silicon + Intel) Insight.app and zip it into dist/.
set -euo pipefail
cd "$(dirname "$0")/.."
VERSION=$(grep -m1 '^version' Cargo.toml | cut -d'"' -f2)
rustup target add aarch64-apple-darwin x86_64-apple-darwin >/dev/null
cargo build --release --target aarch64-apple-darwin
cargo build --release --target x86_64-apple-darwin

APP=dist/Insight.app
rm -rf dist && mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
lipo -create -output "$APP/Contents/MacOS/insight" \
  target/aarch64-apple-darwin/release/insight target/x86_64-apple-darwin/release/insight
sed "s/__VERSION__/$VERSION/g" packaging/Info.plist > "$APP/Contents/Info.plist"

ICONSET=$(mktemp -d)/Insight.iconset && mkdir -p "$ICONSET"
for s in 16 32 128 256 512; do
  cp "assets/icon-$s.png" "$ICONSET/icon_${s}x${s}.png"
  d=$((s * 2)); cp "assets/icon-$d.png" "$ICONSET/icon_${s}x${s}@2x.png" 2>/dev/null || true
done
cp assets/icon-64.png "$ICONSET/icon_32x32@2x.png"
iconutil -c icns "$ICONSET" -o "$APP/Contents/Resources/Insight.icns"

# Ad-hoc signature (required on Apple silicon). Not notarized.
codesign --force --deep --sign - "$APP"
(cd dist && ditto -c -k --keepParent Insight.app Insight-macOS-universal.zip)
echo "dist/Insight-macOS-universal.zip ($(du -h dist/Insight-macOS-universal.zip | cut -f1))"
