---
name: local-services
description: Operate this project's local dev services (start/stop/status/logs) through the local-services daemon instead of raw kill/pkill/docker/task dev.
---

# Local services

This project's local dev services (per-container infra, docker/tailnet-adopted units, and host
processes it runs directly) are managed by a single background daemon — the
`@gnasdev/local-services` engine (compiled binary `lsd`). Do not `kill`/`pkill`/`docker
restart`/run a dev task raw for a unit this daemon tracks: its state would drift from the real
process, and every CLI/TUI/MCP surface reading that state would start reporting the wrong thing.

## MCP tools

This project registers an MCP server exposing 5 tools (prefixed `local_services_` unless this
install customized `--name`, or the server's own `tool_prefix` differs):

| Tool | Does | Mutate gate |
| --- | --- | --- |
| `local_services_status` | Current state of one service or all of them | free |
| `local_services_logs` | Tail (or follow) one service's log | free |
| `local_services_trace` | Look up one operation by id | free |
| `local_services_events` | Recent manager/service lifecycle events | free |
| `local_services_manage` | start / stop / restart a service or group | requires `confirm=true` |

`status`/`logs`/`trace`/`events` are always safe to call for diagnosis — use them freely. Only
call `manage` when the user has explicitly asked for that lifecycle action; never call it
speculatively ("let me just start it to see").

## Equivalent CLI

The same daemon backs a non-interactive CLI and an interactive TUI — all three (MCP, CLI, TUI)
talk to the same daemon over the same loopback HTTP+SSE API, so state seen through one is state
seen through all:

- `lsd status [service]` / `lsd logs <service> [--tail N] [-f]`
- `lsd start|stop|restart <service|group> [--wait]`
- `lsd doctor` / `lsd manager ensure|status|stop|reload`
- `lsd tui`

## Architecture, in one paragraph

One daemon per project root (HMAC-signed lock file under `.local-services/runtime/`), reached by
any client via `lsd manager ensure --json`, which prints the daemon's token/port/runtime
directory so a client never needs to reimplement lock-file discovery. A service is either
daemon-owned (started/stopped only through this daemon) or externally-owned (e.g. a `docker
compose`/`tailscale serve` unit whose lifecycle is managed elsewhere and merely observed/adopted
by the daemon) — this project's own catalog decides which is which per service.

This doc is generic to any project using `@gnasdev/local-services`; see this project's own docs
for its specific service list, ports, and any additional operational rules.
