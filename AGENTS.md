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
  and publishes to GitHub Packages only on a `vX.Y.Z` tag matching `package.json`'s version. A
  separate `rust-test` job in the same workflow runs `cargo test --workspace` and `cargo clippy
  --workspace --all-targets -- -D warnings` from `rust/` (unconditionally, not path-filtered, same
  as the TS `test` job) — added once Phase 7 closed out the Rust rewrite below, since until then
  every one of its ~220 tests had only ever been run manually, never gated in CI. Since the
  Docker/tailnet real-adapter tests noted under Phase 2 below, `rust-test` also needs a working
  `docker` (daemon running) and `tailscale` on the runner, on top of the `bun` it already needed for
  the `.config.ts` escape hatch tests.
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
- **`.config.ts` escape hatch: resolved.** `config_file.rs`'s `load_typescript_catalog` shells out to
  a real `bun -e <script>` subprocess that `import()`s the module and prints its exported `catalog`
  (or default export) as JSON, which is then deserialized straight into `ServiceCatalog` — the same
  "shell out to bun" option the migration plan had left open, chosen over migrating `viclass`'s and
  `infra`'s `.config.ts` files to YAML since that would be follow-up work in *their* repos rather
  than something resolvable here. Error text mirrors the TS source's own two messages (`failed to
  import ...` / `... must export a ServiceCatalog as \`catalog\` or a default export`) since the
  script constructs them itself before Rust ever sees stderr. **Real, standing limitation, not a
  porting gap**: a `custom` readiness probe (a `(ctx) => Promise<...>` closure) cannot cross the JSON
  boundary this needs, and `ReadinessSpec` has no `Custom` variant in this crate at all (see its own
  doc comment) — a `.config.ts` catalog using `custom` fails deserialization with a clear error
  rather than silently dropping the probe. Both real consumers' catalogs are pure data today, so this
  doesn't block their eventual cutover. 5 new tests (named export, default export, non-catalog
  export, a throwing module, and the `custom`-readiness deserialization failure), all shelling out to
  a real `bun` exactly like the production code path — 226 tests total in the workspace now, cargo
  test/clippy repeated 5/5 clean. This is also the first place the Rust workspace *requires* `bun` on
  PATH at runtime (not just in dev/CI) — narrowly, only for a project that opts into a `.config.ts`
  catalog; every YAML/JSON-catalog consumer still needs no Bun at all.
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
  cleanly).

  **Docker/tailnet real integration coverage: resolved in a later pass** (`default_adapters.rs`,
  after Phase 7 closed out the rest of the rewrite). Two new tests:
  `real_process_supervisor_starts_and_stops_a_real_docker_compose_service` drives a real
  `docker compose` project (a throwaway `alpine` service, unique-per-run project name) through a
  real `ProcessSupervisor`/`DefaultProcessAdapter` — start, verify `ActualServiceState::Ready` with
  a real `ProcessIdentity::Docker` and `docker inspect` agreeing it's running, stop, verify
  `docker inspect` agrees it's really gone — always tearing the compose project down (even on a
  failed assertion) so a broken test run never leaves a live container behind.
  `tailnet_serving_agrees_with_the_real_tailscale_serve_status` calls the real
  `tailnet_serving`/`DefaultProbeAdapter::tailnet` against whatever `tailscale serve` state already
  exists on the test machine — deliberately **read-only**, never calling `tailscale serve` to add or
  remove anything, since that would mutate a real (and on a dev machine, possibly shared/personal)
  Tailscale configuration outside the test's control. Instead it independently re-derives the same
  "does `Web` have any entries" check from a fresh `tailscale serve status --json` and asserts the
  production function agrees, so the assertion is grounded in live system state rather than a
  hardcoded `true`/`false` that would silently stop meaning anything if the machine's Tailscale
  setup ever changes. **New, real requirement**: both tests need a working `docker` (daemon running)
  and `tailscale` on the machine running `cargo test` — confirmed present on this session's own
  machine (the same kind of environment as the `{self-hosted, macmini}` CI runner) before writing
  them; every other real-tool-dependent test in this crate (`nc`, `ps`, `sh`) makes the same
  assumption, so this isn't a new category of fragility, just a new pair of tools in that category.
  228 tests total in the workspace, clippy-clean, 5/5 clean repeated runs, no leftover containers
  after any run.
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
  **`daemon.rs` (the daemon-*process* glue around `LocalServicesManager`) is also ported**:
  `create_daemon_log` (rotated diagnostics file), `read_lock_instance_id` (the three-way missing/
  unreadable/found distinction — a transient read failure must never be mistaken for losing the
  lock), `LockOwnershipWatch` (the losing-side lock-takeover self-stop), `DaemonLifecycle`
  (memoized shutdown-once, via a `ShutdownManager` trait so it's testable without a real manager),
  and `run_daemon` (bootstrap + SIGINT/SIGTERM/SIGHUP wiring). One documented, deliberate gap: Bun's
  global `unhandledRejection`/`uncaughtException` hooks have no direct Rust equivalent (a panicking
  spawned task is caught at that task's own `JoinHandle`, not globally) — narrower than it looks
  since fire-and-forget work in this codebase already routes errors through
  `Host::record_background_error` rather than panicking; noted rather than papered over with a
  global panic hook that would itself diverge from the TS design. `run_daemon` itself doesn't yet
  have a dedicated test (needs a real long-running process context that will come naturally once
  the `lsd` bin exists in Phase 5); its constituent pieces (log rotation, lock-watch, lifecycle
  memoization) are independently tested — 8 new tests. **Phase 3 is now essentially complete**:
  151 tests total in the workspace, clippy-clean, stable across repeated runs.
- **Phase 4 (`ls-cli`): done.** New crate `rust/crates/ls-cli`, porting `src/cli/localctl.ts` in
  full: lock-file discovery (`discover`/`ensure`/`require_client`, reusing `ls-core`'s own
  ownership-proof verification so a Rust CLI and a Rust daemon agree on what a valid lock looks
  like), the `doctor`/`cleanup`/`manager ensure|status|stop|reload`/`status`/`start|stop|restart`/
  `operation get|watch`/`logs` commands, and the same exit-code scheme (usage=2, unavailable=3,
  protocol=4, failed=5, timeout=6, unauthorized=7). One simplification worth knowing about:
  `operation_id`'s URL-encoding step is skipped, because the validating regex
  (`^[A-Za-z0-9._~-]{1,128}$`) only accepts RFC 3986 "unreserved" characters, which by definition
  never need percent-encoding — the TS `encodeURIComponent` call there is a no-op in practice, not
  a behavior this port is missing.

  Tested against **real bootstrapped `LocalServicesManager`s** (not mocks) end-to-end through
  `main()`'s actual argv dispatch: `status --json`, `start api --wait --json` (verifies the real
  process reaches `ready`), `manager ensure --json`, `doctor`, an unknown command, and `status`
  against no running manager at all (→ exit 3). One test also drives `ensure()`'s "spawn if
  absent" path by wiring `spawn_daemon` to a real `ls_core::daemon::run_daemon` call — the same
  lock-file discovery this CLI does independently finds the daemon it just spawned, which is the
  actual contract that matters (a real consumer's `spawn_daemon` closure looks exactly like this).
  164 tests total in the workspace (151 `ls-core` + 13 `ls-cli`), clippy-clean, stable across
  repeated runs.
- **Phase 5 (`lsd` bin): done.** New binary crate `rust/bin/lsd`, porting `src/bin/lsd.ts`: `lsd
  daemon --root <path>` (loads the catalog, resolves the base environment, calls
  `ls_core::daemon::run_daemon` directly — this invocation *is* the daemon process body once
  spawned detached) and every other subcommand delegating to `ls_cli::main` after loading the
  catalog, with the exact same `--root`-extraction contract in both places so the two never
  disagree on which project root an invocation means. `spawn_daemon` re-invokes
  `std::env::current_exe()` as `lsd daemon --root <root>` with its own process group and discarded
  stdio — Rust has no `Bun.spawn(...).unref()` equivalent to reach for; the same effect (the parent
  can exit without waiting for or killing the child) falls out for free from just not calling
  `.wait()` on the spawned `Child` and letting the handle drop. `tui` is deliberately not wired yet
  (Phase 6 doesn't exist as a crate to depend on) — `ls_cli::main`'s own `tui` command already
  surfaces a clear "not available" message rather than silently doing nothing.

  Proven two ways: a real end-to-end subprocess test (`rust/bin/lsd/tests/end_to_end.rs`, the
  closest equivalent to `test/bin/lsd.test.ts`) drives the actual compiled binary through
  `manager ensure --json` → `start --wait --json` → `status --json` → `logs` → `manager stop
  --json` → a fresh `manager ensure` proving the old instance is really gone — and a dedicated
  test that ad-hoc signs the real (not hello-world) compiled binary and confirms it still runs,
  reconfirming Phase 0's signing spike against the actual multi-thousand-line artifact a real
  consumer would ship, not just a trivial stand-in. 169 tests total in the workspace, clippy-clean,
  stable across repeated runs.
- **Phase 6 (`ls-tui`): done.** New crate `rust/crates/ls-tui`, porting all of `src/tui/*.ts`:
  `state.rs` (`TuiState`/`ServiceSelection`, the connection/request/selection fence machinery),
  `text_utils.rs` (`sanitize_terminal_text`, `visible_width`, `truncate_to_width`), `actions.rs`
  (`keyboard_action`), `screen.rs` (`ServiceScreen`, the cached `(state, geometry) -> lines` layout
  engine), `client.rs` (`ManagerTuiClient` — reuses `ls_cli::{discover, request, require_client,
  wait_operation}` directly rather than re-implementing HTTP plumbing a second time), and `run.rs`
  (`run_tui`, the terminal event-loop orchestration). Wired into `lsd tui` (`rust/bin/lsd/src/
  main.rs`), which intercepts that subcommand itself before delegating to `ls_cli::main` — `ls-cli`
  can't depend on `ls-tui` (that would be circular), so there's no TS-style injectable
  `LocalctlRuntime.tui` handler; the binary that depends on both crates is the natural place to
  wire the concrete implementation in. 45 new tests (214 total in the workspace), clippy-clean,
  5/5 clean repeated full-suite runs.

  **`crossterm`+`unicode-width` chosen over `ratatui`**, confirmed by an earlier research pass: the
  TS TUI only uses `pi-tui` at a low level (raw mode, SGR mouse parsing, key matching), never a
  widget-tree framework, so there was no framework-level API to match — `crossterm`'s structured
  `KeyEvent`/`MouseEvent` types mean `actions.rs` takes one directly instead of re-deriving pi-tui's
  raw-byte key matching (a deliberate, documented deviation, not a corner cut).

  **Two primitives had no vendored source to port from.** pi-tui's `DEFAULT_TAB_WIDTH` and its
  `truncateToWidth`/`visibleWidth` delegate to a native (compiled) addon (`@oh-my-pi/pi-natives`)
  whose Rust source isn't published in this checkout's `node_modules` — both were reverse-engineered
  by probing the real compiled functions directly (`bun -e 'import { truncateToWidth } from
  "@oh-my-pi/pi-tui"; ...'`) rather than read from source. Findings: the tab width is a **fixed
  3-space replacement per tab, not a tab-stop calculation** (confirmed by expanding tabs after
  prefixes of several different lengths — every tab always becomes exactly 3 spaces regardless of
  the column it starts at); `truncate_to_width`'s SGR-colour handling around a truncation cut point
  reproduces every case probed (~a dozen inputs covering open/reset colours before, at, and after
  the cut) but the native engine may special-case inputs outside that probing — see the extensive
  doc comment on `truncate_to_width` in `text_utils.rs` for the exact reproduced rule. None of the
  ported TUI tests exercise the unprobed edge cases, so this is unlikely to matter for real service
  log output, but it is a real fidelity gap distinct from every other "documented simplification" in
  this file, which were all judgment calls made *with* the source in hand.

  **`run.rs` (the terminal event-loop orchestration) is the one piece with no automated test
  coverage** — it owns a real terminal and a real reconnect loop, which is exactly the parity bar
  the original migration plan set for this phase ("ported unit tests pass + a manual smoke pass,
  rendering isn't cleanly auto-diffable"). Three deliberate deviations from the TS source, each
  called out in `run.rs`'s own module doc comment: no incremental-diffing renderer (`pi-tui`'s
  `TUI`/`ProcessTerminal` diff the terminal; this does a full clear + redraw every frame via plain
  ANSI — more flicker-prone under a very fast event stream, functionally equivalent otherwise);
  action dispatch (`start`/`stop`/`restart`/`start all`) runs to completion inline in the same loop
  that reads input, instead of the TS source's fire-and-forget that lets input keep being processed
  mid-request (Rust's single-owner `TuiApp` makes true concurrent mutation awkward without
  message-passing the whole action back in, not worth it for an already-`busy`-gated, typically
  sub-second HTTP round trip); and the `crossterm`-vs-raw-bytes input deviation noted above.
- **Phase 7 (`ls-mcp`): done — the last phase of the rewrite.** New crate `rust/crates/ls-mcp`,
  porting `src/mcp/mcp-server.ts` on top of `rmcp` 3.4.0 (the official Rust MCP SDK): `client.rs`
  (the `LocalServicesMcpClient` trait — `async-trait`-boxed, matching this codebase's existing
  object-safe-trait pattern, e.g. `ls-core`'s `Host` — plus `ManagerApiClient`, built on `ls-cli`'s
  own `require_client`/`request`/`runnable_targets` rather than a third HTTP re-implementation) and
  `server.rs` (`LocalServicesMcpServer`, argument validation, secret-key/Bearer-token redaction).
  `rmcp` was evaluated shallowly in an earlier pass and deep-dived only now, per the original plan.

  **Hand-implements `ServerHandler`** (`get_info`/`list_tools`/`call_tool`) rather than using
  `rmcp`'s declarative `#[tool]`/`#[tool_router]` macros — those generate a tool's name and JSON
  schema at compile time, but this server's tool names carry a runtime-configurable prefix and the
  `status`/`logs`/`manage` schemas embed an `enum` of the caller's actual `knownServiceIds`, known
  only at construction time. Every other `ServerHandler` method (resources, prompts, subscriptions,
  tasks, discover) is left at its provided default, mirroring how the TS `Server` only ever
  registers `ListToolsRequestSchema`/`CallToolRequestSchema` handlers.

  **`rmcp` 3.4.0 implements the 2026-07-28 MCP spec revision** (server discovery, tasks,
  multi-round-trip requests, response caching — none of which this server or the TS source use),
  so `ServerHandler`'s method list is much larger than the TS SDK's; only the three methods above
  needed overriding; `CallToolRequestParams`/`CallToolResponse`/`ListToolsResult`/`ServerConfig`
  (`= InitializeResult`) are all `#[non_exhaustive]`, so built through their constructor/builder
  methods (`Tool::new`, `ListToolsResult::with_all_items`, `ServerConfig::new(...)
  .with_server_info(...)`, `CallToolResult::success`/`::error`) rather than struct-literal syntax.

  **One small upstream fix in `ls-core`** was needed to make this crate's `Arc<dyn
  LocalServicesMcpClient + Send + Sync>` compile at all: `ls_core::doctor`'s
  `DoctorCheckPredicate`/`DoctorCheckDetailFormatter` closures (unused by any real caller today)
  were `Box<dyn Fn(...)>` with no `Send + Sync` bound, which transitively made all of
  `LocalctlOptions` — and therefore `ManagerApiClient`, which owns one — not `Sync`. Widened to
  `Box<dyn Fn(...) + Send + Sync>`; free, since nothing constructs one of these closures yet.

  Tested two ways: 7 in-crate tests (a 1:1 port of `mcp-server.test.ts`'s scenarios — confirm-gating
  by default and when disabled, unknown-service/extra-argument rejection, prefix-qualified tool
  names and ordering) driving a real in-process client/server pair connected over a
  `tokio::io::duplex` transport (this codebase's Rust equivalent of the TS SDK's
  `InMemoryTransport.createLinkedPair()`), and a real end-to-end test
  (`rust/crates/ls-mcp/tests/end_to_end.rs`) against a real bootstrapped `LocalServicesManager` and
  a real `nc -lk`-backed TCP service: `status` (stopped) → `manage` start (waits on real TCP
  readiness, returns the real pid) → `logs` → `events` (asserts a real `service.lifecycle` event
  landed) → `trace` on a made-up operation id (asserts a tool-level error, not a protocol error) →
  `manage` stop (asserts the real OS process is gone) — the same capstone-test category that closed
  out every earlier phase. 222 tests total in the workspace, clippy-clean, 5/5 clean repeated runs.

  **Sharp edge hit while writing the in-crate tests**: `service.serve(transport)` performs the
  `initialize` handshake as a real round trip — each side's call blocks until it hears from the
  other — so awaiting the server's and the client's `.serve()` calls *sequentially* deadlocks
  forever (confirmed the hard way: a test hung indefinitely, 0% CPU, no compile activity). Fixed by
  driving both concurrently with `tokio::join!`. Worth remembering for any future in-process
  client/server test against any `rmcp`-based service, not just this one.

  **Deliberately not wired into `lsd`**: unlike `tui` (a real `localctl.ts` CLI subcommand this
  binary intercepts), the TS source has no `mcp` subcommand anywhere in `localctl.ts`/`lsd.ts` —
  `/mcp` is a subpath a consumer embeds directly into its own MCP host (e.g. a few lines wiring
  `createLocalServicesMcpServer` to a stdio transport in a small wrapper script), the same shape
  `ls-mcp` preserves. Adding an `lsd mcp` subcommand would be new scope beyond what was ported.

  **This closes out the Rust-rewrite plan's Phase 0–7 checklist.** Everything below "Sharp edges" in
  the Rust rewrite section is now settled to the extent this repo's own scope requires — both open
  items from earlier phases (the `.config.ts` escape hatch, Phase 1; the untested Docker/tailnet
  integration paths, Phase 2) were resolved in passes after this one; see their own entries above.
  Real cutover of any of the three downstream consumers (`apps/macos`, `viclass`, `infra`) onto this
  Rust implementation is separate follow-up work, not part of this repo's own scope (see the
  Phase 2/4-scope sharp-edge note above).

## Maintaining this file

Keep this file for knowledge useful to almost every future agent session in this project.
Do not repeat what the codebase already shows; point to the authoritative file or command instead.
Prefer rewriting or pruning existing entries over appending new ones.
When updating this file, preserve this bar for all agents and keep entries concise.
