#!/bin/sh
# Copy a darwin-arm64 hearth binary into extras/hearth.
# cp (not ln) so the bundle gets its own inode. codesign of this copy must
# never target ~/.local/bin/hearth or a daemon that is still mapped there.
set -eu

root=$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)
dest="$root/extras/hearth"

if [ -n "${HEARTH_BIN:-}" ]; then
    src=$HEARTH_BIN
elif [ -x "$HOME/.local/bin/hearth" ]; then
    src=$(realpath "$HOME/.local/bin/hearth")
else
    echo "set HEARTH_BIN to a hearth binary" >&2
    exit 1
fi

if [ ! -f "$src" ]; then
    echo "hearth binary not found: $src" >&2
    exit 1
fi

mkdir -p "$root/extras"
cp "$src" "$dest"
chmod 755 "$dest"
echo "copied $src -> $dest"
