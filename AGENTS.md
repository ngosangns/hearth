# Project agent memory

Project-intrinsic knowledge that should travel with the code: orientation, build/test/release,
architecture, and sharp edges. Add durable notes here as real work discovers them.

## Orientation

The product is the compiled `hearthd` binary (`rust/bin/hearthd`, crates `hearth-core` `hearth-cli` `hearth-tui`
`hearth-mcp`) and the SwiftUI client in `apps/macos`. A project authors `hearth.yaml` (`.yml` /
`.json`); TypeScript catalogs are not accepted.

Consumers: `apps/macos`, `infra`, `viclass` — all spawn the app-bundled binary at
`/Applications/Hearth.app/Contents/Resources/hearthd/bin/hearthd`.

`hearthd manager ensure --json` prints everything (`token`, `port`, `runtimeDirectory`, …) a generic
HTTP+SSE client needs. `env.rs` resolves the daemon's own base environment (login shell + `.env`)
because a GUI-spawned daemon inherits launchd's bare `PATH`.

`SidecarLocator` (`apps/macos`) finds a compiled `hearthd`: env override, bundled copy,
`/Applications` install, known locations, this checkout's `cargo build` output, then the login
shell's PATH. There is no `bun` fallback.

`apps/macos/scripts/build-app.sh` packages an ad-hoc-signed `Hearth.app` with that binary bundled.
Not notarized — distribution to another machine is the one packaging step still missing. See
`apps/macos/README.md`.

## Build, test, release

From `rust/`: `cargo test --workspace` and `cargo clippy --workspace --all-targets -- -D warnings`.
Also `task rust:test` / `task rust:clippy`.

The suite needs a running `docker` daemon and `tailscale`, plus `nc`, `ps`, `sh`.

**Installing/refreshing `hearthd`** — `task rust:install`:
```
cargo build --release -p hearthd && mkdir -p "/Applications/Hearth.app/Contents/Resources/hearthd/bin" && cp target/release/hearthd "/Applications/Hearth.app/Contents/Resources/hearthd/bin/hearthd" && codesign --sign - --force "/Applications/Hearth.app/Contents/Resources/hearthd/bin/hearthd"
```
The ad-hoc re-sign is required after every copy on macOS.

**macOS app.** `task macos:build` / `macos:test` / `macos:package` / `macos:install` (+ `dev` for
the fswatch rebuild loop).

**Release.** CI (`.github/workflows/ci.yml`) runs the Rust test + clippy job on PRs and tags.
There is no npm publish. The binary is installed by hand or bundled into the macOS app.
Pushing a `v*.*.*` tag runs CI, and a successful tag CI run triggers `.github/workflows/release.yml`
(`workflow_run`, so a red tag never publishes; tag/sha come from `github.event.workflow_run`, not
`github.ref`) on the self-hosted runner: `build-app.sh release` → `ditto` zip →
`gh release create --generate-notes` (idempotent — a re-run uploads `--clobber` over the existing
release). The zip is served from the repo's GitHub Releases page (`AppLinks.releases` in the app);
the app is still ad-hoc signed, so first launch elsewhere needs right-click > Open.
`apps/macos`'s workflow is `.github/workflows/macos-app.yml` (path-filtered to `apps/macos/**`)
and runs `swift build` only — its tests need XCTest, which the self-hosted runner lacks.

`PROTOCOL_VERSION` in `rust/crates/hearth-core/src/state.rs` is the protocol-compatibility signal — a
bump there must be treated as breaking for every client.

### The self-hosted runner's environment

The registered `{self-hosted, macmini}` runner (`macmini-hearth`, one of several per-repo
instances under `~/actions-runner-<repo>` on the Mac mini) is a different physical machine
from any dev box here, with the same username.

- **It executes `run:` steps from the runner service's environment, not a login shell**, so a
  toolchain under `~/.cargo/bin` or Homebrew is not on PATH by default. Both workflows add it via
  the composite action `.github/actions/toolchain-path` — keep using it.
- `ci.yml` and `macos-app.yml` skip fork pull requests: the runner shares a user account with
  other repos' runners.
- **Its Rust is managed by rustup, whose stable toolchain does not include clippy**, while dev
  machines here use Homebrew's rust, which bundles it. `ci.yml` adds the component explicitly
  (idempotent).

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
- `ProcessAdapter::inspect` is tri-state (`Inspection::Observed|Gone|Unknown`) — a probe that fails
  to spawn or times out is `Unknown`, and callers must never read it as "gone": a wedged `docker`
  or `ps` would otherwise let `stop` record `stopped` over a running container and `reconcile` mark
  a live service `Failed` (spawning a duplicate). Unknown ⇒ stop/terminate fail loudly, `status`
  and `reconcile` skip, the readiness loop keeps waiting.
