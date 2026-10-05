<p align="center"><img src="docs/icon.png" alt="Hearth" width="128"></p>

# hearth

Local dev services manager: one long-lived daemon per project folder, plus a CLI, TUI, and MCP
server. All of them talk to the daemon over loopback HTTP+SSE. The daemon owns every managed
process; nothing else starts or stops one directly.

The product is the compiled `hearth` binary (`rust/bin/hearth`). `hearth tui` is the workspace
UI. It runs from any directory.

```
 your CLI  ─┐
 your TUI   ├──HTTP + SSE (loopback)──►  daemon (hearth)
 your MCP  ─┘                                  │
                                        ProcessSupervisor (spawns/probes/tails)
```

```
hearth tui
```

The TUI lists workspaces, starts and stops services, tails logs, and manages shared services.
A new folder stays untrusted until a second Enter, and an untrusted folder does not spawn a
daemon. A daemon you stop stays stopped until you press Enter on that workspace again.
**Kill & Start**, and removing a shared instance that still has project attachments, both take
a second keypress. Check for updates opens this repo's GitHub Releases page.
`hearth update` installs the latest binary. Add a folder by
its absolute path. The workspace list stays in
`~/Library/Application Support/HearthApp/workspaces.json`.

A project supplies a `hearth.yaml` (`.yml` / `.json` also work) naming its services, how to
start them, and how to tell when they are ready. TypeScript catalogs are not accepted.

```yaml
version: 1
env: { NODE_ENV: development }
groups:
  all: [redis, api]
services:
  redis:
    kind: infrastructure
    ownership: external
    container: myapp-redis
    run: { argv: [docker, compose, up, -d, redis] }
    readiness: { kind: container }
  api:
    cwd: apps/api
    build: { argv: [go, build, ./...], timeoutMs: 120000, serializationKey: go }
    run: { shell: "air -c .air.toml", exec: true }
    readiness: { kind: tcp, port: 8080 }
    readinessTimeoutMs: 30000 # accepted; does not fail the service
    ports: [{ port: 6060, label: pprof }]
    urls:
      - http://127.0.0.1:8080
      - { url: "https://{tailnetHost}:8443", label: admin, requiresRunning: false }
```

`groups:` members may also name other groups — `all: [infra, app]` expands depth-first in
declaration order (deduplicated, cycles rejected at load). A member naming both a service and a
group resolves as the service. The TUI groups its service list by direct membership and
offers per-group start and stop.

`disabled: true` on a service keeps it in the catalog but out of every lifecycle action: direct
start/stop/restart is rejected (`service_disabled`) and group targets expand past it. It does not
stop a service already running — stop it before disabling, or leave it be.

A command that runs and then exits — a one-shot build, not a server — uses
`readiness: { kind: exit }`. The row stays `running` until the process exits. It does not
become `ready`. Exit 0 becomes `succeeded` and is not run again until you start it; any other
exit is `failed`. `readinessTimeoutMs` does not apply: the command is not failed for still
running.

```yaml
services:
  fe:
    cwd: viclass
    run: { argv: [task, fe] }
    readiness: { kind: exit }
```

Readiness is a probe, not a deadline. After the process is up, hearth probes every 1.5s until the service is stopped or restarted, the process exits, or the daemon shuts down. A passing probe is `ready`. A failing probe is `running-unready` and leaves the process running. `hearth start --wait` returns once the process is up, whether or not the probe has passed yet. `readinessTimeoutMs` is still accepted. It does not fail a long-lived service. It bounds a single `command` probe, and a one-shot shared attach.

| Kind | Ready when | Write it |
|---|---|---|
| `http` | `GET` returns 2xx | `{ kind: http }` is `http://127.0.0.1:<port>/health`. Set `path` for any other path (`/metrics`, `/minio/health/live`). Set `url` when the host or port is not that default. `url` cannot be combined with `path` or `port`. Probed every 1.5s |
| `tcp` | the port accepts a connection | `{ kind: tcp, port: 4222 }`, or `{ kind: tcp }` when `ports:` already names the port. Probed every 1.5s |
| `command` | the command exits 0 | a real protocol check (`redis-cli ping`, `pg_isready`). Probed every 1.5s. One probe is bounded by `readinessTimeoutMs` |
| `exit` | the run command itself exits | one-shot builds. The row is `running` until exit. Exit 0 is `succeeded`. No readiness probe and no deadline. The wait polls every 1.5s |
| `container` | the named container is running | docker compose services. Probed every 1.5s |
| `process` | the process is alive | no port and no HTTP. No probe after start |
| `tailnet` | Tailscale serve is up | tailnet tasks. Probed every 1.5s |

`http` and `tcp` with no port use the first `ports:` entry. Omitting both is a load error (`needs a port`). Do not point `http` at `/health` unless that process serves the path.

