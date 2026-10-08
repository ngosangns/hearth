#!/bin/sh
# Build the AppKit Hearth.app: release swift build, bundle layout, ad-hoc codesign.
#
#   apps/macos-appkit/scripts/bundle.sh
#
# Produces apps/macos-appkit/.build/Hearth.app. The hearth CLI binary is copied
# from HEARTH_BIN (default ~/.local/bin/hearth — resolved through its symlink so
# the versioned file lands in extras/, matching `task install`'s layout).
set -eu

cd "$(dirname "$0")/.."

VERSION=0.1.0
IDENTIFIER=ai.hearth.app

echo "==> swift build -c release"
swift build -c release

BIN=.build/release/Hearth
APP=.build/Hearth.app

HEARTH_BIN="${HEARTH_BIN:-$HOME/.local/bin/hearth}"
# Follow the ~/.local/bin symlink to the versioned binary.
if [ -L "$HEARTH_BIN" ]; then
    resolved="$(readlink "$HEARTH_BIN")"
    case "$resolved" in
        /*) HEARTH_BIN="$resolved" ;;
        *)  HEARTH_BIN="$(dirname "$HEARTH_BIN")/$resolved" ;;
    esac
fi
test -x "$HEARTH_BIN" || { echo "hearth binary not found or not executable: $HEARTH_BIN" >&2; exit 1; }

echo "==> bundling $APP"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/extras" "$APP/Contents/Resources"
cp "$BIN" "$APP/Contents/MacOS/Hearth"
cp "$HEARTH_BIN" "$APP/Contents/extras/hearth"
chmod 755 "$APP/Contents/extras/hearth"

echo "==> app icon"
ICONSET="$(mktemp -d)/AppIcon.iconset"
mkdir -p "$ICONSET"
swift scripts/make-icon.swift "$ICONSET"
iconutil -c icns "$ICONSET" -o "$APP/Contents/Resources/AppIcon.icns"
rm -rf "$(dirname "$ICONSET")"

cat > "$APP/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleExecutable</key><string>Hearth</string>
    <key>CFBundleIdentifier</key><string>$IDENTIFIER</string>
    <key>CFBundleName</key><string>Hearth</string>
    <key>CFBundleDisplayName</key><string>Hearth</string>
    <key>CFBundlePackageType</key><string>APPL</string>
    <key>CFBundleIconFile</key><string>AppIcon</string>
    <key>CFBundleShortVersionString</key><string>$VERSION</string>
    <key>CFBundleVersion</key><string>$VERSION</string>
    <key>LSMinimumSystemVersion</key><string>13.0</string>
    <key>NSHighResolutionCapable</key><true/>
    <key>LSApplicationCategoryType</key><string>public.app-category.developer-tools</string>
</dict>
</plist>
EOF

# Ad-hoc sign: sufficient for local runs. A Developer ID + notarization is what a
# shipped build needs — same limitation the electron build documents.
echo "==> codesign (ad-hoc)"
codesign --force --sign - "$APP/Contents/extras/hearth" 2>/dev/null || true
codesign --force --sign - "$APP"

echo "==> verify"
"$APP/Contents/extras/hearth" --version
ls -la "$APP/Contents/MacOS" "$APP/Contents/extras"
echo "built: $APP"
