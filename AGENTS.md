# Project agent memory

This file is the project's committed home for project-intrinsic agent knowledge: build, test, release, architecture, and sharp-edge notes that should travel with the code.

- Add durable project-specific notes here as they are discovered through real work.

## Architecture

One package, five subpath exports (`/core`, `/cli`, `/tui`, `/mcp`, `/node-bridge`) plus a `lsd` bin —
see README.md for the full design and a minimal consumer example. No default `ServiceCatalog` ships
anywhere; every entry point takes a caller-supplied catalog as a parameter. Ported and generalized
from two independently-forked internal copies of this tool (infra's `scripts/local-services-tui`,
viclass's `tools/local-services-tui`) per `/Users/ngosangns/Github/firstmate/data/smp-npm-plan/report.md`.

A project can skip authoring its own `catalog.ts`/`daemon.ts`/`cli.ts` entirely: `src/core/config-file.ts`
maps a `local-services.yaml`/`.yml`/`.json` (or a `.config.ts` escape hatch) onto `ServiceCatalog`, and
the `lsd` bin (`src/bin/lsd.ts`, README § "Declarative config") wraps that loader around `main()`/`runDaemon`
the same way a hand-written `cli.ts`/`daemon.ts` would. This is also the on-ramp for a non-Bun/non-TS
client (e.g. a desktop app) — `lsd manager ensure --json` prints everything (`token`, `port`,
`runtimeDirectory`, ...) a generic HTTP+SSE client needs, without it reimplementing lock-file discovery.
`src/core/env.ts` resolves the daemon's own base environment (login shell + `.env` file) for exactly
this case: a daemon launched from a GUI has none of the `PATH` customization a terminal-launched one
inherits for free from `process.env` — plumbed into `defaultSupervisorOptions`'s optional third
(`baseEnvironment`) argument, `process.env` if omitted, so every existing terminal-launched consumer
is unaffected.

`apps/macos` is that desktop-app on-ramp actually being built: a SwiftUI client that spawns
`bun run src/bin/lsd.ts ... manager ensure --json` per workspace folder (see its own README for
architecture/status). It only builds in place inside this checkout — no packaged/distributable build
yet. Its `.github/workflows/macos-app.yml` (path-filtered to `apps/macos/**`) runs `swift build`
separately from the package's own `ci.yml`.

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
- `LocalServicesManager.reloadCatalog` (`POST /v1/manager/reload`) stops a removed-but-active service
  using the *old* catalog, and only swaps `this.catalog` to the new one afterward — `ProcessSupervisor`
  needs the old definition to know how to stop it, and swapping first would make that service briefly
  vanish from `serviceStates()`/`/v1/services` while its process was still alive. It's serialized
  against itself via its own `catalogReloadSerial` (not `this.lifecycle`, which `supervisor.stop`'s own
  state writes run through) — nesting into `this.lifecycle` from inside a call already running through
  it would deadlock `AsyncSerial.run`.
- `{ kind: "command" }` readiness (`src/core/catalog.ts`) is the JSON-serializable stand-in for
  `custom` — needed because a `custom` probe is a closure and can't travel over `POST
  /v1/manager/reload` or a YAML file. Its `ProbeAdapter.command` is optional on purpose: every
  existing `SupervisorOptions` test fixture predates it, and `ProcessSupervisor.probe()` degrades to
  `false` (normal readiness-timeout path, never a throw) when it's absent instead of forcing every
  fixture to grow one.
- A `bun build --compile` sidecar (a standalone `lsd` executable the macOS app could bundle instead of
  shelling out to `bun run <ts file>`) was tried and rejected: on the `self-hosted, macmini` CI/dev
  machine, a freshly-compiled, ad-hoc-signed Bun executable gets SIGKILLed on launch — reproduces even
  with a trivial "hello world" compile, while the long-installed system `bun` (also only ad-hoc
  signed) runs fine, so it reads as an endpoint-security heuristic against newly-written unsigned
  executables, not a fixable code-signing detail. **This heuristic is Bun-specific, not "any freshly
  compiled unsigned binary"**: the same ad-hoc-signing test with a trivial Rust binary passed cleanly
  (10/10 runs on the `self-hosted, macmini` runner, 23/23 locally) — see "Rust rewrite" below. Likely
  explanation: the heuristic targets JIT/executable-writable-page behavior a JS engine needs, which a
  static Rust binary doesn't have. `apps/macos` still runs `bun run src/bin/lsd.ts` for the existing
  TypeScript implementation (see `SidecarLocator.swift`).
- The registered `{self-hosted, macmini}` CI runner's Swift toolchain has no XCTest (`no such module
  'XCTest'` — Command Line Tools only, no full Xcode.app), unlike a normal dev machine (confirmed:
  `swift build` succeeds there, `swift test` fails). `apps/macos/.github/workflows/macos-app.yml` runs
  `swift build` only; `apps/macos/Tests/LocalServicesAppTests` exists and passes locally but isn't in
  CI. Don't re-add `swift test` to that workflow without first fixing the runner's Xcode install.

## Rust rewrite (in progress)

A full rewrite of every subpath (`core`/`cli`/`tui`/`mcp`/`node-bridge`) + the `lsd` bin into Rust is
underway, living at `rust/` in this same repo, rolled out incrementally alongside the existing
TypeScript implementation (each of the 3 consumers — `apps/macos`, `viclass`, `infra` — cuts over
independently once its needed surface has verified parity; no big-bang replace). The full design
(crate layout, crate choices, milestone ordering, cutover plan, top risks) lives in the plan this was
built from — ask for it by name if picking this back up, or re-derive it from this section plus the
crate doc comments, which mirror the plan's reasoning inline.

Status:
- **Phase 0 (signing/Gatekeeper spike): GO** — see the sharp-edge note above. A compiled Rust binary
  is not subject to the heuristic that killed compiled Bun binaries.
- **Phase 1 (`ls-core` types + config-file loader): done.** `rust/crates/ls-core/src/{catalog,state,
  paths,platform,file_io,env,doctor,config_file}.rs` — 48 tests passing (`cargo test` from `rust/`),
  including a 1:1 port of `test/core/catalog.test.ts`. Clippy-clean.
- **Known gap, not yet resolved**: `config_file.rs` has no equivalent of the TypeScript loader's
  `.config.ts` escape hatch (dynamic `import()` of a TS module) — Rust can't evaluate TypeScript.
  Both real downstream consumers (`viclass`, `infra`) currently author exactly this kind of file at
  their project root (`local-services.config.ts`, a one-line re-export of a pure-data catalog). A
  `.config.ts` path returns a clear "not supported" error today rather than silently misbehaving.
  This blocks a real cutover for those two repos until resolved — options noted in `config_file.rs`'s
  module doc comment (shell out to `bun` to dump JSON, or migrate those two files to YAML since their
  catalogs are already pure data) are not yet decided.
- **Phase 2 (`ProcessSupervisor`): in progress.** Ported so far, deliberately first (the plan calls
  this the highest-risk phase — get the sharp edges right before building the stateful supervisor on
  top of them): `rust/crates/ls-core/src/supervisor/fingerprint.rs`
  (`normalize_command_fingerprint`/`normalize_observed_command_fingerprint`, the argv/shell fingerprint
  pairing sharp edge) and `.../supervisor/process_tree.rs` (the whole-process-tree snapshot/BFS/
  pid-reuse-guard logic), including a real OS-level port of
  `terminate-tree-regression.test.ts` that spawns a `set -m; sleep 60 & wait` shell and proves the
  tree-walk+signal logic actually kills a child that forked into its own process group — 5/5 clean
  runs, no flakiness observed. **Sharp edge hit while porting this test**: the spawned shell must be
  given its own process group via `process_group(0)` (`setpgid(0,0)`, mirroring the real adapter's
  `detached: true` spawn) — without it, the child inherits the *test harness's own* pgid, and the
  test's `killpg` calls signal the whole cargo-test process group instead of the subtree under test.

  **The stateful `ProcessSupervisor` struct itself is now ported** (`.../supervisor/engine.rs`,
  ~1300 lines incl. tests): start/stop/restart/status/reconcile/`sync_external_services`/shutdown,
  readiness probing per kind (process/tcp/http/container/tailnet/command, matching the "no adapter
  configured degrades to timeout, never throws" sharp edge), retained-identity adoption with the
  liveness re-check, build execution with timeout + external cancellation +
  `serializationKey`-based one-at-a-time serialization, and the two-pass shutdown (active-state stop,
  then persisted-identity reap). Driven entirely through the `ProcessAdapter`/`ProbeAdapter`/
  `PreparationAdapter`/`RunBuild`/`Host`/`SupervisorClock` traits so it's testable without real OS
  processes — 22 tests using fakes (a representative subset of `supervisor.test.ts`'s ~40 scenarios,
  not an exhaustive 1:1 port: happy-path start/restart/stop, readiness timeout, TCP port-conflict
  refusal, the shutdown two-pass reap, fingerprint-mismatch orphaning, spawn/build failure, build
  serialization-by-key, command-readiness-with-no-adapter, container identity, external-ownership
  adopt/release, externally-killed-process reconciliation). 84 tests total in the crate, clippy-clean,
  5/5 clean repeated full-suite runs (no flakiness observed).

  **Default (production) adapters are now ported too** (`.../supervisor/default_adapters.rs`):
  real `tokio::process::Command`-based spawning (own process group via `.process_group(0)`, raw-file
  stdout/stderr capture + `tail_file` copytruncate polling for `attach_output`, exactly mirroring
  the TS "don't pipe into the daemon, a dead daemon would SIGPIPE the child" reasoning), real `ps`
  inspection (`observed_system_process`), the exec-fingerprint settling loop
  (`observed_stable_exec_process`), Docker container spawn/inspect/stop via `docker
  compose`/`docker inspect`, tailnet probing via `tailscale serve status --json`, TCP probing via
  `tokio::net::TcpStream`, HTTP probing via `reqwest` (rustls, not the system TLS/OpenSSL, for
  portability), and build execution with real cancellation (SIGTERM the process group, wait, then
  SIGKILL). `default_supervisor_options(root, runtime_directory, base_environment)` wires all of it
  together, mirroring `defaultSupervisorOptions`. 6 new tests, including a real end-to-end one that
  starts an actual `ProcessSupervisor` with these real adapters, spawns a real `nc`-backed TCP
  service, waits for real `tcp` readiness against a real bound port, stops it, and asserts the real
  OS process is gone (`kill(pid, 0)` fails) afterward — this is the strongest evidence so far that
  the ported engine and its real adapters work together, not just against fakes. 90 tests total,
  clippy-clean, 5/5 clean repeated full-suite runs.

  **Not yet ported** (tracked here as the actual remaining checklist, not "the rest of the file"):
  the remaining `supervisor.test.ts` scenarios this pass didn't cover (queued-start cancellation
  mid-flight, exact persisted-identity reclaim without respawn, PID-reused-but-port-held
  externally-owned retention, Docker Compose concurrent-start serialization exercised against a
  real `docker compose` project, abort-in-flight-build-before-restart, stop-a-preparing-service-
  cleanly). Docker/tailnet code paths compile and are exercised by unit-level parsing logic, but
  have **not** been integration-tested against a real `docker compose`/`tailscale serve` setup in
  this pass — flagging so a future pass doesn't assume they're as battle-tested as the POSIX path.
- **Phase 3 (`LocalServicesManager`/daemon): substantially done.** Built bottom-up, each piece
  tested in isolation before wiring into HTTP: `manager/event_store.rs` (the SSE ring buffer,
  deliberately kept simple — see below), `manager/operations.rs` (per-target operation
  serialization; hit and fixed a real `wait()`/`drain_services()` race where re-acquiring a
  `tokio::sync::Mutex` from outside raced against `tokio::spawn` merely *scheduling* the task,
  fixed with a per-operation `watch` channel), `manager/state_store.rs` (`state.json` load/save +
  legacy `units`/`unitId` migration), `manager/lock.rs` (HMAC ownership proofs + the `claim_lock`
  protocol — the "a healthcheck timeout is never death" 263-daemon-incident guard, tested against a
  real axum server standing in for a live manager), `manager/log_store.rs` (crash-safe two-phase
  rotation journal + UTF-8-safe cursor tailing). `manager/http.rs` then ties all of it plus a
  `ProcessSupervisor` together behind a real `axum` server on a loopback OS-assigned port,
  including the full route table (`/healthz`, `/v1/manager`, `/v1/catalog`,
  `/v1/manager/reload`, `/v1/services`, `/v1/operations`, `/v1/operations/bulk-start`,
  `/v1/operations/:id`, `/v1/events`, `/v1/events/stream` (SSE), `/v1/logs/:id`,
  `/v1/manager/shutdown`), the `startSelectedDag` concurrent-per-node dependency scheduler (via
  `futures::future::Shared`, not naive level-by-level batching — preserves the same
  finer-grained parallelism the TS version gets from per-node promise memoization), and
  bootstrap/shutdown. Proven with a real end-to-end test: a real bootstrapped manager, a real `nc
  -lk`-backed TCP service, driven entirely over real HTTP (start → poll operation → verify ready →
  read logs/events → stop → verify the real OS process is gone → shut the manager down) — the same
  category of capstone test that closed out Phase 2. 143 tests total in the workspace, clippy-clean,
  5/5 clean repeated runs. **Sharp edge hit while writing that test**: plain `nc -l <port>` (no
  `-k`) exits after accepting one connection — and the TCP readiness probe's own `connect()` IS that
  one connection, so the service went `ready` and then immediately `failed` (exit-watcher fired)
  before the test's next assertion ran. Not a bug in the port; a test-double gotcha worth remembering
  for any future test that needs a TCP service to actually stay up.
  **Deliberately simplified, not yet fully faithful**: `/v1/events/stream`'s SSE backpressure is a
  bounded `tokio::sync::mpsc` channel gated by frame *count* (64, matching `maxSseQueueFrames`) —
  the TS source's additional cumulative *byte-size* ceiling (`maxSseQueueBytes`) isn't separately
  tracked. Given SSE frames here are small JSON, frame-count bounding already caps memory to a small
  multiple in practice, but a future pass should add the byte tracking for full parity if a real
  workload ever produces unusually large events.
  **Not yet done**: `daemon.rs` itself (`runDaemon`, `LockOwnershipWatch`, the `DaemonLifecycle`
  SIGINT/SIGTERM wiring) — Phase 3's daemon-*process* glue, as opposed to the `LocalServicesManager`
  it wraps, which is what's built so far.
- **Not started**: Phase 4 (`ls-cli`), Phase 5 (`lsd` bin), Phase 6 (`ls-tui`), Phase 7 (`ls-mcp`).

## Maintaining this file

Keep this file for knowledge useful to almost every future agent session in this project.
Do not repeat what the codebase already shows; point to the authoritative file or command instead.
Prefer rewriting or pruning existing entries over appending new ones.
When updating this file, preserve this bar for all agents and keep entries concise.
