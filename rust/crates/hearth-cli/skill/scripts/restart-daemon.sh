#!/usr/bin/env bash
# Mutate: only when the user explicitly asked to restart the daemon.
set -euo pipefail
exec "$(dirname "$0")/hearth.sh" manager restart "$@"
