# Project agent memory

Project-intrinsic knowledge that should travel with the code: orientation, build/test/release,
architecture, and sharp edges. Add durable notes here as real work discovers them.

## Orientation

The product is the compiled `hearth` binary — `go/cmd/hearth` in module
`github.com/ngosangns/hearth/go`, packages under `go/internal/`. The Rust workspace (`rust/`) is
gone and the TUI was not ported: `hearth tui` is unknown and a bare `hearth` prints help. The
workspace window is `hearth-gpui` in sibling repo `ns-gpui`; this repo has no UI. 0.18.0 renamed
the command from `hearthd`; never install or publish `hearthd`. A project authors `hearth.yaml`
(`.yml` / `.json`); TypeScript catalogs are not accepted. Consumers (`infra`, `viclass`) spawn
`~/.local/bin/hearth` after `task install`.

`hearth manager ensure --json` prints the connection (`token`, `port`, `runtimeDirectory`, …) an
HTTP+SSE client needs; the bearer `token` is masked unless `--reveal`. `internal/env` resolves the
daemon's base environment (login shell + `.env`), since a launchd-started process gets a bare `PATH`.

Manager HTTP lives in `go/internal/manager/` (`manager.go` bootstrap/shutdown, `routes.go` +
`http.go` the mux, `shared_http.go` the smp-only `/v1/shared/*`). `OperationScheduler`
(`internal/manager/operations.go`) serializes per service id; a bulk start locks every id in
`targetServiceIDs` (sorted, to avoid deadlock) — disjoint bulks run in parallel, overlapping ones
wait only on shared services; `__manager__` is reserved for manager-wide work (shutdown).
`ShutdownMode` (`LeaveServices` | `StopServices`, `internal/manager/protocol.go`) replaced
`stop_services: bool`. `ProcessSupervisor` lives in `internal/supervisor/engine*.go` (tests
`engine_tests_*_test.go`), adapters in `adapters.go`.

## Build, test, release

From `go/`: `go test ./...`, `go vet ./...`, `gofmt -l .` (empty); `task test` / `task clippy` /
`task ci` from the root. The suite needs `docker` + `tailscale` running, plus `nc`, `ps`, `sh`.

A test that boots a `HearthManager` in-process must hold
`defer (&manager.StopServicesOnDrop{Manager: m}).Stop()` for its whole body: `m.Close()` is a
leave-services shutdown and a panic skips explicit shutdown, so its real services
(`exec nc -lk {port}`) outlive the test binary under launchd (~6 per run). A clean run leaves no
`nc -lk`; check `ps -axo ppid,command | grep 'nc -lk'`.

**Install/refresh `hearth`** — `task install` builds into `~/.local/share/hearth/bin/hearth-<version>`
(`CGO_ENABLED=0 go build -trimpath -ldflags "-s -w -X main.version=$(cat VERSION)"`), ad-hoc signs
that file (never the live path), and symlinks `~/.local/bin/hearth` to it; `task go:build` leaves a
plain `go/hearth` (used by `scripts/catalog/smoke.sh`). `hearth update` downloads the GitHub asset
`hearth-<tag>` into the same layout after verifying tag, asset name, `github.com` URL, size,
`sha256`, executable bit, and a staged `--version` smoke test; it sends a bearer token only to
`https://github.com` and `https://api.github.com`. It does not re-sign — `codesign --force` on a
mapped ad-hoc binary, or writing over its inode, SIGKILLs it; the symlink inode is re-checked just
before the swap, previous file kept one generation. A running daemon keeps its old inode until
`hearth --root <project> manager restart` (services stay up); restart smp separately. It refuses a
current executable that is not that symlink's regular file or one already in the versioned dir,
anything but darwin-arm64, an equal version unless `--force`; never downgrades.

