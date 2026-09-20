# @gnasdev/local-services

Local dev services manager (daemon, TUI, CLI, MCP) for Bun projects.

A single long-lived background process (the *daemon*) owns the truth about every service your
project's local dev environment needs — infra containers, backends, frontends, whatever else — and
three front doors talk to it over loopback HTTP+SSE: a CLI, a terminal UI, and an MCP server for AI
agents. Nothing else is ever allowed to touch a managed process directly.

```
 your CLI  ─┐
 your TUI   ├──HTTP + SSE (loopback)──►  daemon (LocalServicesManager)
 your MCP  ─┘                                  │
                                        ProcessSupervisor (spawns/probes/tails)
```

This package ships the generic engine only. Every project supplies its own `ServiceCatalog` —
the list of services, how to start them, and how to tell when they're ready — and passes it to
every entry point below. The catalog is the one thing every layer takes as a parameter; nothing in
this package hardcodes a project's services.

## Two implementations

This repo ships the product twice, and both are live:

- **TypeScript (`src/`)** — what this README documents, and what the npm package publishes.
- **Rust (`rust/`)** — a full rewrite at feature parity, compiled to a standalone `lsd` binary. It
  speaks the same HTTP+SSE protocol and reads/writes the same state, so it is a drop-in replacement
  for a consumer that would rather install one binary than depend on Bun. `apps/macos` and `infra`
  use it; `viclass` is still on the TypeScript build.

The Rust binary additionally has `lsd mcp` (serve MCP over stdio), `lsd mcp install` and
`lsd skill install`, which the TypeScript CLI does not. See [AGENTS.md](AGENTS.md) for how the two
relate, how to build and install the binary, and which differences are deliberate.

## Requirements

Bun-only, v1. This package uses `Bun.serve`, `Bun.spawn` (detached, argv-only, process-group
signaling) and `bun:test` directly — there is no Node runtime fallback. Node projects can still
shell out to it via `@gnasdev/local-services/node-bridge` (see below), or spawn the compiled Rust
`lsd` binary, which needs no Bun at all unless the project's catalog uses the `.config.ts` escape
hatch.

## Subpath exports

