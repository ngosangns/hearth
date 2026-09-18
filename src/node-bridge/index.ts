// A ~20-line adapter letting a Node/oclif-based task runner shell out to this Bun-only tool without
// depending on Bun itself as a library — mirrors infra's `scripts/src/lib/local-services/run-localctl.ts`
// and `scripts/bin/mcp-local-services.mjs`. Node-only (no `Bun.*` calls), so it's safe to import from
// a plain Node CLI.

import { spawnSync } from "node:child_process";

export type RunBunEntryOptions = {
  /** Path to the consumer's own Bun entry script — e.g. a small `bin.ts` that imports `main` from
   * `@gnasdev/local-services/cli` together with the consumer's own `ServiceCatalog`. This package
   * never assumes where that file lives. */
  entry: string;
  args?: readonly string[];
  cwd?: string;
  bunBinary?: string;
};

/** Runs the consumer's Bun CLI entry, inheriting this process's stdio, appending `--root <cwd>`.
 * Returns the child's exit code (1 if it could not be determined). */
export function runLocalctl(options: RunBunEntryOptions): number {
  const cwd = options.cwd ?? process.cwd();
  const result = spawnSync(options.bunBinary ?? "bun", ["run", options.entry, ...(options.args ?? []), "--root", cwd], { stdio: "inherit", cwd });
  if (result.error) throw result.error;
  return result.status ?? 1;
}

/** Runs the consumer's Bun MCP stdio-server entry — e.g. a small script that calls
 * `createLocalServicesMcpServer` from `@gnasdev/local-services/mcp` and connects it to
 * `StdioServerTransport` — piping this process's stdio straight through. Lets a Node-hosted MCP
 * client register this tool without ever needing Bun on its own PATH. */
export function spawnMcpBridge(options: Pick<RunBunEntryOptions, "entry" | "cwd" | "bunBinary">): number {
  const cwd = options.cwd ?? process.cwd();
  const result = spawnSync(options.bunBinary ?? "bun", ["run", options.entry], { stdio: "inherit", cwd });
  if (result.error) throw result.error;
  return result.status ?? 1;
}
