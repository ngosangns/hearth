# local-services

Local dev services manager: one long-lived daemon per project folder, plus a CLI, TUI, MCP server,
and a macOS app. All of them talk to the daemon over loopback HTTP+SSE. The daemon owns every
managed process; nothing else starts or stops one directly.

The product is the compiled `lsd` binary (`rust/bin/lsd`) and the SwiftUI app in `apps/macos`.

```
 your CLI  ─┐
 your TUI   ├──HTTP + SSE (loopback)──►  daemon (lsd)
 your MCP  ─┘                                  │
 the macOS app  ───────────────────────────────┘
                                        ProcessSupervisor (spawns/probes/tails)
```

A project supplies a `local-services.yaml` (`.yml` / `.json` also work) naming its services, how to
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

Install `lsd` into the app bundle, then drive a project:

```bash
task rust:install   # copies rust/target/release/lsd into Local Services.app
"/Applications/Local Services.app/Contents/Resources/lsd/bin/lsd" --root /path/to/project status
"/Applications/Local Services.app/Contents/Resources/lsd/bin/lsd" --root /path/to/project tui
"/Applications/Local Services.app/Contents/Resources/lsd/bin/lsd" --root /path/to/project mcp
```

`lsd manager ensure --json` is the connection contract for a non-terminal client (the macOS app's
sidecar): it ensures a daemon is running for `--root` and prints `{instanceId, port, token,
protocolVersion, runtimeDirectory, root}`.

Do not put `lsd` on Homebrew's PATH — that name belongs to the lsdeluxe formula. Consumer scripts
must use the app-bundled absolute path.

## Build and test

```bash
cd rust
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

The suite needs a running `docker` daemon, `tailscale`, plus `nc`, `ps`, and `sh`.

```bash
task macos:install   # package Local Services.app into /Applications and launch it
```

See [AGENTS.md](AGENTS.md) for architecture, sharp edges, and how to refresh the bundled binary.
