# hearth

Local dev services manager: one long-lived daemon per project folder, plus a CLI, TUI, MCP server,
and a macOS app. All of them talk to the daemon over loopback HTTP+SSE. The daemon owns every
managed process; nothing else starts or stops one directly.

The product is the compiled `hearthd` binary (`rust/bin/hearthd`) and the SwiftUI app in `apps/macos`.

```
 your CLI  ─┐
 your TUI   ├──HTTP + SSE (loopback)──►  daemon (hearthd)
 your MCP  ─┘                                  │
 the macOS app  ───────────────────────────────┘
                                        ProcessSupervisor (spawns/probes/tails)
```

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
    readinessTimeoutMs: 30000
    ports: [{ port: 6060, label: pprof }]
    urls:
      - http://127.0.0.1:8080
      - { url: "https://{tailnetHost}:8443", label: admin, requiresRunning: false }
```

Install `hearthd` into the app bundle, then drive a project:

```bash
task rust:install   # copies rust/target/release/hearthd into Hearth.app
"/Applications/Hearth.app/Contents/Resources/hearthd/bin/hearthd" --root /path/to/project status
"/Applications/Hearth.app/Contents/Resources/hearthd/bin/hearthd" --root /path/to/project tui
"/Applications/Hearth.app/Contents/Resources/hearthd/bin/hearthd" --root /path/to/project mcp
```

`hearthd manager ensure --json` is the connection contract for a non-terminal client (the macOS app's
sidecar): it ensures a daemon is running for `--root` and prints `{instanceId, port, token,
protocolVersion, runtimeDirectory, root}`. `hearthd manager restart --json` prints the same payload
for a freshly started daemon, replacing the old one *without* stopping its services — they are
detached, and the new daemon re-adopts them from their persisted identities.

Consumer scripts must use the app-bundled absolute path.

## Build and test

```bash
cd rust
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

The suite needs a running `docker` daemon, `tailscale`, plus `nc`, `ps`, and `sh`.

```bash
task macos:install   # package Hearth.app into /Applications and launch it
```

See [AGENTS.md](AGENTS.md) for architecture, sharp edges, and how to refresh the bundled binary.
