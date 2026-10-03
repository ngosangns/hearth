#!/usr/bin/env bash
# Print this project's provisioned connection for an attached shared service.
set -euo pipefail
if [[ $# -lt 1 ]]; then
  echo "usage: shared-connection.sh <service|name@version> [--json]" >&2
  exit 2
fi
SERVICE="$1"; shift || true
WANT_JSON=0
for a in "$@"; do
  case "$a" in
    --json) WANT_JSON=1 ;;
    *) echo "usage: shared-connection.sh <service|name@version> [--json]" >&2; exit 2 ;;
  esac
done

HERE="$(cd "$(dirname "$0")" && pwd)"
# Resolve root the same way hearth.sh does (without requiring a running daemon).
ROOT="${HEARTH_ROOT:-}"
if [[ -z "$ROOT" ]]; then
  dir="$(pwd)"
  while true; do
    if [[ -f "$dir/hearth.yaml" || -f "$dir/hearth.yml" || -f "$dir/hearth.json" ]]; then
      ROOT="$dir"; break
    fi
    parent="$(dirname "$dir")"
    if [[ "$parent" == "$dir" ]]; then
      echo "hearth skill: no HEARTH_ROOT and no hearth.yaml above $(pwd)" >&2
      exit 1
    fi
    dir="$parent"
  done
fi
export HEARTH_ROOT="$ROOT"
STATUS_JSON="$("$HERE/hearth.sh" shared status --json)"
python3 - "$SERVICE" "$WANT_JSON" "$ROOT" "$STATUS_JSON" <<'PY'
import hashlib, json, os, sys

service, want_json, root, status_raw = sys.argv[1], sys.argv[2] == "1", sys.argv[3], sys.argv[4]
status = json.loads(status_raw)
canon = os.path.realpath(root)
digest = hashlib.sha256(canon.encode()).digest()
project_id = digest[:8].hex()

instances = status.get("instances") or []

def matches(inst_id: str, svc: str) -> bool:
    if inst_id == svc:
        return True
    name = inst_id.split("@", 1)[0]
    return name == svc or svc.startswith(name + "@")

chosen = next((i for i in instances if matches(i.get("id") or "", service)), None)
if chosen is None:
    print(f"{service} is not a registered shared instance", file=sys.stderr)
    sys.exit(1)

attachments = chosen.get("attachments") or []
att = next((a for a in attachments if a.get("projectId") == project_id or a.get("project_id") == project_id), None)
if att is None:
    print(f"this project has not attached {chosen.get('id')} — start the service first", file=sys.stderr)
    sys.exit(1)
if att.get("provisioned") is not True:
    print(f"{chosen.get('id')} is attached but not yet provisioned", file=sys.stderr)
    sys.exit(1)

out = {"service": chosen.get("id"), "projectId": project_id, "connection": att.get("connection")}
if want_json:
    print(json.dumps(out, indent=2))
else:
    conn = out.get("connection") or {}
    if isinstance(conn, dict):
        if conn.get("url"):
            print(conn["url"])
        for k, v in (conn.get("env") or {}).items():
            print(f"{k}={v}")
        if not conn.get("url") and not conn.get("env"):
            print(json.dumps(out, indent=2))
    else:
        print(conn)
PY
