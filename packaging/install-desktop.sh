#!/usr/bin/env sh
set -eu

root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
binary=${1:-"$root/oplus-connect-gui/target/release/oplus-connect-gui"}
data=${XDG_DATA_HOME:-"$HOME/.local/share"}

if [ ! -x "$binary" ]; then
    echo "Build the release binary first: $binary" >&2
    exit 1
fi

install -Dm644 \
    "$root/oplus-connect-gui/assets/app-icon.png" \
    "$data/icons/hicolor/512x512/apps/oplus-connect.png"
mkdir -p "$data/applications"
sed "s|@EXEC@|$binary|" \
    "$root/packaging/oplus-connect.desktop.in" \
    >"$data/applications/oplus-connect.desktop"

if command -v update-desktop-database >/dev/null 2>&1; then
    update-desktop-database "$data/applications" >/dev/null 2>&1 || true
fi

echo "Installed OPLUS 互联 desktop entry and icon."