A service may declare a versioned tarball to install before its first start — same download/script
→ sha256 → extract → marker machinery the shared-services catalog uses, scoped to the project's
runtime directory instead of `~/.hearth/shared`:

```yaml
services:
  postgres:
    artifact:
      version: "16.4"
      url: "https://example.com/postgres-16.4-darwin-arm64.tar.gz"  # or script: packaging/pg-pack.sh
      sha256: "…"                                                  # required for url artifacts
    run: { argv: ["{installDir}/bin/postgres", "-D", "{dataDir}", "-p", "{port}"] }
    readiness: { kind: tcp, port: 5432 }
```

`{installDir}` (`<runtimeDir>/installs/<service>/<version>`), `{dataDir}`
(`<runtimeDir>/data/<service>`), `{port}`/`{port2}…` (declared `ports:`, else the tcp readiness
port for `{port}`), `{serviceId}` and `{projectRoot}` render into run/stop/build/preparation
commands, env values, readiness and urls at load time. The install runs once per version, before
preparation and build; an `external` or run-less service cannot declare an `artifact:`.

A `shared:` block registers machine-global singletons (postgres, redis, …) installed on the host
under `~/.hearth/shared` and run by a separate global daemon (`hearth smp`). Every repo that
registers the same `name@version` shares one instance; different versions run side by side.
Shared entries show up as ordinary `infrastructure` services — `start` attaches this project
(first start installs the service), `stop` only detaches, and connection info is read via the
`local_services_shared_*` MCP tools or `hearth shared status`. See `docs/shared-services.md`.

```yaml
shared:
  redis: "8.2.10"
  mongodb: "8.0.32"
  minio: "RELEASE.2025-10-15T17-29-55Z"
  kafka: "4.3.1"
  # object form: project hooks before attach + args forwarded to recipe provision + urls
  nginx:
    version: "1.30.5"
    preparationCommand: { command: { argv: ["{projectRoot}/scripts/prepare-edge.sh", "{dataDir}", "{projectRoot}"] } }
    attachArgs: ["{dataDir}/conf"]   # e.g. the shared nginx publishes this conf tree
    urls: [{ url: "https://dev.local/", label: dev }]
```

The shared `nginx` pins `127.0.0.1:18080` + `:18443`; a pf rdr anchor exposes them publicly as
`:80`/`:443` (macOS keeps <1024 privileged for everyone — the redirect is the way around it):

```bash
printf '%s\n' \
  'rdr pass on lo0 inet proto tcp from any to 127.0.0.1 port 80  -> 127.0.0.1 port 18080' \
  'rdr pass on lo0 inet proto tcp from any to 127.0.0.1 port 443 -> 127.0.0.1 port 18443' \
  | sudo pfctl -a com.apple/hearth -f - && sudo pfctl -e
```

A plain attach gets a
`/<projectId>/` location + `www/` docroot; an attach with a conf source (dir →
`conf.d/servers/<projectId>/`, file → `conf.d/servers/<projectId>.conf`) mounts whole server
blocks into the shared instance — `nginx -t` validates before reload, so a bad drop fails the
attach instead of wedging the singleton. Detach removes the project's confs via `deprovision`.

Install `hearth` onto `PATH`, then drive a project. `task install` builds the release binary
into `~/.local/share/hearth/bin/hearth-<version>`, ad-hoc signs that file, and points
`~/.local/bin/hearth` at it. `hearth update` installs the latest GitHub release into the same
layout. A daemon that is already running keeps its old binary until it is restarted.

```bash
task install          # versioned binary + symlink at ~/.local/bin/hearth
hearth update --check
hearth update
hearth --root /path/to/project status
hearth tui           # workspaces and shared services; works from any directory
hearth --root /path/to/project tui   # also adopts that project when it has a catalog
hearth --root /path/to/project mcp
```

`hearth manager ensure --json` is the connection contract for another client: it ensures a daemon
is running for `--root` and prints `{instanceId, port, token, protocolVersion, runtimeDirectory,
root}`. `hearth manager restart --json` prints the same payload for a freshly started daemon,
replacing the old one *without* stopping its services — they are detached, and the new daemon
re-adopts them from their persisted identities.

## Build and test

```bash
cd rust
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

The suite needs a running `docker` daemon, `tailscale`, plus `nc`, `ps`, and `sh`.

See [AGENTS.md](AGENTS.md) for architecture, sharp edges, and how `task install` refreshes the binary.
Shared services are specified in [docs/shared-services.md](docs/shared-services.md).

## Release

The version string is `rust/bin/hearth/Cargo.toml`. Pushing a `v*.*.*` tag runs CI
(`.github/workflows/ci.yml`). A successful tag run starts `.github/workflows/release.yml`, which
runs `cargo build --release -p hearth`, ad-hoc signs the binary, and uploads `hearth-<tag>` to the
GitHub Release. A red CI run does not publish. `hearth update` installs that asset.
