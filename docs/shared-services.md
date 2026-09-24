# Shared Services + `smp` — design

A machine-wide service registry of common infrastructure (postgres, redis, mongo, …) installed
on the host into an isolated prefix — no Docker, nothing in `/opt/homebrew` or `/usr/local` — and
run as **singletons per `name@version`**, managed by one global daemon, **`smp`** (service manager
process). Projects declare what they need in `hearth.yaml`; identical registrations across repos
share one instance, different versions run side by side.

## Decisions (locked)

| Topic | Decision |
|---|---|
| Install | Prebuilt tarball + sha256, extracted into `~/.hearth/shared/installs/` |
| Share identity | `name@exact-version` → singleton |
| Ports | Deterministic `hash(name@version)` into `43100–43999`, collision → probe next slot, persisted |
| Topology | One global `smp` daemon; project daemons adopt via `ownership: external` |
| Connection info | Via `hearth-mcp` `shared_*` tools + skill doc; not auto-injected into app env |
| Lifecycle | On-demand start; never auto-stops; `hearthd shared remove` is the only GC |
| Recipe source | `catalog.json` fetched from the pinned GitHub repo URL (HTTPS is the trust boundary) |
| Provisioning | Per-project logical resources (e.g. `db_<hash>` + user) via recipe `provision` commands |
| Yaml schema | Top-level `shared:` map; each entry generates an `external` service |
| Install UX | Blocking attach; progress streams into the project's service log; smp-side `installing` install-state |
| Platform | `darwin-arm64` only |

## Layout — `~/.hearth/shared/`

```
~/.hearth/shared/
├── catalog.json                  # cached remote registry
├── registry.json                 # instances: port, dirs, recipe snapshot, attachments, install state
├── downloads/                    # tmp download/extract staging (quarantined on failure)
├── installs/<name>/<version>/    # extracted artifact, immutable once installed
├── instances/<name>@<version>/   # data dir + generated config
└── runtime-v1/                   # manager.lock/, token, state.json, logs — same layout as projects
```

`smp` is the same `hearthd` binary: `hearthd smp` runs `run_daemon` with
`root = ~/.hearth/shared` and a **synthesized** `ServiceCatalog` (built from `registry.json`, not a
yaml file — `HearthManagerOptions.catalog` is already consumer-authored). `catalog.runtime_directory
= "runtime-v1"` so lock/token/state land in `~/.hearth/shared/runtime-v1` and the existing
`discover()`/`ensure()` machinery works unchanged against `root = ~/.hearth/shared`.

Each registry instance becomes one `ServiceDefinition`:
`id = "postgres@16.4"`, `ownership: daemon`, run/stop/readiness rendered from the recipe snapshot
(template vars `{installDir}`, `{dataDir}`, `{port}`). New registrations enter the running daemon
through `reload_catalog` — the old-catalog-stop rule applies unchanged.

## `catalog.json` (remote registry)

Pinned URL in the binary (`SHARED_CATALOG_URL`). Tarball `sha256` lives in this file, so the file
itself is trusted on TLS alone — fetching anywhere else is not supported.

```json
{
  "version": 1,
  "services": {
    "postgres": {
      "versions": {
        "16.4": {
          "artifacts": {
            "darwin-arm64": { "url": "https://github.com/ngosangns/hearth/releases/download/catalog-v1/postgres-16.4-darwin-arm64.tar.gz", "sha256": "…" }
          },
          "run":       { "argv": ["{installDir}/bin/postgres", "-D", "{dataDir}", "-p", "{port}"] },
          "stop":      { "argv": ["{installDir}/bin/pg_ctl", "-D", "{dataDir}", "stop", "-m", "fast"] },
          "readiness": { "kind": "command", "command": { "argv": ["{installDir}/bin/pg_isready", "-h", "127.0.0.1", "-p", "{port}"] } },
          "provision": [
            { "argv": ["{installDir}/bin/psql", "-h", "127.0.0.1", "-p", "{port}", "-c", "CREATE DATABASE {projectDb}"] }
          ],
          "connection": { "url": "postgres://{projectUser}@127.0.0.1:{port}/{projectDb}", "env": { "DATABASE_URL": "{url}" } }
        }
      }
    }
  }
}
```

Template vars: `{installDir}`, `{dataDir}`, `{port}`, `{projectId}`, `{projectDb}`, `{projectUser}`.
`file://` artifact URLs are accepted (tests/local fixtures).

## Port allocation

`base = 43100 + sha256(name@version)[..8] mod 900`. At instance registration, if `base` is taken by
another registry entry or a live listener, probe `base+1…` (wrapping inside the range); the winner
is persisted in `registry.json`. Deterministic-first keeps the common case stateless.

## Project side

