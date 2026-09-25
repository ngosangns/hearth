---
name: hearth
kind: playbook
description: Operate this project's local dev services (start/stop/status/logs) through the hearth daemon instead of raw kill/pkill/docker/task dev.
---

# Hearth

This project's local dev services (per-container infra, docker/tailnet-adopted units, and host
processes it runs directly) are managed by a single background daemon — the
`@gnasdev/hearth` engine (compiled binary `hearthd`). Do not `kill`/`pkill`/`docker
restart`/run a dev task raw for a unit this daemon tracks: its state would drift from the real
process, and every CLI/TUI/MCP surface reading that state would start reporting the wrong thing.

## MCP tools

This project registers an MCP server exposing 10 tools (prefixed `local_services_` unless this
install customized `--name`, or the server's own `tool_prefix` differs):

| Tool | Does | Mutate gate |
| --- | --- | --- |
| `local_services_status` | Current state of one service or all of them, plus where each can be reached (`urls`) | free |
| `local_services_logs` | Tail (or follow) one service's log | free |
| `local_services_trace` | Look up one operation by id | free |
| `local_services_events` | Recent manager/service lifecycle events | free |
| `local_services_manage` | start / stop / restart a service or group | requires `confirm=true` |
| `local_services_restart_daemon` | Restart this project's daemon, leaving running services up for the new daemon to re-adopt | requires `confirm=true` |
| `local_services_stop_daemon` | Stop this project's daemon AND every service it manages | requires `confirm=true` |
| `local_services_shared_list` | Services/versions installable from the shared-services registry | free |
| `local_services_shared_status` | Shared instances on this machine: ports, install state, attachments | free |
| `local_services_shared_connection` | This project's connection info (url/env) for an attached shared service | free |

## Shared services (`shared:` in hearth.yaml)

A `shared:` block registers machine-global singletons managed by a separate daemon (`smp`), shared
across every repo that registers the same `name@version`. The shipped catalog is `redis`, `mongodb`,
`minio`, `nginx`, and `kafka` (exact versions live in the repo `catalog.json`). They show up as
ordinary `infrastructure` services — start them with `manage` like any other service; the first
start installs the service into `~/.hearth/shared` and may take a while. **Connection info is not
injected into env**: after the service is `ready`, call `local_services_shared_connection` with the
service id (`redis`) or instance id (`redis@8.2.10`) and wire the returned `connection` (`url`/`env`)
into the app's own config. Mongo, nginx, and Kafka provision a per-project resource named
`h_<projectId>` (database, path prefix, topic). MinIO's bucket is `h-<projectId>` because S3 names
reject underscores. Redis is one shared DB 0. Detach does not delete that data. Do not hand-edit
shared data for another project.

`status`/`logs`/`trace`/`events` are always safe to call for diagnosis — use them freely. Only
call `manage` when the user has explicitly asked for that lifecycle action; never call it
speculatively ("let me just start it to see"). `restart_daemon` is for a daemon that is wedged or
running an older binary — it is not a way to restart a *service* (that is `manage` with
`action: restart`), and it briefly makes every other tool call fail while the daemon is down.

`stop_daemon` is the full shutdown: unlike `restart_daemon` it does **not** leave services
running — the daemon stops every service it manages first, then exits. Only call it when the user
has explicitly asked to stop the daemon (or the whole project). Afterwards **every** tool call —
including `status` — fails until some other client (`hearthd`, the app, the TUI) starts a new
daemon; there is no `start_daemon` tool.

`stop` on a service this daemon does not own a process for (an adopted `ownership: external` unit,
or one whose port is held by an unowned process) runs the catalog's `stop:` command; when the
catalog declares none, the operation **fails** with the reason rather than reporting success. A
failed stop means the service is still running — do not tell the user it stopped.

When `manage`/`start` fails with `externally-owned` ("Port N is held by pid … (cmd)"), a process
this daemon does not own holds the service's port. `manage` accepts `killUnowned: true`
(`action: start` only, still gated on `confirm: true`) to terminate that process and continue the
start — only set it when the user has explicitly asked to kill the holder; otherwise leave the
service `externally-owned` and report the holder to the user.

## Equivalent CLI

The same daemon backs a non-interactive CLI and an interactive TUI — all three (MCP, CLI, TUI)
talk to the same daemon over the same loopback HTTP+SSE API, so state seen through one is state
seen through all:

- `hearthd status [service]` / `hearthd logs <service> [--tail N] [-f]`
- `hearthd urls [service]` — the live URLs a service is reachable at (use these rather than guessing ports)
- `hearthd start|stop|restart <service|group> [--wait]` — `start` also takes `--kill-unowned`
  (kill the process holding the service's port, then start; on a TTY `start` prompts instead)
- `hearthd doctor` / `hearthd manager ensure|status|stop|restart|reload` (`restart` replaces the
  daemon process and leaves its services running for the new one to re-adopt; `stop` stops all
  managed services and then the daemon)
- `hearthd tui`

## Architecture, in one paragraph

One daemon per project root (HMAC-signed lock file under `.hearth/runtime/`), reached by
any client via `hearthd manager ensure --json`, which prints the daemon's token/port/runtime
directory so a client never needs to reimplement lock-file discovery. A service is either
daemon-owned (started/stopped only through this daemon) or externally-owned (e.g. a `docker
compose`/`tailscale serve` unit whose lifecycle is managed elsewhere and merely observed/adopted
by the daemon) — this project's own catalog decides which is which per service.

This doc is generic to any project using `@gnasdev/hearth`; see this project's own docs
for its specific service list, ports, and any additional operational rules.
