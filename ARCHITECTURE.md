# How this app is built

Hearth is a process supervisor. The product in this repo is the Go binary at `go/cmd/hearth`. The macOS window is `hearth-gpui` in `apps/hearth-gpui`. It calls this CLI and is a separate Cargo workspace.

## What each Go package is

| Package | Zed analogue | Owns |
| --- | --- | --- |
| `cmd/hearth` | `crates/zed` + `crates/cli` | Process entry. Stamps `version`. Dispatches `daemon` and `smp` before `cli`. |
| `internal/manager` | `project` + the collab RPC server | Catalog, operations, routes, the loopback HTTP + SSE API. |
| `internal/supervisor` | the editor's buffer engine, for processes | Spawn, probe, stop, log tail. The only owner of a managed process. |
| `internal/client` | `crates/rpc` + `crates/client` | What every UI and script speaks. |
| `internal/cli` | `crates/cli` | Argv in, client calls out. No daemon state. Also embeds and installs the agent skill. |
| `internal/configfile` | `crates/settings` | `hearth.yaml` load. Invalid input fails the load. |
| `internal/state` | wire protocol | Typed kebab-case enums. `ProtocolVersion` is the break signal. |
| `internal/paths` | `crates/paths` | Runtime, data, and install directories. |
| `internal/updater` | `crates/auto_update` | GitHub release into the versioned bin dir. |
| `internal/shared` | a second `project` root | The machine-global `smp` daemon at `~/.hearth/shared`. |
| `internal/redact` | — | Masks secret-shaped strings anywhere in a JSON tree. |
| `internal/schema` | — | Embedded JSON Schemas for CLI output, schemaVersion 1. |

One binary hosts the daemon and the CLI. The long-lived process is the daemon. A supervisor cannot live inside a window that the user can quit.

## Scope

In scope: one daemon per project root, one `smp` daemon per machine, a CLI, and bundled skill scripts for coding agents.

Out of scope: a command palette, an extension host, collaboration, a language server, and a second implementation of start/stop. The window is `apps/hearth-gpui`. Do not add a UI target or a Rust module to the Go module.

## GitHub

`.github/actions` holds the steps that more than one workflow runs (`toolchain-path`, `release-asset`). `ci.yml` is `workflow_call` so `publish.yml` can refuse to tag a red tree. That is the part of Zed's `.github` that fits a single-binary tool. Zed's issue-board and guild workflows do not.