**Release.** CI (`.github/workflows/ci.yml`) runs Go build + test + vet + gofmt on PRs and tags
(`timeout-minutes: 90`), `workflow_call`-able with an optional `ref`. A `v*.*.*` tag runs CI; a
successful tag run triggers `.github/workflows/release.yml` (`workflow_run`, so a red tag never
publishes; tag/sha from `github.event.workflow_run`) on the self-hosted runner (`timeout-minutes:
60`), which builds, ad-hoc `codesign`s, and `gh release create --generate-notes` via
`.github/actions/release-asset` (idempotent, `--clobber`). The version lives only in the repo-root
`VERSION`, stamped with `-X main.version=`. Hands-off publishing is `.github/workflows/publish.yml`
(`workflow_dispatch`): bumps `VERSION`, pushes a `release v*` commit to main, calls `ci.yml` on that
sha, then pushes the `v` tag — self-contained because a `GITHUB_TOKEN` push fires no push events. An
existing tag is republished; equal/lower versions are refused. The bump uses `RELEASE_TOKEN` when
present (a PAT pushes to protected `main`), else `GITHUB_TOKEN`.

The self-hosted `{self-hosted, macmini}` runner executes `run:` steps outside a login shell, so
Homebrew and `~/.cargo/bin` are not on PATH — workflow steps add them via
`.github/actions/toolchain-path`; `ci.yml` skips fork PRs. `ProtocolVersion`
(`go/internal/state/state.go`) is the protocol-compatibility signal; a bump is breaking for every
client. It is **3** since `readiness: { kind: log }` and per-service `restart:` landed.

## Sharp edges

**Process supervision**

- `NormalizeCommandFingerprint` hashes the *logical* command text (argv joined, or the bare `shell`
  string), never the physical `sh -c` wrapper, so it matches
  `NormalizeObservedCommandFingerprint`'s prefix-stripped `ps` output; a `CommandArgv` refactor must
  re-check the pairing.
- Stopping signals the **whole process tree**, snapshotted from `ps` before the first signal: `air`
  runs the built server in its **own** group, so signalling only the tracked pgid would leave it
  holding its port. The snapshot is walked only while `ps` still shows the leader's recorded
  `startIdentity`, so a reused pid never drags in an unrelated tree; it also covers the leader's own
  group (a double fork reparents to launchd but stays in the group) and any descendant the 200 ms
  `lastTree` sampler saw alive with the same `lstart` (how a `setsid` escapee is stopped). A leader
  exiting on its own reaps leftover children the same way.
- Helper commands (`runCommand`, builds, `outputWithTimeout` in `internal/shared`) end when the
  leader exits **and** the pipes close. A pipe still open one second later is a leftover child:
  SIGKILL the group. The guard stays armed until then (`captureCommand`'s 5s timeout kills the group
  too); a reaped leader does not mean the group is gone. A child that redirected its output is left
  alone.
- Anything the daemon must kill on exit lives behind a guard (`groupGuard` — `Fire` on every failure
  path, `Disarm` once the leader is reaped and pipes closed) or a context: `os.Exit` runs no
  destructors, so cleanup is wired to contexts and the SIGINT/SIGTERM path in `internal/daemon`. A
  leave-services shutdown still calls `DetachAllOutput`, so `docker logs` followers do not outlive
  the daemon.
- An adopted process still alive is kept; a failing probe leaves it `running-unready` and the 1.5s
  loop keeps checking (start does not kill it; restart replaces it; one already gone is spawned
  again).
- `ProcessAdapter.Inspect` is tri-state (`Observed|Gone|Unknown`); a probe that fails to spawn or
  times out is `Unknown`, never "gone" — a wedged `docker`/`ps` would otherwise let `stop` record
  `stopped` over a running container and `reconcile` mark a live service `Failed` (duplicate spawn).
  Unknown ⇒ stop/terminate fail loudly, `status`/`reconcile` skip, the continuous probe keeps
  running.
