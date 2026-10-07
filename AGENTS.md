# Project agent memory

Project-intrinsic knowledge that should travel with the code: orientation, build/test/release,
architecture, and sharp edges. Add durable notes here as real work discovers them.

## Orientation

The product is the compiled `hearth` binary (`rust/bin/hearth`, crates `hearth-core` `hearth-cli` `hearth-tui`
`hearth-mcp`). `hearth tui` is the workspace UI. 0.18.0 renamed the command from `hearthd`; do not
install or publish `hearthd`. A project authors `hearth.yaml`
(`.yml` / `.json`); TypeScript catalogs are not accepted.

Consumers (`infra`, `viclass`) spawn `hearth` from `~/.local/bin/hearth` after `task install`.

`hearth manager ensure --json` prints everything (`token`, `port`, `runtimeDirectory`, …) a generic
HTTP+SSE client needs. `env.rs` resolves the daemon's own base environment (login shell + `.env`)
because a process started outside a login shell inherits launchd's bare `PATH`.

`hearth-core` manager HTTP surface lives under `rust/crates/hearth-core/src/manager/http/`:
`mod.rs` owns `HearthManager` / bootstrap / shutdown; `routes.rs` is the axum route table
(handlers stay a child module so they can use private manager fields without widening crate visibility).

`OperationScheduler` serializes per service id. A bulk start locks every id in
`target_service_ids` (sorted, to avoid deadlock). Disjoint bulks run in parallel; overlapping
ones wait only on the shared services. `__manager__` is reserved for manager-wide work such as
shutdown — not for ordinary multi-service start/restart.

`ShutdownMode` (`LeaveServices` | `StopServices`) in `manager/protocol.rs` replaces the old `stop_services: bool` on manager/daemon shutdown.

`ProcessSupervisor` lives under `supervisor/engine/` (`mod.rs` + `tests.rs`); adapters remain in `default_adapters.rs`. A deeper owned-vs-external backend split is still deferred.

## Build, test, release

From `rust/`: `cargo test --workspace` and `cargo clippy --workspace --all-targets -- -D warnings`.
Also `task test` / `task clippy`.

The suite needs a running `docker` daemon and `tailscale`, plus `nc`, `ps`, `sh`.

A test that boots a `HearthManager` in-process must hold `StopServicesOnDrop(manager.clone())`
for its whole body. `manager.close()` is a leave-services shutdown, and a panic skips the explicit
shutdown, so either way its real services (`exec nc -lk {port}`) outlived the test binary under
launchd, about six per `cargo test --workspace`. A clean full run leaves no `nc -lk` behind; check
with `ps -axo ppid,command | grep 'nc -lk'` before and after.

**Installing/refreshing `hearth`** — `task install` builds the release binary into
`~/.local/share/hearth/bin/hearth-<version>`, ad-hoc signs that file (never the live path), and
points `~/.local/bin/hearth` at it with a symlink. `hearth update` downloads the GitHub asset
`hearth-<tag>` into the same layout after checking the tag, asset name, `github.com` download
URL, size, `sha256` digest, executable bit, and a staged `--version` smoke test. The asset
request uses `Accept: application/octet-stream` and follows only HTTPS redirects onto `github.com`,
`api.github.com`, `release-assets.githubusercontent.com`, `objects.githubusercontent.com`, or
`github-releases.githubusercontent.com`. A bearer token is sent only to `https://github.com` and
`https://api.github.com`. The `~/.local/bin/hearth` inode is checked again immediately before the
symlink swap, and a file already rotated into place is moved back if that check or the swap fails.
It does not re-sign the download: `codesign --force` on a mapped ad-hoc binary SIGKILLs that
process, and so does writing over its inode. The previous file stays for one generation. A daemon already running
keeps its old inode until `hearth --root <project> manager restart` (services stay up); restart
smp separately. The command refuses any current executable that is not that symlink's regular
file or a file already in the versioned directory, refuses anything but darwin-arm64, and does
not replace an equal version unless `--force`. It does not downgrade.

`task install` rebuilds only the leaf `hearth` bin when workspace code changes, and that
crate's release codegen was the install bottleneck: the bin instantiates a cross-crate-
inlinable copy of every reachable callee (~2,700 functions, ~74 MB of LLVM IR; `main`
alone ~110k lines), so at O3 LLVM's inliner ran ~5 min in a single codegen unit — more
`codegen-units` does not help. `rust/Cargo.toml` pins `[profile.release.package.hearth]
opt-level = 2` (bin-only ~47 s; O1 was slower than O2). Deps keep O3. Do not "fix" it
back to O3 without re-measuring.

