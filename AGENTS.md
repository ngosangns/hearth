# Project agent memory

Project-intrinsic knowledge that should travel with the code: orientation, build/test/release,
architecture, and sharp edges. Add durable notes here as real work discovers them.

## Orientation

This repo ships **two implementations of the same product**, both live:

| | TypeScript (`src/`) | Rust (`rust/`) |
| --- | --- | --- |
| What it is | The published npm package, `@gnasdev/local-services` | A full rewrite, compiled to the `lsd` binary |
| Consumers | `viclass` | `apps/macos`, `infra` |
| Surfaces | `/core` `/cli` `/tui` `/mcp` `/node-bridge` subpath exports + an `lsd` bin shim | `ls-core` `ls-cli` `ls-tui` `ls-mcp` crates + `rust/bin/lsd` |
| Status | Maintained; still the published artifact | Feature-complete, at parity, and what new consumers should use |

They speak the same loopback HTTP+SSE protocol, write the same `state.json` and lock file, and share
`PROTOCOL_VERSION`, so a daemon from one and a client from the other can legitimately meet. **A change
to wire shape, persisted state, or catalog schema must land on both sides**, or a consumer running
mixed versions breaks silently. `src/core/state.ts` and `rust/crates/ls-core/src/state.rs` are the
authoritative pair.

There is no Rust counterpart to `/node-bridge` (a Node-only shim letting a Node task runner shell out
to the Bun CLI) and there does not need to be — a Node caller spawns the `lsd` binary directly.

## Architecture

One package, five subpath exports plus a `lsd` bin — see README.md for the full design and a minimal
consumer example. **No default `ServiceCatalog` ships anywhere**; every entry point takes a
caller-supplied catalog.

A project can skip authoring its own `catalog.ts`/`daemon.ts`/`cli.ts` entirely: `local-services.yaml`
(or `.yml`/`.json`, or a `.config.ts` escape hatch) is mapped onto `ServiceCatalog` by
`src/core/config-file.ts` / `config_file.rs`, and the `lsd` bin wraps that loader around
`main()`/`runDaemon`. This is also the on-ramp for a non-Bun/non-TS client: `lsd manager ensure --json`
prints everything (`token`, `port`, `runtimeDirectory`, …) a generic HTTP+SSE client needs, without it
reimplementing lock-file discovery. `env.ts`/`env.rs` resolves the daemon's own base environment
(login shell + `.env`) for exactly that case — a daemon launched from a GUI has none of the `PATH`
customization a terminal-launched one inherits for free from `process.env`.

`apps/macos` is that desktop on-ramp: a SwiftUI client running one daemon per workspace folder.
`SidecarLocator` prefers a **compiled `lsd`** — env override, then the app's bundled copy, then known
install locations, then this checkout's `cargo build` output, then the login shell's PATH — and falls
back to `bun run src/bin/lsd.ts` only if none is found. `SidecarLocator.swift`'s own doc comment is
authoritative on the order.
`scripts/build-app.sh` packages an ad-hoc-signed `Local Services.app` with that binary bundled, so a
packaged app needs no `bun` for the common path. Not notarized — distribution to another machine is
the one packaging step still missing. See `apps/macos/README.md`.

## Build, test, release

**TypeScript.** Bun-only, no build step: `src/**/index.ts` is published as TypeScript source and
resolved natively by Bun (see `exports` in package.json). Do not add a bundler/tsc-emit step without
revisiting that decision. `bun test` / `bun run typecheck` (strict + `noUncheckedIndexedAccess` +
`verbatimModuleSyntax`).

**Rust.** From `rust/`: `cargo test --workspace` and `cargo clippy --workspace --all-targets -- -D
warnings`. Also `task rust:test` / `task rust:clippy`.

*The Rust suite needs real tools on the machine*: `bun` (for the `.config.ts` escape-hatch tests),
`docker` with its daemon running and `tailscale` (for the real-adapter tests), plus `nc`, `ps`, `sh`.
Same assumption every real-tool test in the crate already makes, not a new category of fragility.