- `ProcessSupervisor.shutdown()` does two passes: an "active state" stop pass, then a reap pass for
  daemon-owned services holding a stale POSIX identity in a non-active state (e.g. `externally-owned`
  after a port conflict). Don't collapse these into one.
- A failed container stop must propagate, not be swallowed — the caller transitions the service to
  `stopped` immediately after, which would record a stopped service whose container is still running.
- **A stop must never report success without stopping something.** A service the daemon holds no
  process identity for — an adopted `ownership: external` unit, or one whose port is held by an
  unowned process — is stopped by running the catalog's `stop:` command, and *fails* when the catalog
  declares none. `externally-owned` used to early-return success, and the identity-less fallthrough
  used to `orphan()` (recording "Process ownership identity no longer matches" for a service that
  never had one): both left the container or process running while every UI showed Stop as done.
  The same rule applies to an identity that no longer matches — the process is alive but is not ours
  to kill, so the operation fails rather than silently succeeding.
- **`killUnowned` is the only path that signals a process the daemon does not own.** It is a
  client-supplied flag on `POST /v1/operations` (`action: start` only — every surface rejects it
  otherwise) that must only ever be set after an explicit user confirmation: CLI `--kill-unowned`
  or its TTY `[y/N]` prompt (non-TTY always answers no), TUI's two-keypress arm-then-confirm, the
  app's "Kill & Start" dialog, MCP's `killUnowned` argument. `ProcessSupervisor::reclaim_port`
  resolves listeners via `ProbeAdapter::port_holders` (lsof) and signals **individual pids** via
  `ProcessAdapter::signal_pid` — SIGTERM, poll, then a re-resolved SIGKILL pass. Never `killpg` an
  unowned holder: its group membership is untrusted (a shared job can hold innocent siblings; pgid
  1 must never be signalled). `signal_pid` re-verifies the resolved `lstart` before sending, so a
  pid recycled between resolve and signal is never killed. `None` from `port_holders` means
  "cannot resolve" — the reclaim fails closed rather than guessing a pid.

**Daemon / state**

- Fire-and-forget work (unit log forwarding, `syncExternalServices` polling) must never panic into
  the daemon. Route it through `record_background_error`.
- A daemon whose lock was taken over stops itself (`LockOwnershipWatch`) and exits without touching
  the winner's lock. Enforced from the losing side so two daemons can never fight over one
  `state.json`.
- `reloadCatalog` stops a removed-but-active service using the **old** catalog and only swaps the
  catalog afterward — the supervisor needs the old definition to know how to stop it.
- A persisted `externally-owned` (from a port held by an unowned process) is only re-evaluated on
  the next `start` — `sync_external_services` only polls `ownership: external` units, and `cleanup`
  does not touch it, so a dead squatter leaves a phantom "Port N is held" row in the UI forever.
  The same blind spot hits a **renamed-ownership id**: removing a `shared:` key and re-adding it as
  a `services:` entry keeps the old row (`adopted from external state`, `command` readiness) across
  restarts — the new daemon-owned service never installs or spawns because `start` short-circuits
  on the persisted `ready`. Fix: `manager stop`, delete the service's `state.json` row, restart.
  Manual reconcile: stop the daemon, then rewrite `state.json` `services` to `{}` (or delete the
  stale entries) — editing while the daemon lives is overwritten on shutdown.
  Squatters hide well: catalog `run:` commands like `exec node dist/main` leave a **relative**
  cmdline, so a path-based `ps` grep misses them — find holders by listening port (`lsof -iTCP:N
  -sTCP:LISTEN`) or cwd (`lsof -d cwd`), and remember non-TCP binds too (viclass `syncer` panics on
  **UDP** 50000, invisible to a TCP-only scan).
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
- A service's `artifact:` block (`version` + `url`|`script` + `sha256` for url) installs a
  versioned tarball into `<runtimeDir>/installs/<id>/<version>` on every start before preparation
  and build — the same marker-based machinery `shared/install.rs` uses for smp instances, via the
  `ArtifactInstaller` option (`DefaultArtifactInstaller` is wired for file-loaded catalogs;
  `None` → the start fails clearly). `{installDir}`/`{dataDir}`/`{port}`/`{portN}`/`{serviceId}`/
  `{projectRoot}` are rendered into commands/env/urls at **catalog load**, not spawn — the served
  catalog always carries literal paths. Only those var names are substituted — other braces
  (`awk '{print}'`) are legal, and a `{port}` with no declared port fails the load.
- Service order from a config file is **document order**, not sorted — it's user-visible in
  `/v1/catalog`, `hearthd status`, and both TUIs.
- `groups:` members may name other groups — flattened depth-first (declaration order, deduplicated)
  into `ServiceCatalog.groups` for target resolution; the declared membership stays in
  `group_tree` (`groupTree` on the wire) so the app can group by *direct* membership instead of
  showing a giant `all` section. A member naming both a service and a group resolves as the
  service. Cycles are a load error, not truncation.
