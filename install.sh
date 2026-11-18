#!/usr/bin/env bash
# Insight installer for macOS and Linux.
#   curl -fsSL https://raw.githubusercontent.com/ehmoratallar8990/insightsapp/main/install.sh | bash
set -euo pipefail
REPO="ehmoratallar8990/insightsapp"
BASE="https://github.com/$REPO/releases/latest/download"
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT
say() { printf '\033[1;35m▸\033[0m %s\n' "$*"; }

case "$(uname -s)" in
  Darwin)
    say "Downloading Insight for macOS (universal)…"
    curl -fL --progress-bar "$BASE/Insight-macOS-universal.zip" -o "$TMP/Insight.zip"
    ditto -x -k "$TMP/Insight.zip" "$TMP"
    DEST=/Applications
    [ -w "$DEST" ] || DEST="$HOME/Applications"
    mkdir -p "$DEST"
    if pgrep -xq insight; then say "Quitting the running copy…"; pkill -x insight || true; sleep 1; fi
    rm -rf "$DEST/Insight.app"
    mv "$TMP/Insight.app" "$DEST/"
    # The app is ad-hoc signed, not notarized: drop the download quarantine flag.
    xattr -dr com.apple.quarantine "$DEST/Insight.app" 2>/dev/null || true
    say "Installed to $DEST/Insight.app"
    say "Tip: for full coverage, add Insight under System Settings → Privacy & Security → Full Disk Access."
    open "$DEST/Insight.app"
    ;;
  Linux)
    [ "$(uname -m)" = "x86_64" ] || { echo "Prebuilt binaries are x86_64 only; build from source (see README)." >&2; exit 1; }
    say "Downloading Insight for Linux…"
    curl -fL --progress-bar "$BASE/Insight-Linux-x86_64.tar.gz" -o "$TMP/insight.tar.gz"
    tar -C "$TMP" -xzf "$TMP/insight.tar.gz"
    BIN="$HOME/.local/bin"; APPS="$HOME/.local/share/applications"; ICONS="$HOME/.local/share/icons/hicolor/256x256/apps"
    mkdir -p "$BIN" "$APPS" "$ICONS"
    install -m 755 "$TMP/insight/insight" "$BIN/insight"
    install -m 644 "$TMP/insight/insight.png" "$ICONS/insight.png"
    sed "s|^Exec=.*|Exec=$BIN/insight|" "$TMP/insight/insight.desktop" > "$APPS/insight.desktop"
    command -v update-desktop-database >/dev/null && update-desktop-database "$APPS" 2>/dev/null || true
    say "Installed $BIN/insight (and a menu entry)."
    case ":$PATH:" in *":$BIN:"*) ;; *) say "Add $BIN to your PATH to run 'insight' from a terminal.";; esac
    (nohup "$BIN/insight" >/dev/null 2>&1 &) || true
    ;;
  *)
    echo "On Windows use PowerShell:  irm https://raw.githubusercontent.com/$REPO/main/install.ps1 | iex" >&2
    exit 1
    ;;
esac