- `ProcessSupervisor.Shutdown` (only `stop-services`, i.e. `hearth manager stop`) does three passes:
  active-state stop of daemon-owned services; then the project's `ownership: external` services that
  are up with a `stop:` command (a `shared:` entry's `hearth shared detach` gets `--stop-if-unused
  --yes` — `shared.ShutdownStopCommand` — so smp stops the instance only when no other project is
  attached); then a reap of daemon-owned services holding a stale POSIX identity in a non-active
  state (e.g. `externally-owned` after a port conflict). Don't collapse them, and keep the external
  pass after the first so apps release a shared database before it goes. `manager restart` and
  SIGTERM never run it.
- A failed container stop must propagate, not be swallowed — the caller transitions the service to
  `stopped` right after, recording a stop whose container still runs.
- **A stop must never report success without stopping something.** A service with no process
  identity — an adopted `ownership: external` unit, or one whose port an unowned process holds — is
  stopped by the catalog's `stop:` command, and *fails* when the catalog declares none. An
  `orphaned` POSIX row is still stopped when pid **and** `lstart` match the recorded identity (the
  same process, typically a shell that `exec`ed into its server); it gets the owned escalation
  (`terminatePosixTree`, TERM → grace → KILL), and restart's stop phase and the shutdown pass take
  the same path. A different start time (reused pid), a Docker identity, or one recorded for another
  service/generation is not ours: those refuse "no longer owned by this manager". Exception: a
  finished `readiness: exit` row — stop succeeds immediately, leaving `succeeded`/`failed`; never
  route it through `stopUnowned`.
- Restart is not that stop. Restart of a row with no identity and no catalog `stop` command skips
  stop's refusal and starts. When the TCP listener is this service — the `exec` target in its
  directory, or the install directory still in the command line — restart records that identity and
  replaces the process with the owned process-tree stop before starting. A different program is not
  signalled; start reports the port is held; stop of the same row still fails. `hearth
  stop`/`restart` of a group waits per member, so the first failure still stops the rest; `--wait`
  only blocks a single service. In the app, group stop/restart wait for every member and the notice
  names a failure.
- **`killUnowned` is the only path that signals an arbitrary process holding a service port.** It
  is a client-supplied `POST /v1/operations` flag (`action: start` only; all other surfaces reject
  it), set only after explicit confirmation: CLI `--kill-unowned` (a TTY flag is its own
  confirmation; off a TTY also needs `--yes`, exit 6), the TTY `[y/N]` prompt for an
  externally-owned row when the flag is absent (non-TTY answers no), and the window's "Kill and
  Start". `reclaimPort` resolves listeners via `ProbeAdapter.PortHolders` (lsof) and
  signals **individual pids** via `ProcessAdapter.SignalPID` — SIGTERM, poll, re-resolved SIGKILL.
  Never `killpg` an unowned holder (untrusted group; pgid 1 untouchable). `SignalPID` re-verifies
  `lstart` before sending, so a recycled pid is never killed; `nil` from `PortHolders` means "cannot
  resolve" — fail closed.
- `manager restart` reaps every other live daemon for the same root first. A root matches `hearth
  daemon --root <path>` (either arg order; spaces allowed; a longer path is not a match);
  `~/.hearth/shared` matches `hearth smp`. It signals those pids only (SIGTERM, then SIGKILL), never
  the group, so detached services keep running. A service `restart` reaps host processes in the cwd
  whose fingerprint is the logical command or the last stored identity, or whose argv0 is that
  service's absolute argv0 when the binary is a dedicated server no other catalog service uses from
  that directory — dedicated = not `hearth` and not a shared interpreter (`node`, `python`, `java`,
  `ruby`, `perl`, a shell, `bun`, `deno`). A start adopts that argv0 process; a stored pid stays
  owned on argv0 even when another service shares the binary. A shell command stays on the settled
  fingerprint (`shell` + `exec`). Each match's ppid tree is signalled per pid; a process that only
  shares the port stays on `killUnowned`. If `ps` is unreadable, restart fails closed; a start skips
  the adopt.
- `spawnDetached` starts `_spawn1`, which starts `_spawn2` and exits; `_spawn2` runs the manager in
  the foreground — the double hop keeps the long-lived daemon out of a GUI client's macOS coalition,
  so quitting the window does not stop it. Each hop uses `Setpgid` and a background `Wait`;
  `platform.IsPIDAlive` must reap our exited children and treat any other zombie as dead, or
  `manager restart` sits on "restarting daemon…" for the 300s stop timeout and `ClaimLock` spins.
  Never weaken a *running* pid into "dead" because a health check timed out.

**Daemon / state**

- Fire-and-forget work (unit log forwarding, external-services polling) must never panic into the
  daemon; route through `RecordBackgroundError`.
- A daemon whose lock was taken over stops itself (`LockOwnershipWatch`) without touching the
  winner's lock — enforced from the losing side so two daemons never fight over one `state.json`.
- `ReloadCatalog` stops a removed-but-active service with the **old** catalog and swaps only
  afterward — the supervisor needs the old definition to stop it.
- On reconcile (including startup) a persisted `externally-owned` row is re-checked: a free TCP port
  with desired `running` becomes `failed` ("Managed process is no longer alive") and is not
  restarted, desired `stopped` becomes `stopped` with the error cleared. A holder that is this
  service — the `exec` target in this service's directory, or a command line still containing the
  `cd && exec build/install/.../bin/...` install directory — is adopted and probed; a different
  program stays `externally-owned` until a confirmed `killUnowned` start; `None` from the port probe
  leaves the row unchanged. `SyncExternalServices` only polls `ownership: external` units.
  A **renamed-ownership id** is a separate blind spot: removing a `shared:` key and re-adding it as
  `services:` keeps the old row (`adopted from external state`, `command` readiness) across
  restarts, so the new daemon-owned service never installs or spawns (`start` short-circuits on the
  persisted `ready`) — fix with `manager stop`, delete the service's `state.json` row, restart.
  Manual reconcile: stop the daemon, rewrite `state.json` `services` to `{}` (or delete stale
  entries); editing while it lives is overwritten on shutdown.
  Squatters: a relative `run:` like `exec node dist/main` is invisible to a path `ps` grep — find
  holders by port (`lsof -iTCP:N -sTCP:LISTEN`) or cwd (`lsof -d cwd`), and non-TCP binds too
  (viclass `syncer` panics on **UDP** 50000, invisible to a TCP scan).
- Wire enums are typed strings already in wire encoding (`state.ActualServiceState`,
  `state.ReadinessKind`); `ActualQueuedStart` **is** `"queued-start"`. Never hand-format a display
  string onto the wire; keep new variants kebab-case.
- `state.json` quarantines rather than trusts: a shape check that passes but deserialize fails must
  fall through to quarantine, never panic — a panic in `load()` kills the daemon at bootstrap.
- A `reset` from the event store returns the **whole** buffer, not an empty list (the client's
  cursor is unusable, so the reply is the resync snapshot). The SSE stream copies that replay and
  registers the listener under the same lock, holding live events until snapshot frames are queued;
  a reset does not subscribe. Backpressure stays 64 frames.
- **SSE is invalidation, not the source of truth for service rows.** `GET /v1/services` is
  authoritative; `service.lifecycle`, `manager.catalog-reloaded`, and
  `operation.accepted`/`operation.updated` only nudge a re-fetch, `service.log` only refreshes the
  log cursor. Never reconstruct lifecycle state from event payloads alone.
- The log `generation` clients echo is `lifecycle * 1_000_000 + rotation`; a rotation resets a
  follower like a restart. Generation 0 is valid (no state row). `GET /v1/logs/:id` with no `limit`
  is the 16 KiB tail; an explicit `limit` is honored up to `DefaultLogMaxBytes` (256 KiB) so a pane
  can page earlier bytes — never clamp it back to the tail, or the next page returns the same window
  forever.

**Catalog**

- `{ kind: "command" }` readiness is the JSON-serializable custom-probe stand-in: exit 0 = ready,
  else `running-unready`; it repeats every 1.5s (`ReadinessBackoffMs`) until stop, restart, process
  exit, or daemon shutdown, and does not fail the service. `readinessTimeoutMs` is still parsed but
  no longer ends a long-lived start — it bounds one command probe and the one-shot shared-attach
  wait. `hearth start --wait` and bulk start settle at `ready`/`running-unready` once the process
  is up; `status` does not probe.
- `{ kind: "exit" }` — the run command is the job. While running the state is `running` (not
  `ready`/`running-unready`). Exit 0 records `succeeded` and sets desired `stopped` (reconcile does
  not rerun); any other exit is `failed`, desired `stopped`, no auto-restart. `readinessTimeoutMs`
  does not apply — a deadline must not fail or stop the command. The exit code is valid only from
  the in-memory spawn watcher: a daemon restart mid-run that finds the process gone records `failed`
  with an unknown code, never `succeeded`. Stop of a finished row (`succeeded`, or `failed` while
  not in flight) succeeds, leaving that state. `exit` is rejected for `ownership: external` and
  container commands. A shared recipe with this kind skips the two-minute instance deadline.
- `preparationCommand` (with `serializationKey`) runs via the same `command` adapter; services
  sharing a key prepare one at a time. **`viclass`'s prep-dependent services must all share one
  key**: `ensure_local_certificates` is a check-then-generate race, and concurrent first-time
  prepares would corrupt the cert file.
- `ownership: 'external'` is the docker/tailnet-task adoption carve-out.
- A service's `artifact:` block (`version` + `url`|`script` + `sha256` for url) installs a versioned
  tarball into `<runtimeDir>/installs/<id>/<version>` before preparation and build on every start —
  the marker-based machinery `internal/shared/install.go` uses for smp (`ArtifactInstaller`;
  `DefaultArtifactInstaller` for file catalogs, `None` ⇒ start fails clearly). `{installDir}`/
  `{dataDir}`/`{port}`/`{portN}`/`{serviceId}`/`{projectRoot}` render at **catalog load**, not spawn
  (the served catalog carries literal paths); only those names substitute (other braces are legal),
  and a `{port}` with no declared port fails the load.
- Service order from a config file is **document order**, not sorted — visible in `/v1/catalog`,
  `hearth status`, and the app.
- `groups:` members may name other groups — flattened depth-first (declaration order, deduplicated)
  into `ServiceCatalog.groups` for target resolution; declared membership stays in `GroupTree`
  (`groupTree` on the wire) so a client can group by *direct* membership. A member naming both
  resolves as the service. Cycles are a load error, not truncation.
- `disabled: true` rejects direct start/stop/restart (`service_disabled`; `status` works) and is
  stripped from flattened `groups` (group ops skip silently; `groupTree` still lists it).
  `StopDisabledServices` (post-listen reconcile, catalog reload, the 2s tick) stops its processes
  from one process-table read: the tracked tree, a port holder that is this service, and other host
  processes of the service — same command, the program a `shell` + `exec` replaced into, or a
  command line still containing the install directory. A different program on the port stays for an
  explicit `killUnowned` start; `SyncExternalServices` does not adopt a disabled row.
- A missing/invalid `readiness` must fail the load, not skip the service.
- Readiness shorthand expands at project load and at shared-recipe synthesize: `kind: http` with no
  `url` is `GET http://127.0.0.1:<primary>/health` (2xx); `path` replaces `/health`; `kind: tcp`
  with no `port` uses the primary port (`ports[0]`, or the allocated shared port). `url` with `path`
  or `port` is a load error. Point `http` at `/health` only if the process serves it — Redis/Kafka
  stay `command` (`hearth-ready`), MinIO `/minio/health/live`, MongoDB `tcp`. The daemon's own
  `GET /healthz` is the manager liveness route, not a service probe. A `registry.json` recipe
  snapshot is not refreshed when `catalog.json` changes.
- **`restart:`** — `{ on: never|on-failure|always, maxRestarts?: n, delayMs?: n|"2s" }`. Counts
  consecutive unexpected exits; resets on `ready` (`transition`) and on an explicit start/restart
  (why auto-respawn calls `startLocked`, not `Start()`). Respawn sleeps `delayMs`, re-enters the
  service queue, and re-checks `Failed && desired running` inside it — a `stop` during the delay
  wins and a manual start that recovered the service can't double-spawn.
- **`readiness: { kind: log, pattern }`** — registered lazily on first output in `appendOutput`
  (the funnel all output paths share: spawn sink, `attachOutput` tails, docker followers); latches
  on the first regex match against a rolling 64 KiB tail (patterns split across writes still land),
  cleared in `spawnAndWait`/`detachOutput` so a prior incarnation's match never satisfies a new
  process. **humantime durations** — `readinessTimeoutMs`, `build.timeoutMs`, `restart.delayMs`
  accept `"30s"` via `readDurationMs` (`internal/configfile`) and bare ms.

**Shared services (`smp`)**

- `shared:` in `hearth.yaml` (`postgres: "16.4"`) expands to generated `ownership: external`
  services (`internal/configfile` → `shared.ProjectServiceEntry`): run = one-shot `hearth shared
  attach <name@ver>`, readiness = `hearth shared probe` (exit 0 iff the instance is ready **and this
  project is attached**), stop = `hearth shared detach`. `projectId` = sha256 of the canonicalized
  cwd, so attach/probe/stop agree by running with the project root as cwd. The object form
  (`{ version, preparationCommand, attachArgs, urls }`) adds a project-side prep hook before every
  attach and trailing args landing on the recipe's `provision` argv (how viclass/infra publish nginx
  conf trees).
