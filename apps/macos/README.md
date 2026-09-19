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
- **This app currently only runs built in place inside this repo.** `SidecarLocator.findLsdEntry()`
  resolves `../../src/bin/lsd.ts` from its own source file's on-disk path at compile time — there is
  no packaged/distributable build yet (see "Known limitations" below).

```bash
cd apps/macos
swift build     # or: swift run
```

## What's implemented (v0)

- Add/remove workspaces (folders), persisted locally.
- Per-folder trust prompt before the first daemon connection (a `local-services.yaml` names arbitrary
  commands the daemon will run).
- Connect (spawn-or-adopt the daemon via `lsd manager ensure`), list services with live status
  (polling `/v1/services` every 2s — see `ManagerClient.swift`'s doc comment for why polling and not
  SSE in this first pass), start/stop/restart per service.

## Known limitations / next steps

- **No packaged `.app` for distribution.** Needs: bundling `lsd.ts` + its `src/` imports as an app
  resource (or a *signed* `bun build --compile` sidecar — an *unsigned* one was tried and rejected,
  see `SidecarLocator.swift`'s doc comment for why), a proper Info.plist/bundle ID, and Developer ID
  signing + notarization for Gatekeeper.
- **Polling, not SSE**, for service status — `/v1/events/stream` would lower latency and cut request
  volume; not done yet, see `ManagerClient.swift`.
- **No log viewer** — `/v1/logs/:serviceId` is unused so far.
- **No MenuBarExtra** — this is a normal window app for now; a menu-bar summary (todo per the
  original plan) is unimplemented.
- **No FSEvents watch on `local-services.yaml`** — editing a workspace's config file doesn't trigger
  `POST /v1/manager/reload` yet; the daemon-side reload endpoint exists (`src/core/manager.ts`), this
  app just doesn't call it.

## CI

`.github/workflows/ci.yml` builds this target (`swift build`) on pull requests / pushes that touch
`apps/macos/**`, alongside (not instead of) the existing Bun `test`/`typecheck` job for the package
itself.
