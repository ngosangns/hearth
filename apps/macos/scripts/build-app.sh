#!/bin/bash
# Packages a local, ad-hoc-signed Hearth.app — NOT a distributable build. Ad-hoc signing
# (`codesign --sign -`, no Developer ID) satisfies Gatekeeper only loosely: launching via Finder will
# still show an "unidentified developer" prompt the first time (right-click > Open once) — on this
# machine and on any other the release zip (.github/workflows/release.yml) is unpacked on. Shipping
# something that opens cleanly on someone else's Mac needs a real Apple Developer ID certificate +
# notarization — this script does not attempt that; see apps/macos/README.md's "Known limitations".
#
# Usage: apps/macos/scripts/build-app.sh [debug|release]  (default: release)
set -euo pipefail

# Copies Sparkle.framework out of the SwiftPM artifact into the app bundle. ditto keeps the
# framework's symlinks and its existing code signature intact.
embed_sparkle() {
  local app_bundle="$1"
  local framework="" fallback="" path
  while IFS= read -r path; do
    case "$path" in
      *.app/*) continue ;;
      *macos-arm64_x86_64*) framework="$path"; break ;;
    esac
    fallback="$path"
  done < <(find "$app_root/.build" -type d -name Sparkle.framework)
  if [ -z "$framework" ]; then
    framework="$fallback"
  fi
  if [ -z "$framework" ]; then
    echo "error: Sparkle.framework not found under $app_root/.build (did swift build resolve Sparkle?)" >&2
    exit 1
  fi
  echo "==> embedding Sparkle.framework from $framework"
  mkdir -p "$app_bundle/Contents/Frameworks"
  ditto "$framework" "$app_bundle/Contents/Frameworks/Sparkle.framework"
}

configuration="${1:-release}"
script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
app_root="$script_dir/.."       # apps/macos
repo_root="$app_root/../.."     # hearth

cd "$app_root"
echo "==> swift build -c $configuration"
swift build -c "$configuration"
bin_path="$(swift build -c "$configuration" --show-bin-path)"
executable="$bin_path/HearthApp"
if [ ! -x "$executable" ]; then
  echo "error: expected built executable at $executable" >&2
  exit 1
fi

# Rust `hearthd` release binary — bundled so a packaged app needs no extra install.
# SidecarLocator.findHearthdBinary() checks this bundled copy first. Built with the
# workspace's own Cargo, independent of `configuration` above (Swift's debug/release, not Cargo's).
rust_dir="$repo_root/rust"
echo "==> cargo build --release -p hearthd"
(cd "$rust_dir" && cargo build --release -p hearthd)
hearthd_binary="$rust_dir/target/release/hearthd"
if [ ! -x "$hearthd_binary" ]; then
  echo "error: expected built hearthd binary at $hearthd_binary" >&2
  exit 1
fi

app_bundle="$app_root/.build/Hearth.app"
rm -rf "$app_bundle"
mkdir -p "$app_bundle/Contents/MacOS" "$app_bundle/Contents/Resources/hearthd/bin"

echo "==> assembling bundle at $app_bundle"
cp "$executable" "$app_bundle/Contents/MacOS/HearthApp"
cp "$app_root/Info.plist" "$app_bundle/Contents/Info.plist"
printf 'APPL????' > "$app_bundle/Contents/PkgInfo"
if [ -f "$app_root/AppIcon.icns" ]; then
  cp "$app_root/AppIcon.icns" "$app_bundle/Contents/Resources/AppIcon.icns"
else
  echo "warning: $app_root/AppIcon.icns not found — run scripts/generate-icon.sh first" >&2
fi
cp "$hearthd_binary" "$app_bundle/Contents/Resources/hearthd/bin/hearthd"
embed_sparkle "$app_bundle"

# The SwiftPM binary's rpath points at the .build directory. The packaged app loads Sparkle from
# Contents/Frameworks instead. install_name_tool must run before codesign.
app_executable="$app_bundle/Contents/MacOS/HearthApp"
if ! otool -l "$app_executable" | grep -q '@executable_path/../Frameworks'; then
  install_name_tool -add_rpath @executable_path/../Frameworks "$app_executable"
fi

version="$("$hearthd_binary" --version | awk '{print $NF}')"
if [ -n "$version" ]; then
  /usr/libexec/PlistBuddy -c "Set :CFBundleShortVersionString $version" "$app_bundle/Contents/Info.plist"
  /usr/libexec/PlistBuddy -c "Set :CFBundleVersion $version" "$app_bundle/Contents/Info.plist"
  echo "==> stamped bundle version $version from hearthd"
fi

entitlements="$app_root/HearthApp.entitlements"
echo "==> ad-hoc codesign with hardened runtime (local use only — see this script's header comment)"
# Sign hearthd, then the app. Omit --deep so codesign seals nested Sparkle code in one pass
# instead of rewriting it twice. The framework ends up ad-hoc, same as the app;
# disable-library-validation is what lets the hardened runtime load it.
codesign --force --sign - --options runtime "$app_bundle/Contents/Resources/hearthd/bin/hearthd"
if [ -f "$entitlements" ]; then
  codesign --force --sign - --options runtime --entitlements "$entitlements" "$app_bundle"
else
  codesign --force --sign - --options runtime "$app_bundle"
fi
codesign --verify --deep --strict "$app_bundle"

echo "==> done: $app_bundle"
echo "    First launch via Finder needs a right-click > Open (ad-hoc signed, not notarized)."
echo "    Bundles a compiled \`hearthd\` sidecar and Sparkle.framework."
echo "    Notarization still needs a Developer ID certificate — see apps/macos/README.md."