**Release.** CI (`.github/workflows/ci.yml`) runs the Rust test + clippy job on PRs and tags
(`timeout-minutes: 90`; tool `cargo`), and is also `workflow_call`-able with an optional `ref`
input so other workflows can gate on it. Pushing a `v*.*.*` tag runs CI, and a successful tag CI
run triggers `.github/workflows/release.yml` (`workflow_run`, so a red tag never publishes;
tag/sha come from `github.event.workflow_run`, not `github.ref`) on the self-hosted runner
(`timeout-minutes: 60`). The build + ad-hoc `codesign` + `gh release create --generate-notes`
step is the composite action `.github/actions/release-asset` (idempotent — a re-run uploads
`--clobber` over the existing `hearth-<tag>` asset). The version string lives only in
`rust/bin/hearth/Cargo.toml` (and the matching `hearth` entry in `rust/Cargo.lock`).

The hands-off path is `.github/workflows/publish.yml` (`workflow_dispatch`):
`gh workflow run publish.yml -f version=0.19.0`, or Actions → Publish → Run workflow from `main`.
It bumps `Cargo.toml`/`Cargo.lock`, pushes a `release v*` commit to main, calls `ci.yml` on that
sha, then pushes the `v` tag and publishes. It is self-contained because a `GITHUB_TOKEN` push
fires no push events — a token-pushed tag can never reach `release.yml`'s `workflow_run` trigger.
Dispatching a version whose tag already exists republishes that tag (`--clobber`); equal or lower
versions are refused, matching `hearth update`'s no-downgrade rule. The bump commit is pushed
with the `RELEASE_TOKEN` repo secret when present (a PAT can push to a protected `main`), else
`GITHUB_TOKEN`.

`PROTOCOL_VERSION` in `rust/crates/hearth-core/src/state.rs` is the protocol-compatibility signal — a
bump there must be treated as breaking for every client.

### The self-hosted runner's environment

The registered `{self-hosted, macmini}` runner (`macmini-hearth`, one of several per-repo
instances under `~/actions-runner-<repo>` on the Mac mini) is a different physical machine
from any dev box here, with the same username.

- **It executes `run:` steps from the runner service's environment, not a login shell**, so a
  toolchain under `~/.cargo/bin` or Homebrew is not on PATH by default. Both workflows add it via
  the composite action `.github/actions/toolchain-path` — keep using it.
- `ci.yml` skips fork pull requests: the runner shares a user account with other repos' runners.
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
  The tree also holds every member of the leader's own group (a double fork reparents to
  launchd but stays in the group), and every descendant the 200 ms sampler (`last_tree`) saw that
  is still alive with the same `lstart` — how a `setsid` escapee is still stopped. When the leader
  exits on its own, its leftover children are reaped the same way, including those in its group;
  that group is only signalled while a snapshotted member of it is still alive.
