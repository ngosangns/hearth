# Local Services (macOS app)

A native SwiftUI front door for `@gnasdev/local-services`, alongside the CLI/TUI/MCP clients in
`../../src`: add a folder that has a `local-services.yaml` (or `.config.ts`), and manage its
daemon-owned services from a menu-bar-friendly window instead of a terminal.

Architecture is unchanged from the rest of the package — see the root [README](../../README.md):
this app is just another HTTP+SSE-ish client of a per-folder daemon, nothing more. It never talks to
a managed process directly.

```
 WorkspaceStore (~/Library/Application Support/LocalServicesApp/workspaces.json)
        │ one folder path per workspace
        ▼
 DaemonConnection.ensure(root:)  ──spawns──►  bun run <repo>/src/bin/lsd.ts --root <folder> manager ensure --json
        │ {instanceId, port, token, protocolVersion, runtimeDirectory, root}
        ▼
 ManagerClient  ──HTTP (Bearer token)──►  the folder's own daemon (LocalServicesManager)
```

## Requirements to build/run

- Xcode 15+ / Swift 5.10 toolchain (`swift build` works standalone, no `.xcodeproj` needed for dev).
- [Bun](https://bun.sh) installed (`/opt/homebrew/bin/bun`, `/usr/local/bin/bun`, or `~/.bun/bin/bun`)
  — same requirement as the rest of this package. The app shells out to it; it does not bundle one.
```bash
cd apps/macos
swift build     # or: swift run — a raw dev binary, resolves lsd.ts from this checkout (see below)
```

## Packaging a local `.app`

```bash
apps/macos/scripts/build-app.sh          # release build (pass `debug` for a debug one)
open "apps/macos/.build/Local Services.app"
```

Produces an ad-hoc-signed `Local Services.app` with a copy of `src/` bundled as a resource
(`Contents/Resources/lsd/src`) — `SidecarLocator.findLsdEntry()` prefers that bundled copy over the
dev-checkout fallback, so the packaged app works even if this repo checkout later moves, and can in
principle be copied elsewhere on the *same* machine (still needs `bun` installed there). **This is
local packaging, not distribution**: ad-hoc signing (no Developer ID) means Gatekeeper still treats it
as untrusted on any *other* machine, and even locally a first Finder launch may need a right-click >
Open. Set `LSD_DEBUG=1` in the environment to log which `bun`/`lsd.ts` paths it resolved to stderr —
the first thing to check for a "manager unavailable" report, and how the bundled-resource path was
confirmed to actually get picked (see `SidecarLocator.swift`).

## What's implemented (v0)

- Add/remove workspaces (folders), persisted locally.
- Per-folder trust prompt before the first daemon connection (a `local-services.yaml` names arbitrary
  commands the daemon will run).
- Connect (spawn-or-adopt the daemon via `lsd manager ensure`), list services with live status
  (polling `/v1/services` every 2s — see `ManagerClient.swift`'s doc comment for why polling and not
  SSE in this first pass), start/stop/restart per service.
- Per-service log viewer (a sheet, polling `/v1/logs/:serviceId`, following the same cursor/generation
  protocol the CLI's `logs --follow` uses).
- A workspace's live connection lives in `WorkspaceControllerRegistry` at the app level, not per-view
  — every *trusted* workspace auto-connects on launch (and stays connected across sidebar navigation),
  which is also what makes the menu bar summary meaningful even when the main window isn't showing
  that workspace.
- `MenuBarExtra` — a global "ready/total" (and failure count) summary across every trusted workspace,
  with a dropdown listing each one's status and a way to bring the main window forward.
- FSEvents-ish config watch (`ConfigFileWatcher`, a `DispatchSource` on the workspace root — see its
  doc comment for why directory-level rather than the exact filename): editing
  `local-services.yaml`/`.yml`/`.json`/`.config.ts` triggers `lsd manager reload` automatically,
  debounced.
- "Start All" / "Stop All" — in the main window's toolbar and in each menu bar dropdown row. Start
  goes through `/v1/operations/bulk-start` (dependency-ordered, stops on first failure, same as
  `lsd start <group> --wait`); there's no bulk-stop endpoint on the daemon (see AGENTS.md), so Stop All
  is a client-side concurrent loop over individual stops, the same way the TUI's `s` key works.

## Known limitations / next steps

- **No *distributable* `.app`** — local packaging exists (`scripts/build-app.sh`, see above), but it's
  ad-hoc signed (no Developer ID), so it only really works on the machine that built it. A `bun build
  --compile` sidecar was tried instead of bundling `src/` as a resource and rejected — see
  `SidecarLocator.swift`'s doc comment for why. Real distribution needs a Developer ID cert +
  notarization that only the project owner has — **not something that can be finished by grinding
  through more code.**
- **Polling, not SSE**, for service status — `/v1/events/stream` would lower latency and cut request
  volume; not done yet, see `ManagerClient.swift`.
- **Config-reload failures surface as a dismissible banner, not a diff/preview** — an edit that breaks
  the catalog (e.g. a bad readiness kind) shows the daemon's validation error in
  `WorkspaceController.lastActionError`, but there's no "here's what changed" view.

## CI

`.github/workflows/macos-app.yml` runs `swift build` on pull requests / pushes that touch
`apps/macos/**`, path-filtered so it never runs for a change that's only in `src/`/`test/`, and
separate from the package's own Bun `test`/`typecheck` job in `.github/workflows/ci.yml`.

`Tests/LocalServicesAppTests` (`swift test`) — decode-fidelity tests against real captured JSON from a
live daemon, plus a few pure-logic checks — is **not** wired into CI yet: the registered self-hosted
runner's toolchain doesn't have XCTest (Command Line Tools only, no full Xcode.app install), unlike a
normal dev machine. Run it locally with `swift test`; see `macos-app.yml`'s own comment.
