#!/usr/bin/env bash
# Mutate: only when the user explicitly asked to stop the daemon (stops managed services too).
set -euo pipefail
exec "$(dirname "$0")/hearth.sh" manager stop "$@"