```yaml
version: 1
shared:
  postgres: "16.4"        # exact version; must exist in catalog.json at attach time
services: { … }
```

`config_file` generates, per entry, a normal `ServiceDefinition` (id = the map key):

- `kind: infrastructure`, `ownership: external`, `label: "postgres@16.4 (shared)"`
- `run`: `argv [hearthd, "shared", "attach", "postgres@16.4"]` — a **one-shot task**: ensure smp,
  install, start, attach, provision, print conninfo to stdout (→ `hearthd logs postgres`).
  `hearthd` resolves to the daemon's own `current_exe` — never PATH.
- `readiness`: `{ kind: command, command: { argv: [hearthd, "shared", "probe", "postgres@16.4"] } }` —
  exit 0 iff the instance is ready **and this project is attached** (projectId = sha256 of the
  canonical project root, derived from the probe's cwd). This makes detach→release work through
  plain `syncExternalServices`.
- `stop`: `argv [hearthd, "shared", "detach", "postgres@16.4"]` — releases the attachment; the
  instance keeps running.
- `readinessTimeoutMs`: 10 min default (first start includes download+install).
- Joins group `all` when it exists. A `shared` key colliding with a `services` id is a validation
  error via the existing duplicate-service check.

Engine change: `is_task_command` extends to `ownership: external` + `command` readiness — the run
command is a trigger, not the process. Daemon-owned `command`-readiness services are unchanged.

## `smp` HTTP surface (additive; `PROTOCOL_VERSION` unchanged)

Mounted only when `HearthManagerOptions.shared` is set:

- `GET  /v1/shared` — instances, ports, install state, attachments (with rendered `connection`)
- `GET  /v1/shared/catalog` — the cached/fetched remote registry
- `POST /v1/shared/attach` `{requestId, service, projectRoot?}` — resolve recipe → register →
  install (state `installing` in `registry.json`) → ensure in catalog (`reload_catalog`) →
  `supervisor.start` → wait ready → provision → return rendered `connection`. Serialized per
  instance, idempotent per `(service, projectId)`.
- `POST /v1/shared/detach` `{service, projectRoot}` — drop attachment (+ `deprovision` if the
  recipe declares it); instance keeps running.
- `POST /v1/shared/install` `{service}` — install without attaching (pre-warm).
- `POST /v1/shared/remove` `{service}` — stop, remove dirs and registry entry (manual GC).

`attach`/`probe`/`detach` on the CLI derive `projectRoot` from `--root` or cwd — no project
catalog needed.

## CLI — `hearthd shared …` (no project catalog required)

`ensure --json` (the `manager ensure --json` contract for smp — prints the `ManagerConnection`
for `~/.hearth/shared`, used by the macOS app) · `list [--json]` (remote registry, no daemon
needed) · `installed` · `status [--json]` · `start|stop <name@ver>` · `attach|detach|probe
<name@ver>` · `install <name@ver>` · `remove <name@ver>`.

## MCP + skill

`hearth-mcp` gains `shared_list`, `shared_status`, `shared_connection` (queried against smp
directly via its lock-dir token). The skill doc (`hearthd skill install`) documents the flow:
read `shared:` in `hearth.yaml` → attach happens on `start` → query connection info via MCP.

## macOS app

`Shared Services` is a singleton `Window` scene (menu bar item + Window menu) driven by an
app-level `SharedServicesController` — not per workspace. It connects via `DaemonConnection
.ensureShared()` (`hearthd shared ensure --json`) and the same `ManagerClient`/`SharedAPI`
surface, so instance state streams over smp's `/v1/events/stream` exactly like a project
daemon's. The catalog UI shows registered instances (state, port, attached projects +
copyable connection info) and installable `name@version` entries; install/start/stop/remove
are the only mutations — attach/detach stay project-side via `shared:` + `start`.

## Edge cases

- Version removed from `catalog.json` upstream: installed instances keep working — the recipe is
  snapshotted into `registry.json` at registration. Remote only governs *new* installs.
- sha256 mismatch / corrupt tarball → quarantine the staging dir, `install` state `failed`, error
  in `daemon.log`. Extract to `downloads/` then atomic rename into `installs/`.
- `registry.json` corrupt → same quarantine discipline as `state.json`.
- Project moved/renamed → new `projectId` → old logical resource orphaned inside the shared
  instance; `shared remove` or manual cleanup.
- smp dies → project service loses probe → released to `stopped`; restart via next `start`.
- Fire-and-forget work in smp routes through `record_background_error`.

## Non-goals (v1)

- No env injection into project services; no refcount stop; no auto-GC; no `darwin-x86_64`;
  no per-registration config overrides beyond what the recipe templates; no signing of
  `catalog.json` beyond HTTPS.
