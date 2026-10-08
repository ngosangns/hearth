#!/bin/sh
# Build Hearth.app: release swift build, bundle layout, ad-hoc codesign.
#
#   apps/macos-swiftui/scripts/bundle.sh
#
# Produces apps/macos-swiftui/.build/Hearth.app. The hearth CLI is copied from HEARTH_BIN
# (default ~/.local/bin/hearth, resolved through its symlink so the versioned file lands in
# Contents/extras/, matching `task install`'s layout). The app version is the hearth version.
set -eu

cd "$(dirname "$0")/.."

VERSION="$(awk -F'"' '$1 ~ /^version = / { print $2; exit }' ../../rust/bin/hearth/Cargo.toml)"
test -n "$VERSION" || { echo "cannot read the hearth version from rust/bin/hearth/Cargo.toml" >&2; exit 1; }

HEARTH_BIN="${HEARTH_BIN:-$HOME/.local/bin/hearth}"
if [ -L "$HEARTH_BIN" ]; then
    resolved="$(readlink "$HEARTH_BIN")"
    case "$resolved" in
        /*) HEARTH_BIN="$resolved" ;;
        *)  HEARTH_BIN="$(dirname "$HEARTH_BIN")/$resolved" ;;
    esac
fi
test -x "$HEARTH_BIN" || { echo "hearth binary not found or not executable: $HEARTH_BIN" >&2; exit 1; }

echo "==> swift build -c release"
swift build -c release
BIN="$(swift build -c release --show-bin-path)/hearth-app"
APP=.build/Hearth.app

echo "==> bundling $APP ($VERSION)"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/extras" "$APP/Contents/Resources"
cp "$BIN" "$APP/Contents/MacOS/Hearth"
cp "$HEARTH_BIN" "$APP/Contents/extras/hearth"
chmod 755 "$APP/Contents/extras/hearth"
sed "s/@VERSION@/$VERSION/g" Resources/Info.plist > "$APP/Contents/Info.plist"

echo "==> app icon"
ICONSET="$(mktemp -d)/AppIcon.iconset"
mkdir -p "$ICONSET"
swift scripts/make-icon.swift "$ICONSET"
iconutil -c icns "$ICONSET" -o "$APP/Contents/Resources/AppIcon.icns"
rm -rf "$(dirname "$ICONSET")"

# Ad-hoc signing runs on this Mac. Distribution needs a Developer ID and notarization.
echo "==> codesign (ad-hoc)"
codesign --force --sign - "$APP/Contents/extras/hearth"
codesign --force --sign - --identifier ai.hearth.app "$APP"
codesign --verify --deep --strict "$APP"

echo "==> verify"
"$APP/Contents/extras/hearth" --version
echo "built: $APP"
