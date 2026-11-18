#!/usr/bin/env bash
# Removes Insight and everything it stored (index, media labels, downloaded model).
#   curl -fsSL https://raw.githubusercontent.com/ehmoratallar8990/insightsapp/main/uninstall.sh | bash
# Pass --keep-data to keep the index and model.
set -euo pipefail
KEEP=0; [ "${1:-}" = "--keep-data" ] && KEEP=1
pkill -x insight 2>/dev/null || true
case "$(uname -s)" in
  Darwin)
    rm -rf /Applications/Insight.app "$HOME/Applications/Insight.app"
    DATA="$HOME/Library/Application Support/insight"
    rm -rf "$HOME/Library/Application Support/Insight" "$HOME/Library/Saved Application State/io.github.ehmoratallar8990.insight.savedState"
    ;;
  Linux)
    rm -f "$HOME/.local/bin/insight" "$HOME/.local/share/applications/insight.desktop" "$HOME/.local/share/icons/hicolor/256x256/apps/insight.png"
    DATA="${XDG_DATA_HOME:-$HOME/.local/share}/insight"
    ;;
esac
if [ $KEEP -eq 0 ]; then rm -rf "$DATA"; echo "Removed Insight and its data."; else echo "Removed Insight (kept $DATA)."; fi
