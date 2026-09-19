#!/bin/bash
# Packages a local, ad-hoc-signed Local Services.app — NOT a distributable build. Ad-hoc signing
# (`codesign --sign -`, no Developer ID) satisfies Gatekeeper only loosely: launching via Finder will
# still show an "unidentified developer" prompt the first time (right-click > Open once), and copying
# this .app to a different machine is not expected to work at all. Shipping something that opens
# cleanly on someone else's Mac needs a real Apple Developer ID certificate + notarization — this
# script does not attempt that; see apps/macos/README.md's "Known limitations".
#
# Usage: apps/macos/scripts/build-app.sh [debug|release]  (default: release)
set -euo pipefail

configuration="${1:-release}"
script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
app_root="$script_dir/.."       # apps/macos
repo_root="$app_root/../.."     # local-services

cd "$app_root"
echo "==> swift build -c $configuration"
swift build -c "$configuration"
bin_path="$(swift build -c "$configuration" --show-bin-path)"
executable="$bin_path/LocalServicesApp"
if [ ! -x "$executable" ]; then
  echo "error: expected built executable at $executable" >&2
  exit 1
fi

app_bundle="$app_root/.build/Local Services.app"
rm -rf "$app_bundle"
mkdir -p "$app_bundle/Contents/MacOS" "$app_bundle/Contents/Resources/lsd"

echo "==> assembling bundle at $app_bundle"
cp "$executable" "$app_bundle/Contents/MacOS/LocalServicesApp"
cp "$app_root/Info.plist" "$app_bundle/Contents/Info.plist"
# Only src/ — not node_modules, not test/. This app only ever runs the `lsd` commands that touch
# core/cli (manager ensure/reload/stop, start/stop/restart, status, logs), none of which need tui's or
# mcp's dependencies; see SidecarLocator.swift's doc comment.
cp -R "$repo_root/src" "$app_bundle/Contents/Resources/lsd/src"

echo "==> ad-hoc codesign (local use only — see this script's header comment)"
codesign --force --deep --sign - "$app_bundle"

echo "==> done: $app_bundle"
echo "    First launch via Finder needs a right-click > Open (ad-hoc signed, not notarized)."
echo "    Requires a \`bun\` install on this machine — the app shells out to it, does not bundle one."
