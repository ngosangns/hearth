#!/usr/bin/env bash
# Mutate: only when the user explicitly asked to start|stop|restart.
set -euo pipefail
if [[ $# -lt 2 ]]; then
  echo "usage: manage.sh start|stop|restart <target> [--wait] [--kill-unowned]" >&2
  exit 2
fi
action="$1"; shift
case "$action" in
  start|stop|restart) ;;
  *) echo "usage: manage.sh start|stop|restart <target> [--wait] [--kill-unowned]" >&2; exit 2 ;;
esac
exec "$(dirname "$0")/hearth.sh" "$action" "$@"
