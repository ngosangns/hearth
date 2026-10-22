#!/usr/bin/env bash
# Print this project's provisioned connection for an attached shared service.
# Secrets are masked unless --reveal. Never pass --reveal unless the user asked.
set -euo pipefail
if [[ $# -lt 1 ]]; then
  echo "usage: shared-connection.sh <service|name@version> [--json] [--reveal]" >&2
  exit 2
fi
SERVICE="$1"; shift || true
WANT_JSON=0
REVEAL=0
for a in "$@"; do
  case "$a" in
    --json) WANT_JSON=1 ;;
    --reveal) REVEAL=1 ;;
    *) echo "usage: shared-connection.sh <service|name@version> [--json] [--reveal]" >&2; exit 2 ;;
  esac
done

HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="${HEARTH_ROOT:-}"
if [[ -z "$ROOT" ]]; then
  dir="$(pwd)"
  while true; do
    if [[ -f "$dir/hearth.yaml" || -f "$dir/hearth.yml" || -f "$dir/hearth.json" ]]; then
      ROOT="$dir"; break
    fi
    parent="$(dirname "$dir")"
    if [[ "$parent" == "$dir" ]]; then
      echo "hearth skill: no HEARTH_ROOT and no hearth.yaml/.yml/.json above $(pwd)" >&2
      exit 1
    fi
    dir="$parent"
  done
fi
export HEARTH_ROOT="$ROOT"

ARGS=(shared connection "$SERVICE")
if [[ "$WANT_JSON" == 1 ]]; then ARGS+=(--json); fi
if [[ "$REVEAL" == 1 ]]; then ARGS+=(--reveal); fi

# A current binary exits 0 for `shared connection --help`. An older one rejects
# the flag (exit 2). Only then fall back to the status lookup.
if "$HERE/hearth.sh" shared connection --help >/dev/null 2>&1; then
  exec "$HERE/hearth.sh" "${ARGS[@]}"
fi

# Older binaries have no `shared connection` and no `--mask`. Their
# `shared status --json` prints real values; mask the connection tree below
# the same way redact.Values does (secret-rule keys, URL userinfo, argv).
STATUS_JSON="$("$HERE/hearth.sh" shared status --json)"
python3 - "$SERVICE" "$WANT_JSON" "$REVEAL" "$ROOT" "$STATUS_JSON" <<'PY'
import hashlib, json, os, sys

service, want_json, reveal, root, status_raw = (
    sys.argv[1], sys.argv[2] == "1", sys.argv[3] == "1", sys.argv[4], sys.argv[5]
)
MASK = "••••••"
FRAGMENTS = ("SECRET", "PASSWORD", "PASSWD", "TOKEN", "API_KEY", "ACCESS_KEY", "PRIVATE_KEY")

def fail(message, code=3):
    if want_json:
        name = "unavailable" if code == 3 else "failed"
        print(json.dumps({"error": {"code": name, "exit": code, "message": message}}))
    else:
        print(message, file=sys.stderr)
    sys.exit(code)

def is_secret(name):
    upper = name.upper().replace("-", "_")
    return any(fragment in upper for fragment in FRAGMENTS)

def valid_scheme(scheme):
    if scheme == "":
        return False
    for i, ch in enumerate(scheme):
        if ch.isalpha():
            continue
        if i > 0 and (ch.isdigit() or ch in "+-."):
            continue
        return False
    return True

def mask_url(value):
    if not isinstance(value, str) or "://" not in value:
        return value
    scheme, rest = value.split("://", 1)
    if not valid_scheme(scheme) or "@" not in rest:
        return value
    userinfo, tail = rest.split("@", 1)
    if ":" not in userinfo:
        return value
    user, password = userinfo.split(":", 1)
    if password == "":
        return value
    return f"{scheme}://{user}:{MASK}@{tail}"

def is_assignment_key(key):
    body = key
    if key.startswith("--"):
        body = key[2:]
    elif key.startswith("-"):
        body = key[1:]
    if body == "":
        return False
    if not (body[0].isalpha() or body[0] == "_"):
        return False
    return all(ch.isalnum() or ch in "_-" for ch in body)

def mask_assignment(text):
    if "=" not in text:
        return None
    key, value = text.split("=", 1)
    if not is_assignment_key(key):
        return None
    if is_secret(key):
        return f"{key}={MASK}"
    return f"{key}={mask_url(value)}"

def is_secret_flag(arg):
    if "=" in arg or not arg.startswith("-"):
        return False
    name = arg.lstrip("-")
    return name != "" and is_secret(name)

def mask_list(items):
    out = []
    i = 0
    while i < len(items):
        item = items[i]
        if isinstance(item, str):
            assigned = mask_assignment(item)
            if assigned is not None:
                out.append(assigned)
                i += 1
                continue
            if is_secret_flag(item) and i + 1 < len(items) and isinstance(items[i + 1], str) and not items[i + 1].startswith("-"):
                out.append(item)
                out.append(MASK)
                i += 2
                continue
            out.append(mask_url(item))
            i += 1
            continue
        out.append(mask_tree(item))
        i += 1
    return out

def mask_tree(value, key=None):
    if isinstance(value, str):
        if key is not None and is_secret(key):
            return MASK
        return mask_url(value)
    if isinstance(value, list):
        return mask_list(value)
    if isinstance(value, dict):
        return {item_key: mask_tree(item, item_key) for item_key, item in value.items()}
    return value

try:
    status = json.loads(status_raw)
except json.JSONDecodeError:
    fail("shared status did not return JSON", 1)

canon = os.path.realpath(root)
project_id = hashlib.sha256(canon.encode()).digest()[:8].hex()
instances = status.get("instances") or []

def matches(inst_id, svc):
    if inst_id == svc:
        return True
    name = inst_id.split("@", 1)[0]
    return name == svc or svc.startswith(name + "@")

chosen = next((item for item in instances if matches(item.get("id") or "", service)), None)
if chosen is None:
    fail(f"{service} is not a registered shared instance")

attachments = chosen.get("attachments") or []
att = next((item for item in attachments if item.get("projectId") == project_id or item.get("project_id") == project_id), None)
if att is None:
    fail(f"this project has not attached {chosen.get('id')} — start the service first")
if att.get("provisioned") is not True:
    fail(f"{chosen.get('id')} is attached but not yet provisioned")

connection = att.get("connection")
if not reveal:
    connection = mask_tree(connection)
out = {"service": chosen.get("id"), "projectId": project_id, "connection": connection}
if want_json:
    print(json.dumps(out))
else:
    conn = out.get("connection") or {}
    if isinstance(conn, dict):
        if conn.get("url"):
            print(conn["url"])
        for key, value in (conn.get("env") or {}).items():
            print(f"{key}={value}")
        if not conn.get("url") and not conn.get("env"):
            print(json.dumps(out, indent=2))
    else:
        print(conn)
PY