**Installing/refreshing the `lsd` binary** after a Rust change — `task rust:install`, or by hand from
`rust/`:
```
cargo build --release -p lsd && cp target/release/lsd /opt/homebrew/bin/lsd && codesign --sign - --force /opt/homebrew/bin/lsd
```
The ad-hoc re-sign is required after every copy on macOS.

**macOS app.** `task macos:build` / `macos:test` / `macos:package` / `macos:install`.

**Current test counts are whatever the suites report** — run them. Counts are deliberately not
recorded here; every number this file used to carry had gone stale.

**Release.** CI (`.github/workflows/ci.yml`) runs the TS `test` + `typecheck` jobs and a separate
`rust-test` job on PRs, and publishes to GitHub Packages only on a `vX.Y.Z` tag matching
package.json's version. The Rust binary has no release/distribution process of its own yet
(`publish = false` in the workspace); it is built and installed by hand, or bundled into the macOS
app. `apps/macos`'s workflow is `.github/workflows/macos-app.yml` (repo root, path-filtered to
`apps/macos/**`) and runs `swift build` only.

**Semver doubles as the protocol-compatibility signal**: `PROTOCOL_VERSION` is the one place a daemon
and its TUI/CLI/MCP clients read it from — a bump there must be a major release.

### The self-hosted runner's environment

The registered `{self-hosted, macmini}` runner (`ngosangns-Mini`) is a different physical machine
from any dev box here, with the same username. Two things about it that a local run cannot predict:

- **It executes `run:` steps from the runner service's environment, not a login shell**, so a
  toolchain under `~/.cargo/bin` or Homebrew is not on PATH by default. `rust-test` failed with exit
  127 for a long time because of this. `ci.yml` now resolves cargo's directory into `$GITHUB_PATH`
  itself — keep that in the workflow rather than relying on undocumented machine state.
- **Its Rust is managed by rustup, whose stable toolchain does not include clippy**, while dev
  machines here use Homebrew's rust, which bundles it. A clippy-clean local run therefore does not
  predict CI; `ci.yml` adds the component explicitly (idempotent).
- **Its Swift toolchain has no XCTest** (Command Line Tools only, no full Xcode.app), so
  `apps/macos/Tests/` passes locally but is not in CI. Don't re-add `swift test` to
  `macos-app.yml` without first fixing the runner's Xcode install.

`publish` needs both `test` and `rust-test`, so a red Rust suite blocks a release.

## Sharp edges

Applies to both implementations unless noted.

**Process supervision**

- `normalizeCommandFingerprint` must hash the *logical* command text (argv joined, or the bare
  `shell` string) — never the physical `sh -c` spawn wrapper — so it matches
  `normalizeObservedCommandFingerprint`'s prefix-stripped `ps` output. A future refactor of
  `commandArgv` should re-check this pairing.
- Stopping a service signals its **whole process tree**, snapshotted from `ps` before the first
  signal. `air` runs the built server in its **own** process group, so signalling only the tracked
  pgid leaves the real server alive holding its port, and the next start fails with `Port N is held
  by an unowned process`. The snapshot is only walked while the OS table still shows the recorded
  `startIdentity` for the leader pid, so a stale/reused pid can never pull an unrelated live tree
  into a signal or into the wait-for-death loop.
- An adopted identity is kept only while it still answers its readiness probe; an alive-but-
  unresponsive one (air survives its child) is terminated and replaced, rather than re-adopted on
  every start into a permanent `Readiness timed out`.
- `ProcessSupervisor.shutdown()` does two passes: an "active state" stop pass, then a reap pass for
  daemon-owned services holding a stale POSIX identity in a non-active state (e.g. `externally-owned`
  after a port conflict). Don't collapse these into one.
- A failed container stop must propagate, not be swallowed — the caller transitions the service to
  `stopped` immediately after, which would record a stopped service whose container is still running.

**Daemon / state**

