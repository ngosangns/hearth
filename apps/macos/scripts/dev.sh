#!/bin/bash
# Dev loop: build + launch the app immediately, then rebuild and relaunch it whenever a Swift source
# changes. This is a fast rebuild+relaunch loop, not in-process hot code swapping (app state resets on
# every relaunch) — there's no Xcode project here to wire up an injection tool against; fswatch-driven
# rebuild is the pragmatic equivalent for a plain SPM package. Ctrl+C stops the watch and kills the app.
set -euo pipefail
# No `set -m`: with job control on, each foreground job (perl below, `swift build`) gets its own
# process group AND the terminal's foreground — Ctrl+C would deliver SIGINT only to that job, never
# to this shell, so the INT trap would never run and the loop would just respawn perl forever.
# Without it, SIGINT hits the whole foreground process group — script, watcher, and app all die
# together — and the EXIT trap still runs `stop_pid` on anything that survived (e.g. a binary that
# catches SIGINT).

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
app_root="$script_dir/.."
cd "$app_root"

app_pid=""
watch_pid=""
watch_dir=""

# SIGTERM, then SIGKILL. An unbounded `wait` in the Ctrl+C trap blocks every later Ctrl+C:
# bash does not deliver the signal again until the trap returns.
stop_pid() {
  local target="$1"
  if [ -z "$target" ] || ! kill -0 "$target" 2>/dev/null; then
    return 0
  fi
  kill "$target" 2>/dev/null || true
  local i=0
  while kill -0 "$target" 2>/dev/null && [ "$i" -lt 20 ]; do
    sleep 0.1
    i=$((i + 1))
  done
  if kill -0 "$target" 2>/dev/null; then
    kill -9 "$target" 2>/dev/null || true
  fi
  wait "$target" 2>/dev/null || true
}

cleanup() {
  trap - EXIT INT TERM
  stop_pid "$app_pid"
  stop_pid "$watch_pid"
  app_pid=""
  watch_pid=""
  if [ -n "$watch_dir" ]; then
    rm -rf "$watch_dir"
    watch_dir=""
  fi
}

# A trapped SIGINT is deferred until the current command finishes. The old loop blocked in
# `read` on fswatch, which ignores SIGINT, so the trap never ran. This handler exits; the
# watch loop blocks in `perl` instead, which Ctrl+C actually kills.
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

build_and_launch() {
  echo "==> building"
  if ! swift build; then
    echo "!! build failed — waiting for the next change"
    return
  fi
  stop_pid "$app_pid"
  app_pid=""
  bin_path="$(swift build --show-bin-path)"
  echo "==> launching $bin_path/HearthApp"
  "$bin_path/HearthApp" &
  app_pid=$!
}

build_and_launch

echo "==> watching Sources/ and Package.swift for changes (Ctrl+C to stop)"
# The fifo keeps this loop in the current shell, so $app_pid stays visible to the EXIT trap.
watch_dir="$(mktemp -d)"
watch_fifo="$watch_dir/events"
mkfifo "$watch_fifo"
exec 3<>"$watch_fifo"
fswatch -o -l 0.5 "$app_root/Sources" "$app_root/Package.swift" >"$watch_fifo" &
watch_pid=$!

# `read` cannot be the thing we block on: bash defers a trapped SIGINT until that
# builtin returns, and it never returns while fswatch (which ignores SIGINT) holds
# the pipe. `perl` is a normal foreground process, so Ctrl+C kills it and the trap runs.
while kill -0 "$watch_pid" 2>/dev/null; do
  # perl must stay inside `if` — a bare non-zero command under `set -e` would kill the script on
  # the first poll timeout, before any Ctrl+C ever lands.
  if /usr/bin/perl -e 'use IO::Select; exit(IO::Select->new(\*STDIN)->can_read(0.2) ? 0 : 1)' <&3; then
    IFS= read -r _ <&3 || true
    build_and_launch
  elif [ "$?" -ge 128 ]; then
    # perl died to a signal (SIGINT et al.) — treat it as our own: exit, let the traps clean up.
    exit 130
  fi
done
