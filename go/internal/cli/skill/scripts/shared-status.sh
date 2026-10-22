#!/usr/bin/env bash
# Shared instance status for agents. The CLI masks secrets unless --reveal.
# --mask is accepted by the CLI and changes nothing; this script drops it.
# Never pass --reveal unless the user asked for the value.
set -euo pipefail

REVEAL=0
MASK=0
ARGS=()
for a in "$@"; do
  case "$a" in
    --reveal) REVEAL=1 ;;
    --mask) MASK=1 ;;
    *) ARGS+=("$a") ;;
  esac
done

if [[ "$REVEAL" == 1 && "$MASK" == 1 ]]; then
  echo "usage: shared-status.sh [--json] [--reveal]" >&2
  echo "--mask and --reveal cannot be used together" >&2
  exit 2
fi
if [[ "$REVEAL" == 1 ]]; then
  ARGS+=(--reveal)
fi
if [[ ${#ARGS[@]} -eq 0 ]]; then
  exec "$(dirname "$0")/hearth.sh" shared status
fi
exec "$(dirname "$0")/hearth.sh" shared status "${ARGS[@]}"
