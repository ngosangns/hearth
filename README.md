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

## Requirements

Bun-only, v1. This package uses `Bun.serve`, `Bun.spawn` (detached, argv-only, process-group
signaling) and `bun:test` directly — there is no Node runtime fallback. Node projects can still
shell out to it via `@gnasdev/local-services/node-bridge` (see below).

## Subpath exports

| Subpath | What it is |
|---|---|
| `@gnasdev/local-services/core` | `LocalServicesManager` (the daemon engine), catalog types + `validateCatalog`/`dependencyLevels`, `runDoctor`, `runDaemon`/`DaemonLifecycle` |
| `@gnasdev/local-services/cli` | `main()` — the `localctl`-style command surface (`status`, `logs`, `start`/`stop`/`restart`, `operation`, `doctor`, `cleanup`, `manager`, `tui`) |
| `@gnasdev/local-services/tui` | `runTui()` — a [`pi-tui`](https://www.npmjs.com/package/@oh-my-pi/pi-tui)-based terminal app, just another HTTP+SSE client of the daemon |
| `@gnasdev/local-services/mcp` | `createLocalServicesMcpServer()` — a 5-tool MCP server (status/logs/trace/events/manage) for AI agents |
| `@gnasdev/local-services/node-bridge` | A ~20-line Node-only adapter so a Node/oclif task runner can shell out to this Bun-only tool without depending on Bun itself |

All five ship as TypeScript source (no build step) and are resolved natively by Bun — see
[§ Versioning](#versioning) for why.

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

## Development

```bash
bun install
bun test
bun run typecheck
```

## CI / publishing

PRs run `bun test` against `{ self-hosted, macmini }`. Pushing a version tag (`vX.Y.Z`) publishes to
GitHub Packages (`https://npm.pkg.github.com`) — publishing is not automatic on every merge to
`main`, only on a tag.
