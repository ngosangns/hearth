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
| Connection info | Via skill scripts (`shared-list`/`shared-status`/`shared-connection`) or CLI; not auto-injected into app env |
| Lifecycle | On-demand start; never auto-stops; `hearth shared remove` is the only GC |
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

`smp` is the same `hearth` binary: `hearth smp` runs `run_daemon` with
`root = ~/.hearth/shared` and a **synthesized** `ServiceCatalog` (built from `registry.json`, not a
yaml file — `HearthManagerOptions.catalog` is already consumer-authored). `catalog.runtime_directory
= "runtime-v1"` so lock/token/state land in `~/.hearth/shared/runtime-v1` and the existing
`discover()`/`ensure()` machinery works unchanged against `root = ~/.hearth/shared`.

Each registry instance becomes one `ServiceDefinition`:
`id = "postgres@16.4"`, `ownership: daemon`, run/stop/readiness rendered from the recipe snapshot
(template vars `{installDir}`, `{dataDir}`, `{port}`). New registrations enter the running daemon
through `reload_catalog` — the old-catalog-stop rule applies unchanged.

## `catalog.json` (remote registry)

Catalog URL resolution order: `HEARTH_SHARED_CATALOG_URL` → `~/.hearth/shared/catalog-url` → the
pinned `SHARED_CATALOG_URL` in the binary. The `catalog-url` file exists because a daemon started
outside a login shell does not inherit `HEARTH_SHARED_CATALOG_URL` — write a `file://` (or
`https://`) URL there to point every daemon at a local/private catalog, including its sibling
`script` artifacts (`HEARTH_CATALOG_ORIGIN` then resolves to that catalog's own directory).

Tarball `sha256` lives in this file, so the file itself is trusted on TLS alone — fetching anywhere
else is not supported. The same file is also baked into the binary (`include_str!` in `remote.rs`)
as the last-resort fallback after the disk cache — this matters because the pinned
`raw.githubusercontent.com` URL answers 404 for a private repo, which is otherwise an unfetchable
catalog on a fresh machine.

```json
{
  "version": 1,
  "services": {
    "postgres": {
      "versions": {
        "16.4": {
          "artifacts": {
            "darwin-arm64": {
              "url": "https://example.com/postgres-16.4-darwin-arm64.tar.gz",
              "sha256": "…"
            }
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

A recipe may use `readiness: { kind: exit }` when the command is a job that runs and stops
rather than a server. While it runs the state is `running`, not `ready`. Exit 0 settles
`succeeded`. That recipe ignores `readinessTimeoutMs` and does not inherit the two-minute
readiness deadline applied to shared servers, so a long job is not killed for still running.
A port is still allocated; a command that does not listen can ignore `{port}`.

Recipe readiness uses the same shorthand as `hearth.yaml`, resolved when the instance port is
known:

- `{ "kind": "http" }` probes `GET http://127.0.0.1:{port}/health` (2xx). `"path"` replaces
  `/health` — MinIO stays `/minio/health/live` because that is the path MinIO serves. The shared
  nginx serves `/health` (and `/healthz`, the same body).
- `{ "kind": "tcp" }` with no `port` connects to the allocated primary port. MongoDB is this:
  replica-set init runs in `provision`, after the port accepts a connection.
- `{ "kind": "command" }` is for a protocol check a bare connect cannot express. Redis and Kafka
  run `{installDir}/bin/hearth-ready {port}` (`PING`, broker ApiVersions). `pg_isready` above is
  the same kind.

A recipe snapshot already stored in `registry.json` is not rewritten when `catalog.json` changes.
The shorthand applies to instances registered after the binary that understands it.

An artifact sets **one** of:

- `url` — a tarball that already exists (`https://…` or `file://` in tests). The daemon downloads it.
- `script` — a path relative to this catalog file (`scripts/catalog/pack.sh`) plus optional
  `scriptArgs`. The daemon runs `bash <script> <scriptArgs...> <out.tar.gz>`. The script does the
  packaging; the daemon does not invent an archive layout of its own. A remote catalog resolves the
  path next to `catalog.json` and downloads that script. The script is given `HEARTH_CATALOG_ORIGIN`
  so it can pull sibling files from the same place.

`sha256` is the digest of the tarball either source produced — but it is **enforced only for `url`
artifacts**. A `script` artifact's output is never byte-reproducible (tar mtimes, compile variance),
so no committed hash could ever match a consumer's rebuild; the field records the producer's hash
for reference only — the script itself is the trust boundary. `file://` artifact URLs are accepted
for tests.

Template vars: `{installDir}`, `{dataDir}`, `{port}`, `{port2}`…, `{projectId}`, `{projectDb}`,
`{projectUser}`, `{projectBucket}`, `{projectRoot}` (in `provision`/`deprovision`/`connection`).
`{port2}` exists only when the recipe reserves a second port (`additionalPorts` or `ports`).
`{projectBucket}` is `h-<projectId>` — the S3-safe form of `{projectDb}`.

Optional recipe fields beyond the example above:

- `prepare` — one command, run before every start (supervisor `preparation_command`). Idempotent.
  First-boot work (write a config, `kafka-storage format`) goes here. `run` stays the real server
  binary, because that is the command line `ps` must keep showing.
- `additionalPorts` — extra listeners reserved as one contiguous block after `{port}`. MinIO uses
  one for the console (`{port2}`); Kafka uses one for the KRaft controller.
- `ports` — explicitly pinned ports (`ports[0]` is `{port}`, the rest `{port2}`…). Skips hash
  allocation entirely; registration only checks the set is bindable. For recipes whose port IS
  the feature — the shared nginx pins `[80, 443]` so dev URLs need no port suffix. Mutually
  exclusive with `additionalPorts`. macOS keeps <1024 privileged for everyone — the shared
  nginx's public :80/:443 come from a pf rdr anchor onto its loopback pins 18080/18443.
- `extraPortLabels` — display labels for those ports, in order.

`deprovision` runs best-effort on detach — nginx uses it to remove the project's published confs.
The database recipes leave it empty: detach drops the attachment and leaves the database, bucket,
or topic in place.

## Shipped recipes

All shipped artifacts are `script` entries: `scripts/catalog/pack.sh` builds the tarball on demand,
because the publishers either do not ship a darwin-arm64 archive in the layout the recipe runs or
(MongoDB) the recipe needs payload files repacked alongside it. Nothing is published as a release
asset. Run from a remote catalog, `pack.sh` downloads the rest of `scripts/catalog` from
`$HEARTH_CATALOG_ORIGIN` using the file list in `scripts/catalog/MANIFEST`.

Versions and sha256 live in `catalog.json`. The daemon does not verify sha256 for `script`
artifacts (a rebuild is not byte-reproducible); the recorded hash is what the smoke test checks.
The pipeline, from the repo root:

- `task catalog:package` (`scripts/catalog/package.sh [service]`) — build the tarballs and their
  `.sha256` files into `dist/catalog/`.
- `task catalog:write` (`scripts/catalog/write-catalog.py`) — regenerate `catalog.json` and
  `scripts/catalog/MANIFEST` from those `.sha256` files. Re-run it after adding a payload file.
- `task catalog:smoke` (`scripts/catalog/smoke.sh`) — attach every shipped service against the
  local tarballs under a throwaway `HOME`, and fail if `MANIFEST` is stale.

Every listener is `127.0.0.1`. There is no per-project auth; MinIO's root credentials are the fixed
dev pair `hearth` / `hearth-local-dev`.

| Service | What `start` provisions | Connection |
|---|---|---|
| redis | nothing — every project shares DB 0 | `redis://127.0.0.1:{port}/0` |
| mongodb | database name `h_<projectId>`; single-node replica set `rs0` initiated idempotently on first attach so transactions work; `--wiredTigerCacheSizeGB 0.25` | `mongodb://127.0.0.1:{port}/{projectDb}?replicaSet=rs0&directConnection=true` |
| minio | bucket `h-<projectId>` (hyphen: S3 names reject `_`); API on `{port}`, console on `{port2}` | `http://127.0.0.1:{port}` plus `S3_*` / `AWS_*` env |
| nginx | a `location /<projectId>/` + `www/<projectId>/` drop-in — or, when the project attaches with a conf arg, the project's rendered conf tree under `conf.d/servers/<projectId>/` (whole server blocks; see project-side `attachArgs` below) | `http://127.0.0.1:{port}/{projectId}/` |
| kafka | topic `h_<projectId>` on a single KRaft broker; controller on `{port2}` | `127.0.0.1:{port}` plus `KAFKA_BROKERS` / `KAFKA_TOPIC` |

Kafka's tarball bundles a Temurin 21 JRE, so the broker does not need `java` on `PATH`. Nginx is
built from the stable source with vendored PCRE2 and zlib. Redis is built from the upstream tag.
MongoDB's tarball repacks the official darwin-arm64 `mongod` plus `mongosh` (needed by the
replica-set `provision` hook). MinIO and `mc` are built from
their pinned release tags (`CGO_ENABLED=0`), not downloaded from `dl.min.io`.

