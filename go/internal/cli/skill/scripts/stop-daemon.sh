#!/usr/bin/env bash
# Mutate: only when the user explicitly asked to stop the daemon (stops managed services too).
# Running this script is the confirmation. The child is not a TTY, so pass --yes.
set -euo pipefail
args=()
for a in "$@"; do
  if [[ "$a" != "--yes" ]]; then
    args+=("$a")
  fi
done
if [[ ${#args[@]} -eq 0 ]]; then
  exec "$(dirname "$0")/hearth.sh" manager stop --yes
fi
exec "$(dirname "$0")/hearth.sh" manager stop --yes "${args[@]}"