- `disabled: true` rejects direct start/stop/restart (`service_disabled`, `status` still works) and
  is stripped from flattened `groups` — group ops skip it silently; `group_tree` still lists it for
  display. It does not stop an already-running service — the row just stops being actionable.
- A missing/invalid `readiness` must fail the load, not skip the service.

**Shared services (`smp`)**

- A top-level `shared:` map in `hearth.yaml` (`postgres: "16.4"`) expands into generated
  `ownership: external` services (`config_file.rs` → `shared::synthesize::project_service_entry`):
  run = one-shot `hearthd shared attach <name@ver>` task, readiness = `hearthd shared probe`
  (exit 0 iff the instance is ready **and this project is attached**), stop = `hearthd shared
  detach`. `projectId` = sha256 of the canonicalized cwd, so attach/probe/stop commands all agree
  by running with the project root as cwd. The object form (`{ version, preparationCommand,
  attachArgs, urls }`) adds a project-side prep hook before every attach and trailing attach
  args that land on the recipe's `provision` argv — how viclass/infra publish their nginx conf
  trees into the shared instance.
- The task semantics depend on a supervisor rule: `ownership: external` + `command` readiness ⇒
  the run command is a one-shot trigger, not the service process (`is_external_task`). A
  daemon-owned `command`-readiness service is still a normal long-lived process — don't widen it.
- **`smp` = `hearthd smp`** — the same binary, rooted at `~/.hearth/shared`, catalog *synthesized*
  from `registry.json` (single writer: the smp daemon; CLIs only read). Runtime dir is
  `~/.hearth/shared/runtime-v1`, so `discover`/`ensure` work unchanged against that root.
- Ports: `sha256(name@version)` into `43100–43999`, collision probes forward and persists into
  `registry.json`. Never hand out a well-known port (5432/6379) to a shared instance — the one
  sanctioned exception is a recipe's `ports: […]` **pinned** field (the shared nginx's `[80, 443]`),
  which skips hashing entirely and only bind-checks. For public :80/:443 on macOS there is no
  privileged-bind workaround for userspace — the shared nginx pins `18080/18443` and a pf rdr
  anchor (`com.apple/hearth`, loaded once with sudo `pfctl -a … -f - && pfctl -e`) maps the
  public ports onto them. A recipe's
  `additionalPorts` reserves that many extra ports in one contiguous block (`{port2}`, …); every
  port in the block is taken. `prepare` is the idempotent init hook (runs before every start via
  `preparation_command`). `run` must stay the process `ps` keeps showing. A wrapper that does work
  and then `exec`s is adopted as the wrapper and orphaned once the real server replaces it. A
  server that rewrites its own title (nginx's `nginx: master process …`) has to be started with
  `shell` + `exec: true` so the stored identity is that settled line, not the pre-title argv.
  `{projectBucket}` is `h-<projectId>` — S3 bucket names reject the underscore in `{projectDb}`.
- Install = tarball + sha256 into `installs/<name>/<version>` (atomic rename; `.hearth-installed`
  marker last). The catalog artifact is either a `url` (download that archive) or a `script`
  (run it to write the archive). **sha256 is verified only for `url` artifacts** — script output is
  never byte-reproducible (tar mtimes, compile variance), so no committed hash can match a
  consumer's rebuild; the script itself is the trust boundary. Recipe snapshots live in
  `registry.json`, so installed instances
  survive upstream removal from `catalog.json`. `catalog.json` at the repo root is the registry the pinned
  `SHARED_CATALOG_URL` serves; `HEARTH_SHARED_CATALOG_URL` overrides it for dev/tests (`file://`
  works for both the catalog and artifact URLs). Catalog URL order: env →
  `~/.hearth/shared/catalog-url` → pinned URL — the file exists because a GUI-launched `smp` daemon
  doesn't inherit shell env. `remote.rs` also embeds the same `catalog.json` (`include_str!`) as the
  final fallback — a stale disk cache is topped up with any recipe the embedded catalog has that the
  cache lacks, so recipes added in newer `hearthd` builds still reach machines that cached an old
  `catalog-url`/`file://` document.
- Shared MongoDB runs `--replSet rs0` (consumers use transactions — e.g. engreel's
  `WithTransaction`) and `--wiredTigerCacheSizeGB 0.25`; its `provision` payload initiates rs0
  idempotently then writes the project-db marker, which is why the recipe needs the repacked
  mongod+mongosh tarball rather than the bare upstream archive.
- `POST /v1/shared/attach` blocks through install+start+provision — clients must not use the 10s
  manager timeout for it (`request_with_timeout` with `None`).
- `POST /v1/shared/remove` refuses (409) an instance that still has project attachments unless
  `force: true` — it deletes every attached project's data. `hearthd shared remove --force` and the
  app's attachment-counting confirmation dialog are the sanctioned confirmations, same contract as
  `killUnowned`. `POST /v1/manager/reload` likewise fails with 409 `stop_failed` and keeps the old
  catalog when a removed-but-active service can't be stopped. `hearthd shared stop`/`start` exit
  non-zero when the operation fails. `hearthd doctor` checks the suite's host tools (docker,
  tailscale, nc, ps, sh) on top of platform + catalog validation.

