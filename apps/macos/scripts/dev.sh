#!/bin/bash
# Dev loop: build + launch the app immediately, then rebuild and relaunch it whenever a Swift source
# changes. This is a fast rebuild+relaunch loop, not in-process hot code swapping (app state resets on
# every relaunch) — there's no Xcode project here to wire up an injection tool against; fswatch-driven
# rebuild is the pragmatic equivalent for a plain SPM package. Ctrl+C stops the watch and kills the app.
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
app_root="$script_dir/.."
cd "$app_root"

pid=""

cleanup() {
  if [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null; then
    kill "$pid" 2>/dev/null || true
    wait "$pid" 2>/dev/null || true
  fi
}
trap cleanup EXIT INT TERM

build_and_launch() {
  echo "==> building"
  if ! swift build; then
    echo "!! build failed — waiting for the next change"
    return
  fi
  if [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null; then
    kill "$pid" 2>/dev/null || true
    wait "$pid" 2>/dev/null || true
  fi
  bin_path="$(swift build --show-bin-path)"
  echo "==> launching $bin_path/HearthApp"
  "$bin_path/HearthApp" &
  pid=$!
}

build_and_launch

echo "==> watching Sources/ and Package.swift for changes (Ctrl+C to stop)"
# Process substitution (not a trailing pipe) keeps this loop in the current shell, so $pid set inside
# build_and_launch persists across iterations for the EXIT trap above.
while read -r _; do
  build_and_launch
done < <(fswatch -o -l 0.5 "$app_root/Sources" "$app_root/Package.swift")
