# Project agent memory

Project-intrinsic knowledge that should travel with the code: orientation, build/test/release,
architecture, and sharp edges. Add durable notes here as real work discovers them.

## Orientation

The product is the compiled `lsd` binary (`rust/bin/lsd`, crates `ls-core` `ls-cli` `ls-tui`
`ls-mcp`) and the SwiftUI client in `apps/macos`. A project authors `local-services.yaml` (`.yml` /
`.json`); TypeScript catalogs are not accepted.

Consumers: `apps/macos` (bundled sidecar), `infra`, `viclass` — all spawn the app-bundled binary at
`/Applications/Local Services.app/Contents/Resources/lsd/bin/lsd`.

`lsd manager ensure --json` prints everything (`token`, `port`, `runtimeDirectory`, …) a generic
HTTP+SSE client needs. `env.rs` resolves the daemon's own base environment (login shell + `.env`)
because a GUI-spawned daemon inherits launchd's bare `PATH`.

`SidecarLocator` finds a compiled `lsd`: env override, bundled copy, `/Applications` install,
known locations, this checkout's `cargo build` output, then the login shell's PATH. There is no
`bun` fallback.

`scripts/build-app.sh` packages an ad-hoc-signed `Local Services.app` with that binary bundled. Not
notarized — distribution to another machine is the one packaging step still missing. See
`apps/macos/README.md`.

## Build, test, release

From `rust/`: `cargo test --workspace` and `cargo clippy --workspace --all-targets -- -D warnings`.
Also `task rust:test` / `task rust:clippy`.

The suite needs a running `docker` daemon and `tailscale`, plus `nc`, `ps`, `sh`.

**Installing/refreshing `lsd`** — `task rust:install`:
```
cargo build --release -p lsd && mkdir -p "/Applications/Local Services.app/Contents/Resources/lsd/bin" && cp target/release/lsd "/Applications/Local Services.app/Contents/Resources/lsd/bin/lsd" && codesign --sign - --force "/Applications/Local Services.app/Contents/Resources/lsd/bin/lsd"
```
The ad-hoc re-sign is required after every copy on macOS. Do not install into Homebrew — that name
collides with the `lsd` (lsdeluxe) formula.

**macOS app.** `task macos:build` / `macos:test` / `macos:package` / `macos:install`.

**Release.** CI (`.github/workflows/ci.yml`) runs the Rust test + clippy job on PRs and tags.
There is no npm publish. The binary is installed by hand or bundled into the macOS app.
`apps/macos`'s workflow is `.github/workflows/macos-app.yml` (path-filtered to `apps/macos/**`) and
runs `swift build` only.

`PROTOCOL_VERSION` in `rust/crates/ls-core/src/state.rs` is the protocol-compatibility signal — a
bump there must be treated as breaking for every client.

### The self-hosted runner's environment

The registered `{self-hosted, macmini}` runner (`ngosangns-Mini`) is a different physical machine
from any dev box here, with the same username.

- **It executes `run:` steps from the runner service's environment, not a login shell**, so a
  toolchain under `~/.cargo/bin` or Homebrew is not on PATH by default. `ci.yml` resolves cargo's
  directory into `$GITHUB_PATH` itself — keep that in the workflow.
- **Its Rust is managed by rustup, whose stable toolchain does not include clippy**, while dev
  machines here use Homebrew's rust, which bundles it. `ci.yml` adds the component explicitly
  (idempotent).
- **Its Swift toolchain has no XCTest** (Command Line Tools only, no full Xcode.app), so
  `apps/macos/Tests/` passes locally but is not in CI. Don't re-add `swift test` to
  `macos-app.yml` without first fixing the runner's Xcode install.

## Sharp edges

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

- Fire-and-forget work (unit log forwarding, `syncExternalServices` polling) must never panic into
  the daemon. Route it through `record_background_error`.
- A daemon whose lock was taken over stops itself (`LockOwnershipWatch`) and exits without touching
  the winner's lock. Enforced from the losing side so two daemons can never fight over one
  `state.json`.
- `reloadCatalog` stops a removed-but-active service using the **old** catalog and only swaps the
  catalog afterward — the supervisor needs the old definition to know how to stop it.
- Never build a client-facing state string with `format!("{:?}", state)` — `Debug` gives
  `QueuedStart`, the wire encoding is `queued-start`. Use `ActualServiceState::as_wire_str()` /
  `ReadinessKind::as_wire_str()`, which are pinned to the serde encoding by test.
- `state.json` quarantines rather than trusts: a shape check that passes but then fails to
  deserialize must fall through to quarantine, never panic — a panic in `load()` kills the daemon at
  bootstrap, the opposite of what quarantining exists for.
- A `reset` from the event store returns the **whole** buffer, not an empty list. `reset` means the
  client's cursor is unusable, so the reply is the snapshot it resynchronizes from.

