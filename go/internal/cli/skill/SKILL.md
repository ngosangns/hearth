---
name: hearth
kind: playbook
description: Operate this project's local dev services (start/stop/status/logs) through hearth CLI skill scripts instead of raw kill/pkill/docker/task. MCP retired for agents — use scripts/ or `hearth` directly.
---

# Hearth

This project's local dev services are managed by one background daemon, the `hearth` binary.
Do not `kill`, `pkill`, `docker restart`, or run a dev task raw for a unit this daemon tracks.

**MCP is retired for coding agents.** Do not run `hearth mcp install`. Use the scripts below, or `hearth` directly.

Refresh this skill with `hearth skill install` (default `~/.agents/skills/hearth`) and confirm with `hearth skill check`. Do not hand-edit the installed copy.

## Scripts

Root: `$HEARTH_ROOT`, else walk up from cwd for `hearth.yaml` / `.yml` / `.json`.
Binary: `$HEARTH_BIN` or `~/.local/bin/hearth`.

| Script | Mutate gate |
| --- | --- |
| `scripts/status.sh [service]` | free |
| `scripts/logs.sh <service> [--tail N] [--follow]` | free |
| `scripts/trace.sh <operationId>` | free |
| `scripts/events.sh [--after N] [--epoch E]` | free |
| `scripts/urls.sh [target]` | free |
| `scripts/doctor.sh` | free |
| `scripts/manage.sh start\|stop\|restart <target> [--wait] [--dry-run] [--kill-unowned] [--yes]` | user asked |
| `scripts/restart-daemon.sh` | user asked |
| `scripts/stop-daemon.sh` | user asked |
| `scripts/shared-list.sh [--json] [--reveal]` | free |
| `scripts/shared-status.sh [--json] [--reveal]` | free |
| `scripts/shared-connection.sh <service> [--json] [--reveal]` | free |
| `scripts/hearth.sh <any hearth subcommand>` | see below |

### Mutate rule

Run a command that changes a process or a file only when the user asked for that action in the current task. That includes `start`, `stop`, `restart`, `manager ensure|stop|restart|reload`, `shared ensure|attach|detach|install|start|stop|remove`, `cleanup`, `update` without `--check`, `mcp install`, and `skill install`. Never start something to see if it works.

`manager ensure` and `shared ensure` can start a daemon. Prefer `manager status` and `shared status` to look.

`--dry-run` on `start|stop|restart` prints the services that would change and does not contact the daemon. `manage.sh` does not add `--yes`. Pass `--yes` only after the user confirmed. `--kill-unowned` only when the user asked to kill the process holding the port; off a TTY it also needs `--yes`.

`manager stop`, `shared remove`, and `shared detach --stop-if-unused` need `--yes` off a TTY. `stop-daemon.sh` passes `--yes` because running that script is the confirmation. Ordinary service stop does not need `--yes`.

### `--reveal`

`shared list`, `shared status`, `shared installed`, `shared connection`, and `shared attach` mask secrets unless `--reveal`. `--mask` on `shared list`, `shared status`, and `shared installed` changes nothing. `--mask` with `--reveal` exits 2.

`manager ensure`, `manager restart`, and `shared ensure` mask the daemon bearer token unless `--reveal`. `--mask` on those commands changes nothing. `--mask` with `--reveal` exits 2. A client that calls the daemon over HTTP+SSE must pass `--reveal`. `scripts/events.sh` passes `--reveal` because it calls `127.0.0.1` itself.

`scripts/shared-list.sh` and `scripts/shared-status.sh` are masked by default and forward `--reveal` only when the caller passed it. `scripts/shared-connection.sh` stays masked the same way. Never pass `--reveal`, including through `hearth.sh`, unless the user asked for the real value or the script is calling the daemon's HTTP API.

## Recipes

**Why is service X down?** `scripts/status.sh X`, then `scripts/logs.sh X --tail 80`. If the row is `externally-owned`, another process holds the port. Do not kill it unless the user asked (`manage.sh start X --kill-unowned --yes`).

**Restart a group and wait.** `scripts/manage.sh restart <group> --wait`. Then `scripts/status.sh <group>`. Use `--dry-run` first when membership is unclear.

**Find URLs.** `scripts/urls.sh` or `scripts/urls.sh <service>`. A finished one-shot can still have a URL.

**Shared recipes and connection, masked.** `scripts/shared-list.sh` is the remote catalog. `scripts/shared-connection.sh <service>` is this project's connection. `scripts/shared-status.sh` is the instance list. Add `--reveal` only when the user asked for the password or URL.

**List workspaces.** `hearth workspaces list --json`. Read-only. A missing file is an empty list.

## Failures

With `--json`, a failure prints `{"error":{code,exit,message,hint?}}` on stdout. These stay their own document: `update --json` (its report), `skill check` drift (exit 8), `doctor --json` when checks fail (exit 5), and silent `shared probe` (exit 0 or 1).

| Exit | Code | Action |
| --- | --- | --- |
| 2 | usage | Fix the arguments. Read `hearth <command> --help`. |
| 3 | unavailable | Daemon or smp is down. Start one only if the user asked. |
| 4 | protocol | Binary and daemon disagree. Tell the user. Do not restart unless they asked. |
| 5 | failed | Read logs or `hearth operation get <id>`. |
| 6 | needs-confirm | Re-run with `--yes` only after the user confirmed. |
| 7 | unauthorized | The token does not match. Do not delete state. Tell the user. |
| 8 | conflict | `skill check` found drift. Reinstall only if the user asked. |

## Verify

1. `scripts/status.sh` shows the service you changed.
2. After a start or restart, the row is `ready`, `running`, or `succeeded`, or the logs say why not.
3. `scripts/urls.sh` matches what you told the user.
4. Shared values in the reply are masked unless the user asked to see them.
5. After a skill install, `hearth skill check` is in sync.
