#!/usr/bin/env bash
set -euo pipefail
if [[ $# -lt 1 ]]; then
  echo "usage: trace.sh <operationId> [--json]" >&2
  exit 2
fi
exec "$(dirname "$0")/hearth.sh" operation get "$@"
