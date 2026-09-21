#!/usr/bin/env bun
// Generic host binary for a project whose catalog is authored declaratively (see
// `../core/config-file.ts`): a `local-services.yaml`/`.yml`/`.json` in the project root
// is all a consumer needs to write. Every other package.json-listed entry point (`/core`, `/cli`,
// `/tui`, `/mcp`) still takes a caller-supplied catalog as a parameter and is unaffected; this binary
// is just one more caller, generic instead of project-specific — see the README's "every consumer
// owns exactly one file" example, which this replaces with zero files for a project that only needs
// the declarative shape.
//
// `lsd daemon --root <path>` is the daemon entrypoint this binary spawns detached (mirrors the
// README's `daemon.ts`); every other subcommand delegates to `../cli/localctl`'s `main()`, exactly
// like the README's `cli.ts`. A desktop app (or anything else that isn't itself a Bun/TS project)
// spawns this same binary as its sidecar rather than reimplementing catalog loading, lock discovery,
// or the daemon lifecycle.

import { main, type LocalctlOptions, type LocalctlRuntime } from "../cli/localctl";
import { loadCatalog, type ConfigFileLoadResult } from "../core/config-file";
import { runDaemon } from "../core/daemon";
import { resolveBaseEnvironment } from "../core/env";
import { resolveRuntimeDirectory } from "../core/paths";
import { defaultSupervisorOptions } from "../core/supervisor";

/** Mirrors `main()`'s own `--root` extraction exactly (a leading `--root <path>`, nothing more
 * lenient) so this binary and `main()` always agree on which project root a given invocation means. */
function extractRoot(argv: readonly string[]): { root: string; rest: string[] } {
  if (argv[0] === "--root") return { root: argv[1] ?? process.cwd(), rest: argv.slice(2) };
  return { root: process.cwd(), rest: [...argv] };
}

function reportConfigError(root: string, loaded: Extract<ConfigFileLoadResult, { ok: false }>): void {
  console.error(`lsd: could not load a service catalog for ${root}`);
  for (const error of loaded.errors) console.error(`  - ${error}`);
}

async function runDaemonSubcommand(argv: readonly string[]): Promise<number> {
  const { root, rest } = extractRoot(argv);
  if (rest.length) {
    console.error("usage: lsd daemon --root <path>");
    return 2;
  }
  const loaded = await loadCatalog(root);
  if (!loaded.ok) {
    reportConfigError(root, loaded);
    return 1;
  }
  const runtimeDirectory = resolveRuntimeDirectory(root, loaded.catalog.runtimeDirectory);
  const baseEnvironment = await resolveBaseEnvironment({ root });
  await runDaemon({ root, catalog: loaded.catalog, runtimeDirectory, supervisor: defaultSupervisorOptions(root, runtimeDirectory, baseEnvironment) });
  return 0;
}

function spawnDaemon(root: string): void {
  const proc = Bun.spawn(["bun", "run", import.meta.path, "daemon", "--root", root], { cwd: root, stdout: "ignore", stderr: "ignore", stdin: "ignore", detached: true });
  proc.unref();
}

async function runCli(argv: readonly string[]): Promise<number> {
  const { root } = extractRoot(argv);
  const loaded = await loadCatalog(root);
  if (!loaded.ok) {
    reportConfigError(root, loaded);
    return 1;
  }
  const options: LocalctlOptions = { catalog: loaded.catalog, spawnDaemon };
  const runtime: LocalctlRuntime = {
    // `tui` is only wired in when the caller actually reaches for it, so a plain `lsd status` never
    // pays for importing `@oh-my-pi/pi-tui` at all.
    tui: async (tuiRoot) => {
      const { runTui } = await import("../tui");
      return runTui({ root: tuiRoot, catalog: options.catalog, spawnDaemon });
    },
  };
  return main(options, [...argv], runtime);
}

async function run(): Promise<number> {
  const argv = process.argv.slice(2);
  if (argv[0] === "daemon") return runDaemonSubcommand(argv.slice(1));
  return runCli(argv);
}

process.exitCode = await run();
