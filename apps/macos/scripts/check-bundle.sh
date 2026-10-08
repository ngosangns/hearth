#!/bin/sh
# Print where extras/hearth landed in the built .app, and the signing result.
# Developer ID + notarization are required before this app is distributed.
# This machine may only have an Apple Development identity, in which case
# codesign and spctl report that and exit 0 so the path check still prints.
set -eu

root=$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)
dist="$root/nativephp/electron/dist"
app=$(find "$dist" -name 'Hearth.app' -type d -print -quit 2>/dev/null || true)

if [ -z "$app" ]; then
    echo "no Hearth.app under $dist" >&2
    exit 1
fi

echo "app: $app"
hearth=$(find "$app" -name hearth -type f -print)
echo "hearth files:"
printf '%s\n' "$hearth"

case $hearth in
    *".asar/"*|*.asar)
        if ! printf '%s\n' "$hearth" | grep -q '.asar.unpacked/'; then
            echo "FAIL: hearth is inside app.asar" >&2
            exit 1
        fi
        ;;
esac

echo "--- codesign app ---"
codesign -dv --verbose=4 "$app" 2>&1 || true
echo "--- codesign hearth ---"
printf '%s\n' "$hearth" | while IFS= read -r file; do
    [ -n "$file" ] || continue
    codesign -dv --verbose=4 "$file" 2>&1 || true
done
echo "--- spctl ---"
spctl --assess --type execute --verbose=4 "$app" 2>&1 || true
echo "--- version ---"
printf '%s\n' "$hearth" | while IFS= read -r file; do
    [ -n "$file" ] || continue
    "$file" --version
done