- `ownership: external` + `command` readiness ⇒ the run command is a one-shot trigger, not the
  service process (`isExternalTask`); a daemon-owned `command`-readiness service stays a normal
  long-lived process — don't widen it. The shared row's readiness budget (pack + extract + instance
  probe, ~49 min) applies only while `hearth shared attach` runs; once it exits, a non-zero code
  fails the row at once and exit 0 gets ≤5s of probes — don't keep polling for the pack budget; a
  failed instance does not become ready on its own.
- **`smp` = `hearth smp`** — same binary, rooted at `~/.hearth/shared`, catalog *synthesized* from
  `registry.json` (single writer: the smp daemon; CLIs only read). Runtime dir
  `~/.hearth/shared/runtime-v1`, so `discover`/`ensure` work unchanged.
- Masking (`internal/redact.Values`) applies unless `--reveal` to `shared list`/`status`/`installed`,
  `shared connection`, the printed `shared attach` connection, `manager ensure`/`restart`, and
  `shared ensure` (the bearer `token`) — direct HTTP+SSE clients must pass `--reveal`. hearth-gpui
  Copy buttons and `load_shared_board`'s `shared list --json` do. `--mask` is accepted, changes
  nothing, and with `--reveal` is a usage error (exit 2). A masked string's upper-case key contains
  SECRET, PASSWORD, PASSWD, TOKEN, API_KEY, ACCESS_KEY, or PRIVATE_KEY (`-` = `_`), or it is a URL
  userinfo password or a `KEY=value`/`--password` argv element — covering `recipe.env`, attachment
  maps, provision args. The skill's `shared-list.sh`/`shared-status.sh` stay masked and forward
  `--reveal` only when the caller passed it. `Client.EnsurePayload()` and in-process callers keep
  the real token.
