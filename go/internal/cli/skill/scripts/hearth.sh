#!/usr/bin/env bash
# Thin wrapper: hearth --root <project> …  (canonical skill path; MCP retired for agents)
set -euo pipefail

HEARTH_BIN="${HEARTH_BIN:-$HOME/.local/bin/hearth}"

resolve_root() {
  if [[ -n "${HEARTH_ROOT:-}" ]]; then
    printf '%s\n' "$HEARTH_ROOT"
    return 0
  fi
  local dir
  dir="$(pwd)"
  while true; do
    if [[ -f "$dir/hearth.yaml" || -f "$dir/hearth.yml" || -f "$dir/hearth.json" ]]; then
      printf '%s\n' "$dir"
      return 0
    fi
    local parent
    parent="$(dirname "$dir")"
    if [[ "$parent" == "$dir" ]]; then
      echo "hearth skill: no HEARTH_ROOT and no hearth.yaml/.yml/.json above $(pwd)" >&2
      return 1
    fi
    dir="$parent"
  done
}

ROOT="$(resolve_root)"
export HEARTH_ROOT="$ROOT"
if [[ ! -x "$HEARTH_BIN" ]]; then
  echo "hearth skill: binary not found or not executable: $HEARTH_BIN (set HEARTH_BIN)" >&2
  exit 127
fi
exec "$HEARTH_BIN" --root "$ROOT" "$@"
