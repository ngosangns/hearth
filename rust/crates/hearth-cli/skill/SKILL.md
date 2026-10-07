---
name: hearth
kind: playbook
description: Operate this project's local dev services (start/stop/status/logs) through hearth CLI skill scripts instead of raw kill/pkill/docker/task. MCP retired for agents — use scripts/ or `hearth` directly.
---

# Hearth

This project's local dev services (per-container infra, docker/tailnet-adopted units, and host
processes it runs directly) are managed by a single background daemon — the Hearth daemon, the
`hearth` binary (0.18+; older binary name retired). Do not `kill`/`pkill`/`docker restart`/run a
dev task raw for a unit this daemon tracks.

**MCP is retired for coding agents.** Do not run `hearth mcp install`, and do not register a
Hearth MCP server in mcp.json. Canonical path = skill scripts below (or `hearth` CLI).

## Skill scripts

Installed under this skill's `scripts/` (or call `hearth` with `--root <project>`). Root resolution
for scripts: `$HEARTH_ROOT`, else walk up from cwd for `hearth.yaml` / `.yml` / `.json`. Binary:
`$HEARTH_BIN` or `~/.local/bin/hearth`.

| Former MCP tool | Script | Mutate gate |
| --- | --- | --- |
| `local_services_status` | `scripts/status.sh [service]` | free |
| `local_services_logs` | `scripts/logs.sh <service> [--tail N] [--follow]` | free |
| `local_services_trace` | `scripts/trace.sh <operationId>` | free |
| `local_services_events` | `scripts/events.sh [--after N] [--epoch E]` | free |
| `local_services_manage` | `scripts/manage.sh start\|stop\|restart <target> [--wait] [--kill-unowned]` | user asked |
| `local_services_restart_daemon` | `scripts/restart-daemon.sh` | user asked |
| `local_services_stop_daemon` | `scripts/stop-daemon.sh` | user asked |
| `local_services_shared_list` | `scripts/shared-list.sh` | free |
| `local_services_shared_status` | `scripts/shared-status.sh` | free |
| `local_services_shared_connection` | `scripts/shared-connection.sh <service>` | free |

Also free: `scripts/urls.sh [target]`, `scripts/doctor.sh`, `scripts/hearth.sh <any hearth subcommand>`.

### Mutate gate

Only call manage / restart-daemon / stop-daemon (or `hearth start|stop|restart|manager stop|manager restart`) when the user explicitly asked. Never speculate ("let me just start it to see"). `--kill-unowned` only when the user asked to kill the port holder.

### Diagnosis (always safe)

`status`, `logs`, `urls`, `doctor`, `trace`, `events`, `shared-list`, `shared-status`, `shared-connection`, `manager ensure --json`, `manager status`.

## Equivalent CLI

- `hearth status [service]` / `hearth logs <service> [--tail N] [--follow]`
- `hearth urls [service]` — live URLs (prefer these over guessing ports)
- `hearth start|stop|restart <service|group> [--wait]` — `start` also takes `--kill-unowned`
- `hearth operation get|watch <id>`
- `hearth doctor` / `hearth manager ensure|status|stop|restart|reload`
- `hearth shared list|status|…` / `hearth tui`

## Shared services (`shared:` in hearth.yaml)

Machine-global singletons via `smp`. After a shared service is `ready`, use
`scripts/shared-connection.sh <id>` for `url`/`env` and wire them into the app config (not injected
into env). Detach does not delete per-project data.

## Architecture

One daemon per project root (HMAC lock under `.hearth/runtime/`). `hearth manager ensure --json`
prints token/port/runtime directory. Services are daemon-owned or externally-owned per catalog.

This doc is generic; see the project's own docs for its service list and ports.
