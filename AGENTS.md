# Project agent memory

This file is the project's committed home for project-intrinsic agent knowledge: build, test, release, architecture, and sharp-edge notes that should travel with the code.

- Add durable project-specific notes here as they are discovered through real work.

## Architecture

One package, five subpath exports (`/core`, `/cli`, `/tui`, `/mcp`, `/node-bridge`) — see README.md
for the full design and a minimal consumer example. No default `ServiceCatalog` ships anywhere;
every entry point takes a caller-supplied catalog as a parameter. Ported and generalized from two
independently-forked internal copies of this tool (infra's `scripts/local-services-tui`, viclass's
`tools/local-services-tui`) per `/Users/ngosangns/Github/firstmate/data/smp-npm-plan/report.md`.

## Build, test, release

- Bun-only, no build step: `src/**/index.ts` is published as TypeScript source and resolved natively
  by Bun (see `exports` in package.json). Do not add a bundler/tsc-emit step without also revisiting
  that decision.
- `bun test` / `bun run typecheck` (tsc --noEmit, strict + `noUncheckedIndexedAccess` +
  `verbatimModuleSyntax`). CI (`.github/workflows/ci.yml`) runs both on PRs on `{self-hosted, macmini}`
  and publishes to GitHub Packages only on a `vX.Y.Z` tag matching `package.json`'s version.
- Semver doubles as the protocol-compatibility signal: `PROTOCOL_VERSION` (src/core/state.ts) is the
  one place a daemon and its TUI/CLI/MCP clients read it from — a bump there must be a major release.

## Sharp edges

- `src/core/supervisor.ts`'s `normalizeCommandFingerprint` must hash the *logical* command text
  (argv joined, or the bare `shell` string) — never the physical `sh -c` spawn wrapper — so it matches
  `normalizeObservedCommandFingerprint`'s prefix-stripped `ps` output. Already fixed once during the
  initial port; a future refactor of `commandArgv` should re-check this pairing.
- Stopping a service signals its **whole process tree**, snapshotted from `ps` before the first
  signal (`ProcessSupervisor.processTree`): `air` runs the built server in its **own** process group,
  so signalling only the tracked pgid leaves the real server alive and holding its port, and the next
  start fails with `Port N is held by an unowned process`. The snapshot is only walked when the OS
  table still shows the recorded `startIdentity` for the leader pid, so a stale/reused pid can never
  pull an unrelated live tree into a signal or into the wait-for-death loop.
- An adopted identity (`startLocked`'s `retainedIdentity`) is kept only while it still answers its
  readiness probe; an alive-but-unresponsive one (air survives its child) is terminated and replaced
  instead of being re-adopted on every start into a permanent `Readiness timed out`.
- Fire-and-forget work (unit log forwarding, `syncExternalServices` polling) must never reject into
  the daemon: Bun terminates the process on an unhandled rejection, orphaning every managed service
  (its identity keeps the dead daemon's instance id, so the next daemon can only adopt it). Guarded
  sinks report through the optional `Host.recordBackgroundError`, and `runDaemon` logs stray
  rejections/exceptions to `<runtimeDir>/daemon.log` (one rotated copy) instead of dying.
- A daemon whose lock was taken over stops itself (`LockOwnershipWatch` in `daemon.ts`) — it exits
  without touching the winner's lock. Enforced from the losing side so two daemons can never fight
  over one `state.json`.
- `ProcessSupervisor.shutdown()` does two passes: an "active state" stop pass, then a second reap pass
  for daemon-owned services holding a stale POSIX identity in a non-"active" state (e.g.
  `externally-owned` after a port conflict). Don't collapse these back into one pass.
- `ownership: 'external'` services are this package's own generalization (no equivalent in either
  source repo) of infra's docker/tailnet-task external-adoption carve-out — see
  `test/core/external-ownership.test.ts` for the only coverage of `syncExternalServices()`.
- Phase 2 (switching infra's `scripts/local-services-tui` onto this package) and Phase 4 (porting
  viclass) are out of scope for this repo — separate follow-up work in those repos, per the plan
  report's phased migration (§4).

## Maintaining this file

Keep this file for knowledge useful to almost every future agent session in this project.
Do not repeat what the codebase already shows; point to the authoritative file or command instead.
Prefer rewriting or pruning existing entries over appending new ones.
When updating this file, preserve this bar for all agents and keep entries concise.