- Helper commands (`run_command`, builds, `shared::output_with_timeout`) end when the leader
  exits **and** the output pipes close. A pipe still open one second after the leader exits is a
  leftover child: SIGKILL the group. The group guard stays armed until then — a reaped leader does
  not mean the group is gone (`capture_command`'s 5s timeout kills the group for the same reason).
  A child that redirected its output is left alone.
- Anything the daemon must kill when it exits lives in a destructor (`KillGroupOnDrop`,
  `kill_on_drop`), so `main` shuts the runtime down before `process::exit`. A leave-services
  shutdown still calls `detach_all_output` so `docker logs` followers do not outlive the daemon.
- An adopted process that is still alive is kept. A failing probe leaves it `running-unready`
  and the 1.5s loop keeps checking; start does not kill it. Restart replaces it. A process that
  is already gone is spawned again.
- `ProcessAdapter::inspect` is tri-state (`Inspection::Observed|Gone|Unknown`) — a probe that fails
  to spawn or times out is `Unknown`, and callers must never read it as "gone": a wedged `docker`
  or `ps` would otherwise let `stop` record `stopped` over a running container and `reconcile` mark
  a live service `Failed` (spawning a duplicate). Unknown ⇒ stop/terminate fail loudly, `status`
  and `reconcile` skip, and the continuous probe keeps running.
- `ProcessSupervisor.shutdown()` (only `stop-services`, i.e. `hearth manager stop`) does three
  passes: an "active state" stop pass for daemon-owned services, then the project's
  `ownership: external` services that are up and declare a `stop:` command (a `shared:` entry's
  `hearth shared detach` gets `--stop-if-unused`, so smp stops the instance only when no other
  project is attached — `shutdown_stop_command`), then a reap pass for daemon-owned services
  holding a stale POSIX identity in a non-active state (e.g. `externally-owned` after a port
  conflict). Don't collapse these into one, and keep the external pass after the first so apps
  release a shared database before it goes. `manager restart` and SIGTERM never run it.
- A failed container stop must propagate, not be swallowed — the caller transitions the service to
  `stopped` immediately after, which would record a stopped service whose container is still running.
- **A stop must never report success without stopping something.** A service the daemon holds no
  process identity for — an adopted `ownership: external` unit, or one whose port is held by an
  unowned process — is stopped by running the catalog's `stop:` command, and *fails* when the catalog
  declares none. `externally-owned` used to early-return success, and the identity-less fallthrough
  used to `orphan()` (recording "Process ownership identity no longer matches" for a service that
  never had one): both left the container or process running while every UI showed Stop as done.
  An `orphaned` POSIX row (the command line or manager instance no longer matches) is still stopped
  when its pid **and** start time (`ps` lstart) match the recorded identity: that is the same process,
  typically a shell that `exec`ed into its server after the spawn settled. It gets the owned stop's
  escalation (`terminate_posix_tree`: leader group plus sampled descendant groups, TERM, grace,
  KILL), and restart's stop phase and the `stop-services` shutdown pass take the same path. A
  different start time is a reused pid, and a Docker identity or one recorded for another
  service/generation is not ours either: those still refuse with "no longer owned by this manager",
  rather than silently succeeding. Status and reconcile still *mark* such a row `orphaned` (the
  ownership check is unchanged); only stop verifies around it. A finished `readiness: exit`
  row is the exception: stop succeeds immediately and leaves `succeeded` or `failed` in place,
  because there is no process left. Do not send that row through `stop_unowned`.
- Restart is not that stop. A row with no process identity and no catalog `stop` command used to
  fail inside stop, so restarting a group only showed the verb and never started the members.
  Restart skips that refusal and starts. When the TCP listener is this service — the `exec` target
  in its directory, or the install directory still in the command line — restart records that
  identity and replaces the process with the owned process-tree stop before starting. A different
  program is not signalled; start reports the port is held. Stop of the same row still fails.
  `hearth stop` / `hearth restart` of a group waits for each member, so the first failure still
  stops the rest; `--wait` only makes a single service block. In the TUI, group stop and restart
  wait for every member and the notice names a failure.
- **`killUnowned` is the only path that signals an arbitrary process holding a service port.** It is a
  client-supplied flag on `POST /v1/operations` (`action: start` only — every surface rejects it
  otherwise) that must only ever be set after an explicit user confirmation: CLI `--kill-unowned`
  or its TTY `[y/N]` prompt (non-TTY always answers no), TUI's two-keypress arm-then-confirm, and
  MCP's `killUnowned` argument. `ProcessSupervisor::reclaim_port`
  resolves listeners via `ProbeAdapter::port_holders` (lsof) and signals **individual pids** via
  `ProcessAdapter::signal_pid` — SIGTERM, poll, then a re-resolved SIGKILL pass. Never `killpg` an
  unowned holder: its group membership is untrusted (a shared job can hold innocent siblings; pgid
  1 must never be signalled). `signal_pid` re-verifies the resolved `lstart` before sending, so a
  pid recycled between resolve and signal is never killed. `None` from `port_holders` means
  "cannot resolve" — the reclaim fails closed rather than guessing a pid.
- `manager restart` reaps every other live daemon for that same root before starting the new one.
  A project root matches `hearth daemon --root <path>` (either argument order; the path may contain
  spaces; a longer path is not a match). `~/.hearth/shared` matches `hearth smp`. The reap signals
  those pids only — SIGTERM, then SIGKILL — never the process group, so detached services keep
  running. A service `restart` reaps host processes in the service cwd whose command fingerprint
  is the logical command or the last stored identity, or whose argv0 is that service's absolute
  argv0 when the binary is a dedicated server and no other catalog service uses it from the same
  directory. Dedicated means not `hearth` and not a shared interpreter (`node`, `python`, `java`,
  `ruby`, `perl`, a shell, `bun`, `deno`). A start adopts that absolute-argv0 process; the pid
  already stored stays owned on argv0 even when another service shares the binary. A shell
  command stays on the settled fingerprint (`shell` + `exec`). Each match's ppid tree is
  signalled per pid. A process that only shares the port stays on `killUnowned`. If `ps` cannot
  be read, restart fails closed; a start skips the adopt and continues.
- A daemon spawned by `ensure` stays a child of that process (`process_group` does not reparent).
  `kill(pid, 0)` is true for the zombie left when that child exits. `is_pid_alive` must reap an
  exited child and treat any other zombie as dead — otherwise `manager restart` sits on
  "restarting daemon…" for the 300s stop timeout, and `claim_lock` spins. The spawn path also
  waits the child in the background so it does not linger as a zombie. Do not weaken a *running*
  pid into "dead" because a health check timed out.

**Daemon / state**

- Fire-and-forget work (unit log forwarding, `syncExternalServices` polling) must never panic into
  the daemon. Route it through `record_background_error`.
- A daemon whose lock was taken over stops itself (`LockOwnershipWatch`) and exits without touching
  the winner's lock. Enforced from the losing side so two daemons can never fight over one
  `state.json`.
- `reloadCatalog` stops a removed-but-active service using the **old** catalog and only swaps the
  catalog afterward — the supervisor needs the old definition to know how to stop it.
- A persisted `externally-owned` row is re-checked when the daemon reconciles, including startup.
  A free TCP port with desired `running` becomes `failed` ("Managed process is no longer alive")
  and is not started again; desired `stopped` becomes `stopped` and the error is cleared. A holder
  that is this service — the `exec` target running in this service's directory, or a command line
  that still contains the `cd && exec build/install/.../bin/...` install directory — is adopted and
  probed. A different program stays `externally-owned` until a confirmed `killUnowned` start.
  `None` from the port probe leaves the row unchanged. `sync_external_services` still only polls
  `ownership: external` units.
  A **renamed-ownership id** is a separate blind spot: removing a `shared:` key and re-adding it as
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
  client's cursor is unusable, so the reply is the snapshot it resynchronizes from. The SSE stream
  copies that replay and registers the listener under the same lock, and holds live events until
  the snapshot frames are queued. A reset does not subscribe. Backpressure stays 64 frames.
- **SSE is invalidation, not the source of truth for service rows.** `GET /v1/services` is
  authoritative. The TUI treats `service.lifecycle`, `manager.catalog-reloaded`, and
  `operation.accepted`/`operation.updated` as a nudge to re-fetch that snapshot; `service.log`
  only refreshes the log cursor. Do not reconstruct full lifecycle state from event payloads alone.
- The log `generation` clients echo is `lifecycle * 1_000_000 + rotation`, not the lifecycle alone.
  A rotation resets a follower the same way a restart does. Generation 0 is valid (no state row).

**Catalog**

- `{ kind: "command" }` readiness is the JSON-serializable stand-in for a custom probe. Exit 0 means
  ready; anything else is `running-unready`. The probe repeats every 1.5s (`readiness_backoff_ms`)
  until stop, restart, process exit, or daemon shutdown. It does not fail the service.
  `readinessTimeoutMs` is still parsed. It no longer ends a long-lived start. It bounds one
  command probe, and the one-shot shared-attach wait. `hearth start --wait`, bulk start, and MCP
  `manage` settle at `ready` or `running-unready` once the process is up. `status` does not probe.
- `{ kind: "exit" }` means the run command is the job. While it runs the state is `running` — not
  `ready` and not `running-unready`. Exit 0 records `succeeded` and sets desired back to `stopped`,
  so reconcile does not run it again; any other exit is `failed` with desired `stopped` (no
  auto-restart). `readinessTimeoutMs` does not apply: a configured deadline must not fail or stop
  the command. The exit code is valid only from the in-memory spawn watcher — a daemon restart
  mid-run that finds the process gone records `failed` with an unknown code, never `succeeded`.
  Stop of a finished row (`succeeded`, or `failed` while not in flight) succeeds and leaves that
  state. `exit` is rejected for `ownership: external` and for container commands. A shared recipe
  with this kind does not get the two-minute instance readiness deadline.
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
  `/v1/catalog`, `hearth status`, and the TUI.
- `groups:` members may name other groups — flattened depth-first (declaration order, deduplicated)
  into `ServiceCatalog.groups` for target resolution; the declared membership stays in
  `group_tree` (`groupTree` on the wire) so a client can group by *direct* membership instead of
  showing a giant `all` section. A member naming both a service and a group resolves as the
  service. Cycles are a load error, not truncation.
- `disabled: true` rejects direct start/stop/restart (`service_disabled`, `status` still works) and
  is stripped from flattened `groups` — group ops skip it silently; `group_tree` still lists it for
  display. It does not stop an already-running service — the row just stops being actionable.
- A missing/invalid `readiness` must fail the load, not skip the service.
- Readiness shorthand expands at project load, and for a shared recipe at synthesize: `kind: http`
  with no `url` is `GET http://127.0.0.1:<primary>/health` (2xx); `path` replaces `/health`;
  `kind: tcp` with no `port` uses the primary port (`ports[0]`, or the allocated shared port).
  `url` together with `path` or `port` is a load error. Do not point `http` at `/health` unless
  that process serves it — Redis and Kafka stay `command` (`hearth-ready`), MinIO stays
  `/minio/health/live`, MongoDB is `tcp`. The daemon's own `GET /healthz` is the manager liveness
  route, not a service probe. A recipe snapshot in `registry.json` is not refreshed when
  `catalog.json` changes.

**Shared services (`smp`)**

- A top-level `shared:` map in `hearth.yaml` (`postgres: "16.4"`) expands into generated
  `ownership: external` services (`config_file.rs` → `shared::synthesize::project_service_entry`):
  run = one-shot `hearth shared attach <name@ver>` task, readiness = `hearth shared probe`
  (exit 0 iff the instance is ready **and this project is attached**), stop = `hearth shared
  detach`. `projectId` = sha256 of the canonicalized cwd, so attach/probe/stop commands all agree
  by running with the project root as cwd. The object form (`{ version, preparationCommand,
  attachArgs, urls }`) adds a project-side prep hook before every attach and trailing attach
  args that land on the recipe's `provision` argv — how viclass/infra publish their nginx conf
  trees into the shared instance.
- The task semantics depend on a supervisor rule: `ownership: external` + `command` readiness ⇒
  the run command is a one-shot trigger, not the service process (`is_external_task`). A
  daemon-owned `command`-readiness service is still a normal long-lived process — don't widen it.
  The shared row's readiness budget (script pack + extract + instance probe, ~49 min) applies
  only while `hearth shared attach` is still running. Once that process exits, a non-zero code
  fails the row immediately and exit 0 gets at most five seconds of probes. Do not keep polling
  `hearth shared probe` for the pack budget after attach has finished: a failed instance does
  not become ready on its own.
- **`smp` = `hearth smp`** — the same binary, rooted at `~/.hearth/shared`, catalog *synthesized*
  from `registry.json` (single writer: the smp daemon; CLIs only read). Runtime dir is
  `~/.hearth/shared/runtime-v1`, so `discover`/`ensure` work unchanged against that root.
- Ports: `sha256(name@version)` into `43100–43999`, collision probes forward and persists into
  `registry.json`. Never hand out a well-known port (5432/6379) to a shared instance — the one
  sanctioned exception is a recipe's `ports: […]` **pinned** field (the shared nginx's `[18080, 18443]`),
  which skips hashing entirely and only bind-checks. For public :80/:443 on macOS there is no
  privileged-bind workaround for userspace — the shared nginx pins `18080/18443` and a pf rdr
  anchor (`com.apple/hearth`, loaded once with sudo `pfctl -a … -f - && pfctl -e`) maps the
  public ports onto them. A recipe's
  `additionalPorts` reserves that many extra ports in one contiguous block (`{port2}`, …); every
  port in the block is taken. `prepare` is the idempotent init hook (runs before every start via
  `preparation_command`). `run` must stay the process `ps` keeps showing. A wrapper that does work
  and then `exec`s is adopted as the wrapper and orphaned once the real server replaces it (stop
  still kills it, since pid and start time still match; start does not re-adopt it). A
  server that rewrites its own title (nginx's `nginx: master process …`) has to be started with
  `shell` + `exec: true` so the stored identity is that settled line, not the pre-title argv.
  A dedicated argv binary that keeps the same absolute executable (Redis `setproctitle`) stays
  owned without that wrapper; a start adopts an untracked copy instead of spawning one that
  cannot bind.
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
  `~/.hearth/shared/catalog-url` → pinned URL — the file exists because a daemon started outside a
  login shell does not inherit `HEARTH_SHARED_CATALOG_URL`. `remote.rs` also embeds the same `catalog.json` (`include_str!`) as the
  final fallback — a stale disk cache is topped up with any recipe the embedded catalog has that the
  cache lacks, so recipes added in newer `hearth` builds still reach machines that cached an old
  `catalog-url`/`file://` document.
- Shared MongoDB runs `--replSet rs0` (consumers use transactions — e.g. engreel's
  `WithTransaction`) and `--wiredTigerCacheSizeGB 0.25`; its `provision` payload initiates rs0
  idempotently then writes the project-db marker, which is why the recipe needs the repacked
  mongod+mongosh tarball rather than the bare upstream archive.
- `POST /v1/shared/attach` blocks through install+start+provision — clients must not use the 10s
  manager timeout for it (`request_with_timeout` with `None`).
- `POST /v1/shared/remove` refuses (409) an instance that still has project attachments unless
  `force: true` — it deletes every attached project's data. `hearth shared remove --force` and the
  TUI's two-press confirm (when attachments > 0) are the sanctioned confirmations, same contract as
  `killUnowned`. `POST /v1/manager/reload` likewise fails with 409 `stop_failed` and keeps the old
  catalog when a removed-but-active service can't be stopped. `hearth shared stop`/`start` exit
  non-zero when the operation fails. `hearth doctor` checks the suite's host tools (docker,
  tailscale, nc, ps, sh) on top of platform + catalog validation.

