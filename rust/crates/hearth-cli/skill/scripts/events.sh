#!/usr/bin/env bash
# Lifecycle events via daemon HTTP (no CLI subcommand for events).
set -euo pipefail
AFTER=""
EPOCH=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --after) AFTER="$2"; shift 2 ;;
    --epoch) EPOCH="$2"; shift 2 ;;
    --json) shift ;;
    *) echo "usage: events.sh [--after N] [--epoch E]" >&2; exit 2 ;;
  esac
done
HERE="$(cd "$(dirname "$0")" && pwd)"
ENSURE_JSON="$("$HERE/hearth.sh" manager ensure --json)"
python3 - "$ENSURE_JSON" "$AFTER" "$EPOCH" <<'PY'
import json, sys, urllib.parse, urllib.request

ensure = json.loads(sys.argv[1])
after, epoch = sys.argv[2], sys.argv[3]
port = ensure.get("port")
token = ensure.get("token")
if not port or not token:
    print("manager ensure did not return port/token", file=sys.stderr)
    sys.exit(1)
q = []
if after:
    q.append(f"after={urllib.parse.quote(str(after))}")
if epoch:
    q.append(f"epoch={urllib.parse.quote(epoch)}")
qs = ("?" + "&".join(q)) if q else ""
req = urllib.request.Request(
    f"http://127.0.0.1:{port}/v1/events{qs}",
    headers={"Authorization": f"Bearer {token}"},
)
with urllib.request.urlopen(req, timeout=30) as resp:
    sys.stdout.write(resp.read().decode())
PY
