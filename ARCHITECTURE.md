# How this app is built

Hearth follows the same construction as [Zed](https://github.com/zed-industries/zed): a composition root wires a window shell to a headless core, and every other binary is a client of that core. The languages differ on purpose. Zed rejected a web view because a text editor cannot spend its frame budget on one. Hearth is a process supervisor, not an editor. Its core stays the Go binary. Its window stays SwiftUI. GPUI is not a dependency: it is GPL-3.0 and it exists to draw an editor at 120 fps.

The names below are Zed's crate names, so a reader can move between the two trees.

## Dependency direction

Arrows point at what a layer is allowed to import. Nothing below the line imports SwiftUI.

```
hearth-app          crates/zed          composition root: menus, actions, which panel is open
   │  WindowShell   crates/workspace    banner, docks, center pane, status bar, toast, modal
   │  Design        crates/ui + theme   tokens and components, no service rules
   ▼
HearthKit          crates/client        HTTP + SSE, workspace list, service-board rules
   │
   ▼
hearth (Go)        crates/project       the only process that starts or stops a service
```

`hearth-app` may import `HearthKit` and `NSShell`. `HearthKit` imports neither SwiftUI nor `hearth-app`. The Go module does not import the app. Feature views do not call `supervisor` or spawn processes; they call `AppModel`, and `AppModel` calls `HearthKit`.

`NSShell` is the private package [ns-shell](https://github.com/ngosangns/ns-shell), checked out beside this repo. It holds the tokens, components, toast, confirmation, and window layers shared with synca and promptscope. This repo keeps hearth's commands, service board, and daemon client.

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

One binary hosts the daemon, the CLI, and MCP, the way one Zed binary hosts the window and a separate `cli` binary only finds that window and speaks IPC. Hearth inverts the thin side: the long-lived process is the daemon, and the window is the client. A supervisor cannot live inside a window that the user can quit.

## Window

Zed's `Workspace` is the root of one window. Hearth already uses "workspace" for a project folder, so the window type is `WindowShell` and the folder stays a workspace.

| Zed | This window |
| --- | --- |
| Left dock, `ProjectPanel` | `SidebarView` — workspace list and shared-services list |
| Center `Pane` of items | `DetailView` — the open workspace or the shared board |
| Right / bottom dock | `LogView`, shown only while `logOpen` |
| `StatusBar` | version, in-flight operation, ready or working |
| Banner | `CLIMissingBanner` when `hearth` is not on `PATH` |
| `toast_layer` | `Toast` over the shell |
| `modal_layer` | `confirmationDialog`. Destructive work does not run without it. |

`App` (`HearthApp`) is the composition root. It owns `AppModel` the way GPUI's `App` owns every entity: one owner, views only read it. Work that talks to the daemon runs in a `Task`, which is the foreground executor; the daemon's own goroutines are the background executor. The window never starts a service itself.

Commands are `HearthCommand` values. Menus only bind them. Adding a command means one case, one title, one shortcut, one `run`.

## Scope

In scope: one daemon per project root, one `smp` daemon per machine, a CLI and an MCP server on the same protocol, and one macOS window that is a client.

Out of scope, including things Zed has that this product does not need: a GPU UI framework, a command palette, an extension host inside the window, collaboration, a language server, multi-window workspace restore, and a second implementation of start/stop in Swift. The Rust workspace was removed on purpose; do not bring it back to resemble Zed's toolchain.

## GitHub

`.github/actions` holds the steps that more than one workflow runs (`toolchain-path`, `release-asset`). `ci.yml` is `workflow_call` so `publish.yml` can refuse to tag a red tree. That is the part of Zed's `.github` that fits a single-binary tool. Zed's issue-board and guild workflows do not.