**Consumers and the `hearth` binary**

- Consumer scripts should call `~/.local/bin/hearth` (what `task install` writes), not assume
  a bare `hearth` on `PATH`.
- **Never assume tools are on `PATH`**: a process started by launchd gets
  `/usr/bin:/bin:/usr/sbin:/sbin`. The daemon inherits that, so `hearth` appends Homebrew and
  `~/.bun`/`~/.cargo` bin dirs (`with_known_tool_directories`) — otherwise `docker compose`,
  `tailscale serve status` and `tailscale status` fail to spawn. Test anything the daemon spawns under
  `env -i PATH=/usr/bin:/bin:/usr/sbin:/sbin`.
- `serde_json`'s `preserve_order` feature is on workspace-wide and is load-bearing: without it
  `Value`'s object type is a `BTreeMap`, and `hearth mcp install` silently alphabetizes every key in any
  hand-maintained config file it touches.
- `hearth mcp install --key <name>` exists because infra's registry uses `servers`, not the standard
  `mcpServers`. The merge only ever touches `command`/`args`.

**Terminal UI (`hearth tui`)**

- Dispatched before catalog load, so it runs with no `hearth.yaml` in the current directory.
  `hearth --root <project> tui` adds that project untrusted and selects it when it has a catalog.
  Extra arguments are `usage: hearth tui` (exit 2). The workspace file stays
  `~/Library/Application Support/HearthApp/workspaces.json` (ISO-8601 `addedAt`, no fractional
  seconds) so lists written before the desktop app was removed still load.