**Consumers and the `hearthd` binary**

- Consumer scripts must use the absolute path
  `/Applications/Hearth.app/Contents/Resources/hearthd/bin/hearthd`.
- **Never assume tools are on `PATH`**: a Dock/Finder-launched GUI process gets launchd's bare
  `/usr/bin:/bin:/usr/sbin:/sbin`. The daemon inherits that, so `hearthd` appends Homebrew and
  `~/.bun`/`~/.cargo` bin dirs (`with_known_tool_directories`) — otherwise `docker compose`,
  `tailscale serve status` and `tailscale status` fail to spawn. Test anything the app spawns under
  `env -i PATH=/usr/bin:/bin:/usr/sbin:/sbin`.
- `serde_json`'s `preserve_order` feature is on workspace-wide and is load-bearing: without it
  `Value`'s object type is a `BTreeMap`, and `hearthd mcp install` silently alphabetizes every key in any
  hand-maintained config file it touches.
- `hearthd mcp install --key <name>` exists because infra's registry uses `servers`, not the standard
  `mcpServers`. The merge only ever touches `command`/`args`.

**macOS app (apps/macos)**

- **Single instance is enforced in `AppDelegate`**, not just `LSMultipleInstancesProhibited`:
  `open -n` and `scripts/dev.sh`'s bare-binary launch bypass LaunchServices. The check matches bundle
  ID, falling back to executable path when there is no bundle; the loser posts a
  `DistributedNotificationCenter` show-window notification to the winner and terminates. Reopening a
  closed main window goes through `MainWindow.open` — the `openWindow` action captured from
  `ContentView` — driven by `applicationShouldHandleReopen` and that notification.

- The daemon's own log is the pinned `daemon log` row — pseudo-id `$daemon` (`$` can't collide with
  a real service id), fetched from `GET /v1/daemon/log` rather than `/v1/logs/:id`, so it survives
  catalog reloads and is exempt from the apply-services prune.
- The app's `displayState` collapse splits in-flight states the CLI's `text_state` does not:
  `running`/`running-unready` → `running`, `starting`/`preparing` → `starting` (CLI prints both as
  `running`). `ready` and `running` both count in the "ready/total" summaries — a `kind: process`
  service sits in `running` forever and must not read as a stuck boot.

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
- `HearthManager::shutdown_completion()` is a `watch` value, and `watch::Sender::send` is a **no-op
  when no receiver exists yet**. A test that subscribes *after* triggering a shutdown can wait
  forever on a shutdown that already finished — subscribe first, then trigger. (The existing
  `stop-services` test only passes because stopping a service takes long enough to lose that race.)

**Rejected approaches (don't re-litigate)**

- A `bun build --compile` sidecar: on the `self-hosted, macmini` machine a freshly compiled, ad-hoc
  signed Bun executable is SIGKILLed on launch. The same test with a trivial Rust binary passed,
  which is why the product is the Rust `hearthd` binary.

## Notes on the Rust implementation

- **No `Custom` readiness variant.** A closure can't cross the YAML/JSON boundary.
- **SSE backpressure is frame-count only** (64 frames).
- **`crossterm` + `unicode-width`** for the TUI. `hearth-tui`'s `run.rs` has no automated coverage (it
  owns a real terminal).
- **`truncateToWidth`/`visibleWidth`/`DEFAULT_TAB_WIDTH` were reverse-engineered** against pi-tui's
  native addon. The tab width is a fixed 3-space replacement. See `truncate_to_width`'s doc comment.
- **`hearth-mcp` hand-implements `ServerHandler`** rather than using `rmcp`'s `#[tool]` macros: names
  carry a runtime-configurable prefix and schemas embed the caller's `knownServiceIds`.
- **`hearthd mcp` and `hearthd tui` are intercepted in the binary**, not in `hearth-cli`, to avoid a crate cycle.
  `hearthd mcp install` / `hearthd skill install` live in `hearth-cli`.
- **`state.json` timestamp validation is a non-empty-string check.**

## Maintaining this file

Keep this file for knowledge useful to almost every future agent session in this project.
Do not repeat what the codebase already shows; point to the authoritative file or command instead.
Prefer rewriting or pruning existing entries over appending new ones.
When updating this file, preserve this bar for all agents and keep entries concise.

Record the *rule* a bug taught, not the story of finding it.