## Port allocation

`base = 43100 + sha256(name@version)[..8] mod 900`. At instance registration, if `base` is taken by
another registry entry or a live listener, probe `base+1…` (wrapping inside the range); the winner
is persisted in `registry.json`. A recipe with `additionalPorts: N` takes a contiguous block of
`1+N` ports that does not wrap off the end of the range; every port in the block counts as taken.
Deterministic-first keeps the common case stateless.

A recipe with `ports: […]` skips all of that — the declared ports are registered verbatim after a
bind check. The shared nginx pins `[18080, 18443]`. macOS keeps ports below 1024 privileged for
every user, so a pf rdr anchor maps public `:80`/`:443` onto those loopback pins.

## Project side

```yaml
version: 1
shared:
  postgres: "16.4"        # exact version; must exist in catalog.json at attach time
  nginx:                  # the object form adds per-project hooks:
    version: "1.30.5"
    # project-side, run before every attach — e.g. render the conf tree the recipe publishes
    preparationCommand: { command: { argv: ["{projectRoot}/scripts/prepare-edge.sh", "{dataDir}", "{projectRoot}"] } }
    # appended to the attach argv → forwarded onto the recipe's provision commands
    attachArgs: ["{dataDir}/conf"]
    urls: [{ url: "https://dev.local/", label: dev }]
services: { … }
```