**Catalog**

- `{ kind: "command" }` readiness is the JSON-serializable stand-in for a custom probe. Exit 0 means
  ready; anything else keeps retrying until the readiness timeout.
- `preparationCommand` (with `serializationKey`) runs via the same `command` adapter. Services
  sharing a key run their preparation one at a time. **`viclass`'s prep-dependent services must all
  share one key**: `ensure_local_certificates` is a check-then-generate race, and concurrent
  first-time prepares would corrupt the cert file.
- `ownership: 'external'` is the docker/tailnet-task adoption carve-out.
- Service order from a config file is **document order**, not sorted — it's user-visible in
  `/v1/catalog`, `lsd status`, and both TUIs.
- A missing/invalid `readiness` must fail the load, not skip the service.

**Consumers and the `lsd` binary**

- Consumer scripts must use the absolute path
  `/Applications/Local Services.app/Contents/Resources/lsd/bin/lsd`.
- **Never assume tools are on `PATH`**: a Dock/Finder-launched GUI process gets launchd's bare
  `/usr/bin:/bin:/usr/sbin:/sbin`. The daemon inherits that, so `lsd` appends Homebrew and
  `~/.bun`/`~/.cargo` bin dirs (`with_known_tool_directories`) — otherwise `docker compose`,
  `tailscale serve status` and `tailscale status` fail to spawn. Test anything the app spawns under
  `env -i PATH=/usr/bin:/bin:/usr/sbin:/sbin`.
- `serde_json`'s `preserve_order` feature is on workspace-wide and is load-bearing: without it
  `Value`'s object type is a `BTreeMap`, and `lsd mcp install` silently alphabetizes every key in any
  hand-maintained config file it touches.
- `lsd mcp install --key <name>` exists because infra's registry uses `servers`, not the standard
  `mcpServers`. The merge only ever touches `command`/`args`.

**Testing gotchas**

- The macOS app's controllers take `any ManagerAPI`, not the concrete `ManagerClient`, so
  `FakeManagerAPI` can drive them without a daemon. Prefer stepping `refresh()`/`fetchOnce()`
  directly over waiting on the real poll timer. The model is `ManagerOperation`, not `Operation`
  — the latter shadows `Foundation.Operation`.
- **A fixed sleep waiting on another process is a flake.** Poll for the actual condition with a
  generous deadline instead.
- Right after spawning, a child can still be mid-`execve`, and macOS `ps` reports a parenthesized
  placeholder (`(sh)`). Never treat the first readable `ps` row as a just-spawned process's
  authoritative fingerprint — see `accepts_spawn_observation`.
- A spawned test shell needs its own process group (`process_group(0)` / `setpgid(0,0)`), or it
  inherits the cargo-test harness's pgid and the test's `killpg` calls signal the whole test run.
- `nc -l <port>` without `-k` exits after accepting one connection — and the TCP readiness probe's
  own `connect()` **is** that connection, so the service goes ready then immediately failed.
- An in-process `rmcp` client/server pair deadlocks if both `.serve()` calls are awaited
  sequentially: drive them with `tokio::join!`.

**Rejected approaches (don't re-litigate)**

- A `bun build --compile` sidecar: on the `self-hosted, macmini` machine a freshly compiled, ad-hoc
  signed Bun executable is SIGKILLed on launch. The same test with a trivial Rust binary passed,
  which is why the product is the Rust `lsd` binary.

## Notes on the Rust implementation

- **No `Custom` readiness variant.** A closure can't cross the YAML/JSON boundary.
- **SSE backpressure is frame-count only** (64 frames).
- **`crossterm` + `unicode-width`** for the TUI. `ls-tui`'s `run.rs` has no automated coverage (it
  owns a real terminal).
- **`truncateToWidth`/`visibleWidth`/`DEFAULT_TAB_WIDTH` were reverse-engineered** against pi-tui's
  native addon. The tab width is a fixed 3-space replacement. See `truncate_to_width`'s doc comment.
- **`ls-mcp` hand-implements `ServerHandler`** rather than using `rmcp`'s `#[tool]` macros: names
  carry a runtime-configurable prefix and schemas embed the caller's `knownServiceIds`.
- **`lsd mcp` and `lsd tui` are intercepted in the binary**, not in `ls-cli`, to avoid a crate cycle.
  `lsd mcp install` / `lsd skill install` live in `ls-cli`.
- **`state.json` timestamp validation is a non-empty-string check.**

## Maintaining this file

Keep this file for knowledge useful to almost every future agent session in this project.
Do not repeat what the codebase already shows; point to the authoritative file or command instead.
Prefer rewriting or pruning existing entries over appending new ones.
When updating this file, preserve this bar for all agents and keep entries concise.

Record the *rule* a bug taught, not the story of finding it.