- Fire-and-forget work (unit log forwarding, `syncExternalServices` polling) must never reject into
  the daemon: Bun terminates the process on an unhandled rejection, orphaning every managed service
  (its identity keeps the dead daemon's instance id, so the next daemon can only adopt it). Guarded
  sinks report through `Host.recordBackgroundError`, and `runDaemon` logs stray
  rejections/exceptions to `<runtimeDir>/daemon.log` instead of dying. Rust has no global equivalent
  of Bun's `unhandledRejection` hook (a panicking task is caught at its own `JoinHandle`) — narrower
  than it sounds, because fire-and-forget work here already routes through `record_background_error`.
- A daemon whose lock was taken over stops itself (`LockOwnershipWatch`) and exits without touching
  the winner's lock. Enforced from the losing side so two daemons can never fight over one
  `state.json`.
- `reloadCatalog` stops a removed-but-active service using the **old** catalog and only swaps
  `this.catalog` afterward — the supervisor needs the old definition to know how to stop it, and
  swapping first would make that service briefly vanish from `serviceStates()` while its process was
  still alive. It is serialized against itself via its own `catalogReloadSerial`, not `this.lifecycle`:
  nesting into `lifecycle` from inside a call already running through it would deadlock.
- Never build a client-facing state string with `format!("{:?}", state)` — `Debug` gives
  `QueuedStart`, the wire encoding is `queued-start`. Use `ActualServiceState::as_wire_str()` /
  `ReadinessKind::as_wire_str()`, which are pinned to the serde encoding by test.
- `state.json` quarantines rather than trusts: a shape check that passes but then fails to
  deserialize must fall through to quarantine, never panic — a panic in `load()` kills the daemon at
  bootstrap, the opposite of what quarantining exists for.
- A `reset` from the event store returns the **whole** buffer, not an empty list. `reset` means the
  client's cursor is unusable, so the reply is the snapshot it resynchronizes from.

**Catalog**

- `{ kind: "command" }` readiness is the JSON-serializable stand-in for `custom` — a `custom` probe
  is a closure and can't travel over `POST /v1/manager/reload` or a YAML file. Its
  `ProbeAdapter.command` is optional on purpose: every pre-existing `SupervisorOptions` fixture
  predates it, and `probe()` degrades to `false` (normal readiness-timeout path, never a throw) when
  it's absent, instead of forcing every fixture to grow one.
- `preparationCommand` (with `serializationKey`) is the same idea for a bespoke `PreparationAdapter`,
  run via the *same* `command` adapter that `{ kind: "command" }` readiness uses, independently of
  the opaque `preparation` marker list. Services sharing a key run their preparation one at a time,
  reusing the `KeyedLock` primitive `ServiceBuildProfile.serializationKey` already uses.
  **`viclass`'s ~27 prep-dependent services must all share one key** to reproduce its old
  global-queue behaviour: `ensure_local_certificates` is a check-then-generate race with no locking
  of its own, and concurrent first-time prepares (e.g. `filestore` + `math` + `portal`, which share
  those certs) would corrupt the cert file. `defaultSupervisorOptions`/`default_supervisor_options`
  need no changes for any of this — both already wire a real `command` adapter for readiness.
- `ownership: 'external'` is this package's own generalization of infra's docker/tailnet-task
  adoption carve-out. `test/core/external-ownership.test.ts` is the only coverage of
  `syncExternalServices()`.
- Service order from a config file is **document order**, not sorted — it's user-visible in
  `/v1/catalog`, `lsd status`, both TUIs, and within-level start order.
- A missing/invalid `readiness` must fail the load, not skip the service. Skipping let a typo
  silently delete a service from an otherwise-fine catalog.

**Consumers and the `lsd` binary**

- `bun run` prepends `node_modules/.bin` to PATH, and this package ships
  `bin: {lsd: "./src/bin/lsd.ts"}` — so a bare `lsd` in a consumer's package.json script resolves to
  the **TS shim** (which has no `mcp` subcommand), not the installed Rust binary. Reference
  `/opt/homebrew/bin/lsd` by absolute path in consumer scripts.
- `.config.ts` requires `bun` to be installed (narrowly — a YAML/JSON-catalog consumer needs no Bun
  at all), but **never assume it is on `PATH`**: `lsd` is routinely spawned by the macOS app, and a
  Dock/Finder-launched GUI process gets launchd's bare `/usr/bin:/bin:/usr/sbin:/sbin`. `find_bun`
  searches an override, `PATH`, bun's and Homebrew's install dirs, then the login shell's `PATH`.
  The daemon that GUI-spawned `lsd` starts inherits the same bare `PATH`, so `lsd` appends Homebrew
  and `~/.bun`/`~/.cargo` bin dirs to its own `PATH` before its runtime starts
  (`with_known_tool_directories`) — otherwise `docker compose`, `tailscale serve status` and
  `tailscale status` (the `{tailnetHost}` URL placeholder) all fail to spawn. Test anything the app
  spawns under `env -i PATH=/usr/bin:/bin:/usr/sbin:/sbin` — launching the app with `open` from a
  terminal does not reproduce what the user gets.
- `load_typescript_catalog` must canonicalize the path before handing it to `bun -e`: `import()`
  from an eval'd script has no importer file to resolve a relative specifier against, and falls
  into node_modules-style resolution instead.
- `serde_json`'s `preserve_order` feature is on workspace-wide and is load-bearing: without it
  `Value`'s object type is a `BTreeMap`, and `lsd mcp install` silently alphabetizes every key in any
  hand-maintained config file it touches.
- `lsd mcp install --key <name>` exists because infra's registry uses `servers`, not the standard
  `mcpServers`. The merge only ever touches `command`/`args`, preserving every other field on the
  entry and every other entry in the file. A residual limitation: `to_string_pretty` can't preserve a
  compact single-line array, so untouched entries' arrays get expanded — cosmetic, no key/value/order
  effect.

**Testing gotchas**

- The macOS app's controllers take `any ManagerAPI`, not the concrete `ManagerClient`, so
  `FakeManagerAPI` can drive them without a daemon; `WorkspaceController` also takes an injectable
  connector and a flag to skip the real config-file watcher. Prefer stepping `refresh()`/
  `fetchOnce()` directly over waiting on the real poll timer. Note the model is `ManagerOperation`,
  not `Operation` — the latter shadows `Foundation.Operation` and is unnameable from the test module.

- **A fixed sleep waiting on another process is a flake.** Three tests bet on a wall-clock duration
  (a detached daemon finishing shutdown, a writer producing N lines, a process starting up) and
  failed under load for reasons unrelated to the code under test. Poll for the actual condition
  with a generous deadline instead; the assertion stays exactly as strong.
- Right after spawning, a child can still be mid-`execve`, and macOS `ps` reports a parenthesized
  placeholder (`(sh)`) instead of its command line. Never treat the first readable `ps` row as a
  just-spawned process's authoritative fingerprint — see `accepts_spawn_observation`. Reproduces
  ~15% of the time from the TS port, effectively never from Rust (its `ps` call is slower to land),
  so the Rust side tests the decision directly rather than the race.
- A spawned test shell needs its own process group (`process_group(0)` / `setpgid(0,0)`, mirroring
  the real adapter's `detached: true`), or it inherits the *cargo-test harness's own* pgid and the
  test's `killpg` calls signal the whole test run.
- `nc -l <port>` without `-k` exits after accepting one connection — and the TCP readiness probe's
  own `connect()` **is** that connection, so the service goes ready then immediately failed.
- An in-process `rmcp` client/server pair deadlocks if both `.serve()` calls are awaited
  sequentially: `initialize` is a real round trip, so drive them with `tokio::join!`.

**Rejected approaches (don't re-litigate)**

- A `bun build --compile` sidecar: on the `self-hosted, macmini` machine a freshly compiled, ad-hoc
  signed Bun executable is SIGKILLed on launch — reproducing even for a trivial hello-world, while
  the long-installed system `bun` (also only ad-hoc signed) runs fine. Reads as an endpoint-security
  heuristic against newly written unsigned executables with the JIT/writable-exec pages a JS engine
  needs. **It is Bun-specific**: the same ad-hoc-signing test with a trivial Rust binary passed 10/10
  on that runner and 23/23 locally, which is what made the Rust rewrite viable in the first place.

## Rust port: deviations from the TS source

Deliberate, and each a real behavioural difference worth knowing before debugging a mismatch.

- **No `Custom` readiness variant.** A closure can't cross the JSON boundary `.config.ts` loading
  needs, so a catalog using `custom` fails deserialization with a clear error rather than silently
  dropping the probe. Both real consumers' catalogs are pure data today.
- **SSE backpressure is frame-count only** (64 frames). The TS source additionally enforces a
  cumulative byte ceiling (`maxSseQueueBytes`). Frames here are small JSON, so count-bounding caps
  memory in practice, but full parity needs the byte tracking.
- **`.config.ts` is evaluated by shelling out to a real `bun -e` subprocess** rather than in-process.
  Error text mirrors the TS source's own two messages, since the script constructs them itself before
  Rust ever sees stderr.
- **`crossterm` + `unicode-width` instead of `pi-tui`.** The TS TUI only uses `pi-tui` at a low level
  (raw mode, SGR mouse parsing, key matching), never as a widget framework, so there was no
  framework-level API to match. `actions.rs` takes a structured `KeyEvent` directly instead of
  re-deriving pi-tui's raw-byte key matching.
- **`ls-tui`'s `run.rs` has no automated coverage** (it owns a real terminal and a real reconnect
  loop) and deviates three ways, each noted in its own module doc: full clear+redraw instead of
  incremental diffing; action dispatch runs to completion inline rather than fire-and-forget; and the
  structured-input deviation above.
- **`truncateToWidth`/`visibleWidth`/`DEFAULT_TAB_WIDTH` were reverse-engineered**, not ported —
  pi-tui delegates them to a native addon whose Rust source isn't in this checkout. Probed against
  the real compiled functions: the tab width is a **fixed 3-space replacement, not a tab-stop
  calculation**, and the SGR-colour handling around a cut point reproduces every case probed (~a
  dozen covering open/reset colours before, at and after the cut). Inputs outside that probing may be
  special-cased by the native engine — a real fidelity gap, distinct from every other deviation here,
  which were judgement calls made *with* the source in hand. See `truncate_to_width`'s doc comment.
- **`ls-mcp` hand-implements `ServerHandler`** rather than using `rmcp`'s `#[tool]` macros: those
  generate a tool's name and schema at compile time, but this server's names carry a runtime-
  configurable prefix and its schemas embed an enum of the caller's actual `knownServiceIds`.
- **`lsd mcp` and `lsd tui` are intercepted in the binary**, not in `ls-cli`: `ls-cli` can't depend on
  `ls-mcp`/`ls-tui` without a dependency cycle, so the crate that depends on both wires them.
  `lsd mcp install` / `lsd skill install` are pure file manipulation and do live in `ls-cli`.
- **Rust's `state.json` timestamp validation is a non-empty-string check**, not the ISO round-trip the
  TS source performs.

## Consumer cutover status

| Consumer | Uses | Remaining work |
| --- | --- | --- |
| `apps/macos` | Compiled `lsd`, bundled into the app | — |
| `infra` | Compiled `lsd` at an absolute path, for both CLI and MCP | — |
| `viclass` | TypeScript | Migrate its ~27 prep-dependent services from the opaque `preparation` marker list to `preparationCommand`, all sharing one `serializationKey` (see Sharp edges). Every one dispatches through the same `bash scripts/local-services.sh prepare <id>` call keyed on its own case statement — the marker array's *contents* were never read — so the mapping is mechanical. |

Neither `infra`'s nor `viclass`'s commits are this repo's to make.

`lsd mcp install <config-file>...` and `lsd skill install --dest <path>` exist so a consumer doesn't
hand-author a Node wrapper plus hand-edit its MCP client config. `mcp install` resolves the running
binary's own path (`current_exe()`) and merges `{command, args}` into any file matching the standard
`{mcpServers: {name: {…}}}` shape; `skill install` writes a generic, project-agnostic Markdown doc
(`rust/crates/ls-cli/skill/local-services-mcp.md`, embedded via `include_str!`) describing the MCP
tool contract, deliberately carrying no per-consumer catalog or service-list content.

## Maintaining this file

Keep this file for knowledge useful to almost every future agent session in this project.
Do not repeat what the codebase already shows; point to the authoritative file or command instead.
Prefer rewriting or pruning existing entries over appending new ones.
When updating this file, preserve this bar for all agents and keep entries concise.

Specifically: do not record test counts, "clippy-clean", "N/N clean repeated runs", or a
chronological log of what was ported when — all of it goes stale and none of it is actionable.
Record the *rule* a bug taught, not the story of finding it.
