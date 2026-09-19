#!/bin/bash
# Regenerates apps/macos/AppIcon.icns from make-icon.swift's code-drawn design. Run this whenever the
# icon design changes; the resulting .icns is committed (a normal binary asset, like any other app
# icon) so build-app.sh doesn't need to regenerate it on every packaging run.
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
app_root="$script_dir/.."
work_dir="$(mktemp -d)"
trap 'rm -rf "$work_dir"' EXIT

echo "==> rendering 1024x1024 base image"
swift "$script_dir/make-icon.swift" "$work_dir/icon-1024.png"

iconset="$work_dir/AppIcon.iconset"
mkdir -p "$iconset"
echo "==> generating iconset sizes"
declare -a sizes=(16 32 128 256 512)
for size in "${sizes[@]}"; do
  sips -z "$size" "$size" "$work_dir/icon-1024.png" --out "$iconset/icon_${size}x${size}.png" >/dev/null
  double=$((size * 2))
  sips -z "$double" "$double" "$work_dir/icon-1024.png" --out "$iconset/icon_${size}x${size}@2x.png" >/dev/null
done

echo "==> iconutil -c icns"
iconutil -c icns "$iconset" -o "$app_root/AppIcon.icns"
echo "==> done: $app_root/AppIcon.icns"