- Ports: `sha256(name@version)` into `43100–43999`; collisions probe forward and persist to
  `registry.json`. Never hand out a well-known port (5432/6379) — the one exception is a recipe's
  pinned `ports: […]` (`[18080, 18443]` for the shared nginx), which skips hashing and only
  bind-checks. Public :80/:443 on macOS has no userspace privileged-bind, so nginx pins
  `18080/18443` with a pf rdr anchor (`com.apple/hearth`, `sudo pfctl -a … -f - && pfctl -e`).
  `additionalPorts` reserves that many extra contiguous ports (`{port2}`, …). `prepare` is the
  idempotent init hook (runs before every start via `preparation_command`); `run` must stay the
  process `ps` keeps showing. A wrapper that works then `exec`s is adopted as the wrapper and
  orphaned once the real server replaces it (stop still kills it — pid + start time match; start
  does not re-adopt). A server that rewrites its title (nginx `master process …`) needs `shell` +
  `exec: true` so the identity is that settled line; a dedicated argv binary keeping the same
  absolute executable (Redis `setproctitle`) stays owned without it, and a start adopts an untracked
  copy instead of spawning one that cannot bind. `{projectBucket}` = `h-<projectId>` (S3 rejects the
  underscore in `{projectDb}`).
- Install = tarball + sha256 into `installs/<name>/<version>` (atomic rename; `.hearth-installed`
  marker last); the catalog artifact is a `url` (download the archive) or a `script` (run it to
  write one). **sha256 is verified only for `url`** — script output is never byte-reproducible, so
  the script is the trust boundary. Recipe snapshots live in `registry.json`, so installed
  instances survive upstream removal from `catalog.json` (the repo-root file the pinned
  `SHARED_CATALOG_URL` serves; `HEARTH_SHARED_CATALOG_URL` overrides it for dev/tests, `file://`
  works for both). URL order: env → `~/.hearth/shared/catalog-url` → pinned — the file exists
  because a daemon outside a login shell does not inherit the env var. `internal/shared/remote.go`
  embeds the same `catalog.json` (`//go:embed`; `write-catalog.py` writes both copies,
  `remote_test.go` fails on drift) as the final fallback: a stale cache is topped up with any
  embedded recipe it lacks (`loadCached` → `mergeMissingRecipes`).