- It is an HTTP+SSE client. Highlighting a workspace only `discover`s. Enter on a trusted
  workspace with no live daemon `ensure`s. An untrusted folder takes a second enter before
  anything is spawned. A daemon the user stopped stays stopped until they press enter on that
  workspace again; refresh never `ensure`s.
- Two presses confirm trust, forget (services keep running), stop daemon, restart daemon
  (services stay up), kill-unowned reclaim, and shared remove (`force` when attachments > 0).
  Stop, restart, or remove of a shared instance, and stop or restart of a project `shared:` row
  (including group and stop-all), also confirm when another workspace is attached. The confirm
  names those workspaces. Instance stop/restart/remove takes the singleton down for every
  attachment. A project stop or restart only detaches this workspace — the other workspaces keep
  the shared service. The attachment list is `registry.json`; if it cannot be read the confirm
  says so and the second press is an explicit override. A project `shared:` row keeps the
  normal state colour and paints the word `shared` in its label blue.
- The shared view reads recipes from the remote catalog and instances from a live smp or the
  local registry. It does not spawn smp just to draw. Install uses an unbounded request timeout.
- `WorkspaceStore::reload` must not quarantine `workspaces.json`; only `open` does.
- A catalog mtime change reloads once while a daemon is up, and that reload must not `ensure`. The first observation only records the mtime. Stop and copy stay available during an in-flight start. Enter on a queued row cancels it. Reveal is `open -R`. Stop-all skips rows that are already stopped or succeeded.
- The workspace shell paints with Ratatui 0.30 (`terminal.draw` in `shell.rs`). Keys stay on the command table in `desk.rs`, and the HTTP+SSE client is unchanged. The binary calls `run_shell` only. `run_tui` (ANSI single-project) is **deprecated** and kept as reference; do not wire new entry points to it.
- A bare `hearth` on a terminal **is** `tui` (stdin+stdout TTY check in `main.rs::run_cli`); piped
  it prints help, so `hearth` in scripts/`Command::output()` stays a usage printer.
