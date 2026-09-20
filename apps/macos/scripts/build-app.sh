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

# Rust `lsd` release binary — bundled so a packaged app needs no `bun` install at all
# (SidecarLocator.findLsdBinary() checks this bundled copy before any fallback). Built with the
# workspace's own Cargo, independent of `configuration` above (Swift's debug/release, not Cargo's).
rust_dir="$repo_root/rust"
echo "==> cargo build --release -p lsd"
(cd "$rust_dir" && cargo build --release -p lsd)
lsd_binary="$rust_dir/target/release/lsd"
if [ ! -x "$lsd_binary" ]; then
  echo "error: expected built lsd binary at $lsd_binary" >&2
  exit 1
fi

app_bundle="$app_root/.build/Local Services.app"
rm -rf "$app_bundle"
# NOTE: deliberately does NOT pre-create Resources/lsd/src. `cp -R src <dest>` copies the directory
# *into* <dest> when <dest> already exists, which put the fallback at lsd/src/src/bin/lsd.ts —
# a path SidecarLocator.findLsdEntry() does not look at, silently disabling the bundled fallback.
mkdir -p "$app_bundle/Contents/MacOS" "$app_bundle/Contents/Resources/lsd/bin"

echo "==> assembling bundle at $app_bundle"
cp "$executable" "$app_bundle/Contents/MacOS/LocalServicesApp"
cp "$app_root/Info.plist" "$app_bundle/Contents/Info.plist"
if [ -f "$app_root/AppIcon.icns" ]; then
  cp "$app_root/AppIcon.icns" "$app_bundle/Contents/Resources/AppIcon.icns"
else
  echo "warning: $app_root/AppIcon.icns not found — run scripts/generate-icon.sh first" >&2
fi
cp "$lsd_binary" "$app_bundle/Contents/Resources/lsd/bin/lsd"
# Only src/ — not node_modules, not test/. Kept as a fallback for a machine where the bundled `lsd`
# binary somehow can't run; see SidecarLocator.swift's doc comment. This app only ever runs the `lsd`
# commands that touch core/cli (manager ensure/reload/stop, start/stop/restart, status, logs), none
# of which need tui's or mcp's dependencies, so the fallback bundles only `src/`.
cp -R "$repo_root/src" "$app_bundle/Contents/Resources/lsd/src"

echo "==> ad-hoc codesign (local use only — see this script's header comment)"
codesign --force --sign - "$app_bundle/Contents/Resources/lsd/bin/lsd"
codesign --force --deep --sign - "$app_bundle"

echo "==> done: $app_bundle"
echo "    First launch via Finder needs a right-click > Open (ad-hoc signed, not notarized)."
echo "    Bundles a compiled \`lsd\` — no \`bun\` install needed on this machine for the common path;"
echo "    bun is only needed as a fallback, or if this workspace's own catalog uses the .config.ts escape hatch."