| Subpath | What it is |
|---|---|
| `@gnasdev/local-services/core` | `LocalServicesManager` (the daemon engine), catalog types + `validateCatalog`/`dependencyLevels`, `runDoctor`, `runDaemon`/`DaemonLifecycle`, `loadCatalog`/`findConfigFile` (declarative config), `resolveBaseEnvironment` (login-shell/`.env` resolution) |
| `@gnasdev/local-services/cli` | `main()` — the `localctl`-style command surface (`status`, `logs`, `start`/`stop`/`restart`, `operation`, `doctor`, `cleanup`, `manager`, `tui`) |
| `@gnasdev/local-services/tui` | `runTui()` — a [`pi-tui`](https://www.npmjs.com/package/@oh-my-pi/pi-tui)-based terminal app, just another HTTP+SSE client of the daemon |
| `@gnasdev/local-services/mcp` | `createLocalServicesMcpServer()` — a 5-tool MCP server (status/logs/trace/events/manage) for AI agents |
| `@gnasdev/local-services/node-bridge` | A ~20-line Node-only adapter so a Node/oclif task runner can shell out to this Bun-only tool without depending on Bun itself |
| `lsd` (`bin`) | A generic CLI+daemon binary for a project that authors its catalog as `local-services.yaml` — see [§ Declarative config](#declarative-config-local-servicesyaml-and-the-lsd-binary) — instead of writing its own `catalog.ts`/`daemon.ts`/`cli.ts` |

All five subpaths ship as TypeScript source (no build step) and are resolved natively by Bun — see
[§ Versioning](#versioning) for why. `lsd` is the same: a `#!/usr/bin/env bun` script, run directly.

## Minimal consumer example

Every consumer owns exactly one file: its `ServiceCatalog`. Everything else is this package.

```ts
// catalog.ts
import type { ServiceCatalog } from "@gnasdev/local-services/core";

export const catalog: ServiceCatalog = {
  startFailurePolicy: "stop-on-first-failure-keep-started",
  groups: { all: ["redis", "api"] },
  services: [
    {
      id: "redis",
      kind: "infrastructure",
      profiles: {
        run: {
          commandStatus: "verified",
          command: { command: { argv: ["docker", "compose", "up", "-d", "redis"] }, cwd: ".", containerName: "myapp-redis" },
          readiness: { kind: "container" },
        },
      },
    },
    {
      id: "api",
      dependencies: ["redis"],
      profiles: {
        run: {
          commandStatus: "verified",
          command: { command: { argv: ["task", "api:dev"] }, cwd: "apps/api" },
          readiness: { kind: "tcp", port: 8080 },
        },
      },
    },
  ],
};
```

```ts
// daemon.ts — the entry the CLI spawns detached
import { runDaemon } from "@gnasdev/local-services/core";
import { catalog } from "./catalog";

const rootIndex = process.argv.indexOf("--root");
const root = rootIndex >= 0 ? process.argv[rootIndex + 1]! : process.cwd();
await runDaemon({ root, catalog });
```

```ts
// cli.ts — your project's bin entry
import { main, type LocalctlOptions } from "@gnasdev/local-services/cli";
import { catalog } from "./catalog";

const options: LocalctlOptions = {
  catalog,
  spawnDaemon: (root) => {
    const proc = Bun.spawn(["bun", "run", new URL("./daemon.ts", import.meta.url).pathname, "--root", root], { cwd: root, stdout: "ignore", stderr: "ignore", stdin: "ignore", detached: true });
    proc.unref();
  },
};

process.exitCode = await main(options);
```

```bash
bun run cli.ts status        # one-shot status of every service
bun run cli.ts start api     # start api (and its redis dependency)
bun run cli.ts doctor        # environment checks
```

A minimal MCP entry looks like:

```ts
// mcp.ts
import { StdioServerTransport } from "@modelcontextprotocol/sdk/server/stdio.js";
import { createLocalServicesMcpServer, ManagerApiClient } from "@gnasdev/local-services/mcp";
import { catalog } from "./catalog";

const options = { catalog, spawnDaemon: (root: string) => { /* same as cli.ts */ } };
const server = createLocalServicesMcpServer(new ManagerApiClient(process.cwd(), options), {
  name: "myapp-local-services",
  toolPrefix: "myapp_local_services_",
  knownServiceIds: catalog.services.map((s) => s.id),
});
await server.connect(new StdioServerTransport());
```

## Declarative config: `local-services.yaml` and the `lsd` binary

The "one file per consumer" pattern above is still the full-power option, but a project that doesn't
need a `custom` readiness probe closure or computed service lists can skip writing `catalog.ts` /
`daemon.ts` / `cli.ts` entirely. Drop a `local-services.yaml` (`.yml`/`.json` also work; JSON is valid
YAML) in the project root:

```yaml
version: 1
env: { NODE_ENV: development } # merged into every service's environment
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
    dependsOn: [redis]
    cwd: apps/api
    build: { argv: [go, build, ./...], timeoutMs: 120000, serializationKey: go }
    run: { shell: "air -c .air.toml", exec: true }
    readiness: { kind: tcp, port: 8080 }
    ports: [{ port: 6060, label: pprof }]
```

and drive it with this package's own `lsd` binary (installed as this package's `bin`; `bunx
@gnasdev/local-services lsd status`, or `lsd` directly once installed) — it loads the catalog from
that file and behaves exactly like a project-authored `cli.ts`/`daemon.ts` pair:

```bash
lsd status              # one-shot status of every service
lsd start api --wait    # start api (and its redis dependency)
lsd manager ensure --json   # spawn the daemon if needed; print {instanceId, port, token, protocolVersion, runtimeDirectory, root}
lsd daemon --root .     # the daemon entrypoint lsd spawns itself, detached — not usually invoked by hand
```

`manager ensure --json` is the connection contract for a non-Bun client (a desktop app's sidecar
process, say): it ensures a daemon is running for `--root` and prints everything needed to talk to it
directly over HTTP+SSE, without that client re-implementing this package's lock-file discovery.

For anything the declarative shape can't express, drop to `local-services.config.ts` instead (same
filename slot, checked last) — it must `export const catalog` (or a default export) as a hand-authored
`ServiceCatalog`, same as the `catalog.ts` in the example above.

Two related, independently-usable pieces from `/core`:

- **`{ kind: "command" }` readiness** — a declarative, JSON-serializable stand-in for `custom`: exit
  code 0 means ready, anything else means not-ready-yet (never a terminal failure; it keeps retrying
  the same way `tcp`/`http` do until the readiness timeout). `readiness: { kind: "command", command: {
  argv: [...] }, cwd?: "..." }`.
- **`resolveBaseEnvironment` (`./env`)** — a daemon started from a GUI (Finder/Dock/a LaunchAgent, as a
  desktop app's sidecar would be) inherits a bare `PATH`, not the login-shell customization (`nvm`,
  Homebrew, project `.env` files) a terminal-launched daemon gets for free from `process.env`. Resolve
  it once and pass it into `defaultSupervisorOptions(root, runtimeDirectory, baseEnvironment)` so every
  spawned service sees the same environment regardless of how the daemon itself was started — `lsd
  daemon` already does this.

## Hot-reloading the catalog

`POST /v1/manager/reload` (body: `{ requestId, catalog }`, a full `ServiceCatalog` as plain JSON — so
it cannot carry a `custom` readiness closure, only what a declarative config can already express)
validates the new catalog and swaps it in. A service removed from the new catalog that is currently
active gets stopped (using the *old* catalog to do it, so a removed-but-still-stopping service is
never briefly invisible from `/v1/services` while its process is still alive); `external`-owned
services are never touched. A service that stays present but whose definition changed is left running
as-is — reload never restarts a healthy service out from under a developer — and its id is reported
back in the response's `changed` array so a caller can decide whether/when to restart it itself. The
same logic is available in-process as `LocalServicesManager#reloadCatalog(catalog)`, for a daemon that
watches its own config file (e.g. via FSEvents) and wants to reload without a self-HTTP round trip.
`GET /v1/catalog` returns the catalog currently in effect.

## `CommandSpec` — argv vs. shell

```ts
type CommandSpec = { argv: readonly string[] } | { shell: string; exec?: boolean };
```

`argv` is injection-safe (spawned directly, no shell) and is the recommended default. `shell` exists
for build tools that need chaining (`cd -- '...' && exec ...`); set `exec: true` when the shell
command itself execs into the long-running process.

## `ReadinessSpec`

```ts
type ReadinessSpec =
  | { kind: "process" } | { kind: "tcp"; port: number } | { kind: "http"; url: string }
  | { kind: "container" } | { kind: "tailnet" }
  | { kind: "command"; command: CommandSpec; cwd?: string } // exit 0 = ready; JSON-serializable, so config-file-friendly
  | { kind: "custom"; name: string; probe: (ctx: { serviceId: string }) => Promise<"ready" | "not-ready" | "failed"> };
```

## `ownership: "daemon" | "external"`

Most services are `"daemon"`-owned (the default): this manager spawns and stops them. Mark a service
`"external"` when it can *also* be brought up outside the daemon (e.g. a `docker compose up` a
developer runs by hand, or a tailnet serve config) — the daemon polls its readiness and
adopts/releases it into its own state machine, and it never blocks or is touched by a manager
shutdown.

## TUI

`@gnasdev/local-services/tui` exports `runTui({ root, catalog, spawnDaemon })`, a
[`pi-tui`](https://www.npmjs.com/package/@oh-my-pi/pi-tui)-based terminal app that is just another
HTTP+SSE client of the daemon (same `↑/k`/`↓/j` select, `Enter`/`Space` toggle, `x` stop, `r`/`R`
restart, `a` start all, `s` stop all, `q`/`Ctrl-C` quit keybindings as the CLI's `tui` command). Wire
it in as the `/cli` subpath's `tui` runtime handler:

```ts
// cli.ts
import { main, type LocalctlOptions, type LocalctlRuntime } from "@gnasdev/local-services/cli";
import { runTui } from "@gnasdev/local-services/tui";
import { catalog } from "./catalog";

const options: LocalctlOptions = { catalog, spawnDaemon /* as in the minimal example above */ };
const runtime: LocalctlRuntime = { tui: (root) => runTui({ root, catalog, spawnDaemon: options.spawnDaemon }) };

process.exitCode = await main(options, process.argv.slice(2), runtime);
```

Resolves with an exit code once the user quits. `runTui` calls `ensure()` itself, so it also works
standalone without going through `local-services tui`.

## Versioning

Semver, and the package version is also the protocol compatibility signal: `PROTOCOL_VERSION` ships
from `/core` as the one place a daemon and its TUI/CLI/MCP clients read it from. A daemon and a
client built against different `PROTOCOL_VERSION`s refuse to talk to each other rather than silently
misbehaving. A `PROTOCOL_VERSION` bump is always a **major** release.

## macOS app

`apps/macos` is a native SwiftUI front door — add a folder, trust it, manage its services from a
window instead of a terminal. Same architecture, just another client of a per-folder daemon; see
[apps/macos/README.md](apps/macos/README.md) for what's implemented, what isn't yet, and how to build
it.

## Development

```bash
bun install
bun test
bun run typecheck
```

The Rust workspace is separate, and needs `bun`, a running `docker`, and `tailscale` on the machine
for its real-adapter tests:

```bash
cd rust
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

`task --list` shows both, plus `task rust:install` (build the `lsd` binary in release mode, install
it to `/opt/homebrew/bin`, ad-hoc sign it) and the `macos:*` targets.

> **Careful with a bare `lsd` in a package.json script.** `bun run` prepends `node_modules/.bin` to
> PATH and this package ships its own `lsd` bin, so a bare `lsd` resolves to the TypeScript shim —
> not the installed Rust binary, which has a different command surface. Use an absolute path.

## CI / publishing

PRs run `bun test` and `bun run typecheck` against `{ self-hosted, macmini }`, plus a separate
`rust-test` job (`cargo test` + `cargo clippy`). Pushing a version tag (`vX.Y.Z`) publishes to
GitHub Packages (`https://npm.pkg.github.com`) — publishing is not automatic on every merge to
`main`, only on a tag, and it depends only on the TypeScript jobs. The Rust binary has no
distribution process of its own yet: it is installed by hand or bundled into the macOS app.