- Mouse reporting is button, drag, and wheel (`?1000`/`?1002`/`?1006`, `MOUSE_ON` in `shell.rs`).
  Do not enable all-motion (`?1003`) or crossterm's `EnableMouseCapture`. Any reporting mode stops
  the terminal from selecting text. Ghostty will not select while it is on, and Shift does nothing
  when `mouse-shift-capture` is off, so a drag that starts on a painted URL is selected in the shell
  (reversed cells, copied on mouse-up). `m` drops reporting for a native drag. `paint_links` must
  not rewrite unchanged link cells, or that selection is cleared on the next frame.
- URLs are OSC 8 hyperlinks: `Desk::draw` records each painted http(s) URL (the `URL …` rows and
  log lines) into `desk.links`, and `paint_links` re-writes those cells wrapped in `]8;;` after
  `terminal.draw`. The escape must stay out-of-band — inside a `Span` it lands as literal cell text.
  A Cmd-click is not delivered while reporting is on (Command is not in the SGR mouse report), so
  the shell copies the URL itself.
- A `requiresRunning` URL (the default) is shown while its row is up *or* `succeeded`. A finished
  `readiness: exit` row has no process by design, so hiding its link after success (Viclass `fe`)
  read as "no URL registered". `url_visible` carries that rule; do not fold `Succeeded` into
  `is_up`, which also picks Stop vs Start and group Restart. `hearth urls` does not flag a
  succeeded row `(not running)`, but its `--json` `running` stays false.