- Shared MongoDB runs `--replSet rs0` (consumers use transactions — e.g. engreel's
  `WithTransaction`) and `--wiredTigerCacheSizeGB 0.25`; its `provision` payload initiates rs0
  idempotently then writes the project-db marker, which is why the recipe needs the repacked
  mongod+mongosh tarball rather than the bare upstream archive.
- `POST /v1/shared/attach` blocks through install+start+provision — clients must not use the 10s
  manager timeout (`RequestWithTimeout` with a nil timeout).
- `POST /v1/shared/remove` refuses (409) an instance with project attachments unless `force: true`
  — it deletes every attached project's data. `hearth shared remove --force` and the app's dialog
  (attachments > 0) are the sanctioned confirmations, same contract as `killUnowned`. Off a TTY,
  `shared remove` and `shared detach --stop-if-unused` also need `--yes` (exit 6); the daemon's
  shutdown detach appends `--yes`. `POST /v1/manager/reload` likewise fails 409 `stop_failed`,
  keeping the old catalog when a removed-but-active service can't be stopped. `hearth shared
  stop`/`start` exit non-zero on failure. `hearth doctor` checks host tools (docker, tailscale, nc,
  ps, sh) on top of platform + catalog validation.

**Consumers and the `hearth` binary**

- Consumer scripts call `~/.local/bin/hearth` (what `task install` writes), not a bare `hearth` on
  `PATH`.
- **Never assume tools are on `PATH`**: a launchd-started process gets
  `/usr/bin:/bin:/usr/sbin:/sbin`, and the daemon inherits it, so `hearth` appends Homebrew and
  `~/.bun`/`~/.cargo` bin dirs (`env.WithKnownToolDirectories`, in `main`) — else `docker compose`,
  `tailscale serve status`, `tailscale status` fail to spawn. Test daemon-spawned commands under
  `env -i PATH=/usr/bin:/bin:/usr/sbin:/sbin`.
- `internal/omap` (ordered map wherever key order matters) is load-bearing: without it
  `map[string]any` marshals keys alphabetically and hand-maintained JSON loses its key order.

**Workspace UI contract (`hearth-gpui` in `ns-gpui`; `hearth tui` was removed)**

- A bare `hearth` prints help on or off a terminal — no interactive shell in the binary; `tui` is
  unknown (exit 2). The window is the only workspace UI, and it is not in this repo.
- The window is an HTTP+SSE client (`internal/client` semantics): selecting a workspace only
  `discover`s; opening a trusted workspace with no live daemon `ensure`s; an untrusted folder needs
  a second confirmation. A daemon the user stopped stays stopped until the workspace is reopened;
  refresh never `ensure`s.
- The folder list is `~/Library/Application Support/HearthApp/workspaces.json` (`{ id, path,
  trusted, addedAt }`); `HEARTH_WORKSPACE_FILE` overrides it. `hearth workspaces list` reads it
  without creating it — a missing file is empty.
- Destructive actions confirm explicitly: trust, forget (services keep running), stop/restart daemon
  (restart keeps services up), kill-unowned reclaim, and shared remove (`force` when attachments >
  0). Stop/restart/remove of a shared instance, and stop/restart of a project `shared:` row
  (including group and stop-all), also confirm when another workspace is attached — naming those
  workspaces. Instance stop/restart/remove takes the singleton down for every attachment; a project
  stop/restart only detaches this workspace. The attachment list is `registry.json`; if unreadable
  the confirm says so and confirming anyway is an explicit override.
- The shared view reads recipes from the remote catalog and instances from a live smp or the local
  registry; it never spawns smp just to draw. Install uses an unbounded request timeout.
- A `requiresRunning` URL (default) shows while its row is up *or* `succeeded` — a finished
  `readiness: exit` row has no process, so hiding its link after success read as "no URL
  registered". `url_visible` carries that rule; do not fold `Succeeded` into `is_up`, which also
  picks Stop/Start and group Restart. `hearth urls`'s `--json` `running` stays false for it.
- The daemon's own log is the pinned `daemon log` row — pseudo-id `$daemon` (can't collide with a
  service id), fetched from `GET /v1/daemon/log` not `/v1/logs/:id`, so it survives catalog reloads.
  `running-unready` displays as `degraded`; other rows use the wire string.
  `ready`/`running`/`running-unready` count in ready/total summaries; finite services
  (`readiness: exit`) stay out unless `failed`.

**Testing gotchas**

- **A fixed sleep waiting on another process is a flake.** Poll for the actual condition with a
  generous deadline instead.
- Right after spawning, a child can still be mid-`execve`, and macOS `ps` reports a parenthesized
  placeholder (`(sh)`). Never treat the first readable `ps` row as a just-spawned process's
  authoritative fingerprint — see `acceptsSpawnObservation`.
- A spawned test shell needs its own process group (`SysProcAttr{Setpgid: true}`), or it inherits
  the test harness's pgid and the test's `killpg` calls signal the whole test run.
- The TCP readiness probe's own `connect()` is a real connection: `nc -l <port>` without `-k` exits
  on it, so the service goes ready then immediately failed.
- `HearthManager.ShutdownCompletion()` is a channel **closed** on shutdown, so a late reader sees
  a completed shutdown.

## Notes on the Go port

- **No `Custom` readiness variant** (a closure can't cross YAML/JSON); SSE backpressure is
  frame-count only (64 frames), hand-rolled in `internal/client/events.go`.
- `hearth daemon`/`smp` are intercepted in `cmd/hearth/main.go` (they run a manager in the
  foreground); `shared`/`update`/`version`/`schema`/`skill`/`workspaces` dispatch before catalog
  load.
- With `--json`, a failure prints `{"error":{code,exit,message,hint?}}` (exit 6 = needs-confirm).
  Stdin is a terminal only when ioctl succeeds (`ModeCharDevice` is true for `/dev/null`), wiring
  `Io.Confirm` and the `--yes` gates — a GUI or `</dev/null` must leave Confirm nil.

## Maintaining this file

Keep this file for the knowledge useful to almost every future agent session; do not repeat what
the codebase shows — point to the authoritative file or command. Prefer rewriting or pruning
entries over appending, and record the *rule* a bug taught, not the story of finding it.