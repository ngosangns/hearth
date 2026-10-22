# How this app is built

Hearth is a process supervisor. The product in this repo is the Go binary at `go/cmd/hearth`. The macOS window is `hearth-gpui` in the sibling repo `ns-gpui`. It calls this CLI. This repo has no UI.

## What each Go package is

| Package | Zed analogue | Owns |
| --- | --- | --- |
| `cmd/hearth` | `crates/zed` + `crates/cli` | Process entry. Stamps `version`. Dispatches `daemon`, `smp`, and `mcp` before `cli`, because `cli` must not import `mcpserver`. |
| `internal/manager` | `project` + the collab RPC server | Catalog, operations, routes, the loopback HTTP + SSE API. |
| `internal/supervisor` | the editor's buffer engine, for processes | Spawn, probe, stop, log tail. The only owner of a managed process. |
| `internal/client` | `crates/rpc` + `crates/client` | What every UI and script speaks. |
| `internal/cli` | `crates/cli` | Argv in, client calls out. No daemon state. |
| `internal/mcpserver` | `crates/extension_host` | A tool surface over the same client. Not compiled into the window. |
| `internal/configfile` | `crates/settings` | `hearth.yaml` load. Invalid input fails the load. |
| `internal/state` | wire protocol | Typed kebab-case enums. `ProtocolVersion` is the break signal. |
| `internal/paths` | `crates/paths` | Runtime, data, and install directories. |
| `internal/updater` | `crates/auto_update` | GitHub release into the versioned bin dir. |
| `internal/shared` | a second `project` root | The machine-global `smp` daemon at `~/.hearth/shared`. |
| `internal/redact` | — | Masks secret-shaped strings anywhere in a JSON tree. |
| `internal/schema` | — | Embedded JSON Schemas for CLI output, schemaVersion 1. |

One binary hosts the daemon, the CLI, and MCP. The long-lived process is the daemon. A supervisor cannot live inside a window that the user can quit.

## Scope

In scope: one daemon per project root, one `smp` daemon per machine, and a CLI and an MCP server on the same protocol.

Out of scope: a UI in this repo, a command palette, an extension host, collaboration, a language server, and a second implementation of start/stop. The window is `hearth-gpui` in `ns-gpui`. Do not add a UI target or a Rust module here.

## GitHub

`.github/actions` holds the steps that more than one workflow runs (`toolchain-path`, `release-asset`). `ci.yml` is `workflow_call` so `publish.yml` can refuse to tag a red tree. That is the part of Zed's `.github` that fits a single-binary tool. Zed's issue-board and guild workflows do not.