- The daemon's own log is the pinned `daemon log` row — pseudo-id `$daemon` (`$` can't collide with
  a real service id), fetched from `GET /v1/daemon/log` rather than `/v1/logs/:id`, so it survives
  catalog reloads. `display_state` collapses `running`/`running-unready` → `running` and
  `starting`/`preparing` → `starting`. `ready` and `running` both count in the ready/total
  summaries. Finite services (`readiness: exit`) stay out of those totals unless `failed`.

**Testing gotchas**

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
  which is why the product is the Rust `hearth` binary.

## Notes on the Rust implementation

- **No `Custom` readiness variant.** A closure can't cross the YAML/JSON boundary.
- **SSE backpressure is frame-count only** (64 frames).
- **Ratatui 0.30 paints the workspace shell; `run_tui` still uses crossterm strings.** `run.rs` has no automated coverage (it
  owns a real terminal). Log text is sanitized, then SGR is parsed into spans for the shell log pane.
- **`truncateToWidth`/`visibleWidth`/`DEFAULT_TAB_WIDTH` were reverse-engineered** against pi-tui's
  native addon. The tab width is a fixed 3-space replacement. See `truncate_to_width`'s doc comment.
- **`hearth-mcp` hand-implements `ServerHandler`** rather than using `rmcp`'s `#[tool]` macros: names
  carry a runtime-configurable prefix and schemas embed the caller's `knownServiceIds`.
- **`hearth mcp` and `hearth tui` are intercepted in the binary**, not in `hearth-cli`, to avoid a crate cycle.
  `hearth update` is dispatched before catalog load too (the implementation lives in `hearth-cli`). `hearth mcp install` / `hearth skill install` live in `hearth-cli`. `tui` is dispatched before catalog load: it is an app shell.
- **`state.json` timestamp validation is a non-empty-string check.**

## Maintaining this file

Keep this file for knowledge useful to almost every future agent session in this project.
Do not repeat what the codebase already shows; point to the authoritative file or command instead.
Prefer rewriting or pruning existing entries over appending new ones.
When updating this file, preserve this bar for all agents and keep entries concise.

Record the *rule* a bug taught, not the story of finding it.