`preparationCommand`/`attachArgs`/`urls` render `{dataDir}` (the project service's data dir),
`{serviceId}`, and `{projectRoot}` like an `artifact:` service — `{port}` is not substituted
here: the instance's ports live in the smp registry, not in this file. For the shared nginx the
ports are pinned anyway, so project confs can hardcode 80/443.

`config_file` generates, per entry, a normal `ServiceDefinition` (id = the map key):

- `kind: infrastructure`, `ownership: external`, `label: "postgres@16.4 (shared)"`
- `run`: `argv [hearth, "shared", "attach", "postgres@16.4"]` — a **one-shot task**: ensure smp,
  install, start, attach, provision, print conninfo to stdout (→ `hearth logs postgres`).
  `hearth` resolves to the daemon's own `current_exe` — never PATH.
- `readiness`: `{ kind: command, command: { argv: [hearth, "shared", "probe", "postgres@16.4"] } }` —
  exit 0 iff the instance is ready **and this project is attached** (projectId = sha256 of the
  canonical project root, derived from the probe's cwd). This makes detach→release work through
  plain `syncExternalServices`.
- `stop`: `argv [hearth, "shared", "detach", "postgres@16.4"]` — releases the attachment; the
  instance keeps running.
- `readinessTimeoutMs`: 49 min default. The project-side probe has to out-wait a script artifact pack (45 min) plus extract and the instance readiness budget. The instance server itself stays at 2 min.
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

## CLI — `hearth shared …` (no project catalog required)

`ensure --json` (the `manager ensure --json` contract for smp — prints the `ManagerConnection`
for `~/.hearth/shared`, used by `hearth tui`) · `list [--json]` (remote registry, no daemon
needed) · `installed` · `status [--json]` · `start|stop <name@ver>` · `attach|detach|probe
<name@ver>` · `install <name@ver>` · `remove <name@ver>`.

## MCP + skill

Skill scripts / CLI cover `shared list`, `shared status`, and `scripts/shared-connection.sh` (MCP tools of the same names remain in-binary but are retired for agents) (queried against smp
directly via its lock-dir token). The skill doc (`hearth skill install`) documents the flow:
read `shared:` in `hearth.yaml` → attach happens on `start` → query connection info via MCP.

## TUI

`hearth tui` has a shared-services view. It reads recipes from the remote catalog and instances
from a live smp daemon or the local registry, and it does not spawn smp just to draw the list.
Install follows the version selected on that row and uses an unbounded request timeout. Start,
stop, and restart act on one instance. Remove takes a second keypress; when projects are still
attached, confirming sends `force: true` and deletes their data. Attach and detach stay
project-side: a `shared:` entry plus start or stop.

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
