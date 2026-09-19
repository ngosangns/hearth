// Resolves the environment a daemon should hand to every process it spawns. Exists because a daemon
// started from a GUI (Finder/Dock/LaunchAgent, as a desktop app's sidecar would) inherits a bare
// `PATH` — none of the login-shell customization (`nvm`, `asdf`, Homebrew, a project's own `.env`)
// that a daemon started from an interactive terminal gets for free via `process.env`. A daemon
// spawned from a terminal and one spawned from an app should end up running the exact same commands
// with the exact same environment; this module is the one place that difference gets resolved away.
//
// Deliberately separate from `ServiceCatalog`'s own per-service `environment` and the config file's
// catalog-wide `env` (see `config-file.ts`): those are catalog-authored overrides layered on top of
// whatever this module resolves as the *base* environment for the whole daemon.

const loginShellEnvCache = new Map<string, Promise<Record<string, string>>>();
const defaultLoginShellTimeoutMs = 5_000;
const startMarker = "__lsd_env_start__";
const endMarker = "__lsd_env_end__";

function parseEnvBlock(text: string): Record<string, string> {
  const env: Record<string, string> = {};
  let key: string | undefined;
  for (const rawLine of text.split("\n")) {
    const match = /^([A-Za-z_][A-Za-z0-9_]*)=(.*)$/.exec(rawLine);
    if (match) {
      key = match[1]!;
      env[key] = match[2]!;
    } else if (key !== undefined) {
      // A value containing a literal newline continues on the following line(s).
      env[key] += `\n${rawLine}`;
    }
  }
  return env;
}

/** Runs the user's login shell (`$SHELL` by default) as it would run interactively, then captures
 * the environment it ends up with — the same trick VS Code and similar tools use to see `nvm`/`asdf`/
 * Homebrew shims that only get wired up by `.zshrc`/`.bash_profile`/etc, not by `launchd`. Markers
 * bracket the `env` dump so shell startup noise (a stray `echo` in someone's rc file) on either side
 * is ignored rather than corrupting the parse. Failures (unknown shell, timeout, non-interactive
 * sandboxed shell) resolve to `{}` — never throw — so a caller can always fall back to `process.env`. */
async function runLoginShellEnv(shell: string, timeoutMs: number): Promise<Record<string, string>> {
  try {
    const child = Bun.spawn([shell, "-ilc", `printf '%s' '${startMarker}'; env; printf '%s' '${endMarker}'`], { stdout: "pipe", stderr: "ignore", stdin: "ignore" });
    const timer = setTimeout(() => child.kill(), timeoutMs);
    let stdout: string;
    try {
      stdout = await new Response(child.stdout).text();
      await child.exited;
    } finally {
      clearTimeout(timer);
    }
    const start = stdout.indexOf(startMarker);
    const end = stdout.indexOf(endMarker);
    if (start === -1 || end === -1 || end < start) return {};
    return parseEnvBlock(stdout.slice(start + startMarker.length, end));
  } catch {
    return {};
  }
}

/** Cached per shell path — spawning an interactive login shell is expensive (can run a user's full
 * `.zshrc`), and a daemon only needs this resolved once at startup. */
export function resolveLoginShellEnv(shell: string = process.env.SHELL ?? "/bin/zsh", timeoutMs = defaultLoginShellTimeoutMs): Promise<Record<string, string>> {
  let cached = loginShellEnvCache.get(shell);
  if (!cached) {
    cached = runLoginShellEnv(shell, timeoutMs);
    loginShellEnvCache.set(shell, cached);
  }
  return cached;
}

/** Test-only escape hatch: `resolveLoginShellEnv` memoizes per shell path, which would otherwise
 * leak a stubbed `Bun.spawn` result across unrelated tests. */
export function clearLoginShellEnvCacheForTests(): void {
  loginShellEnvCache.clear();
}

/** Minimal `.env` parser: `KEY=VALUE` per line, optional `export ` prefix, `#`-comments, blank lines
 * skipped, optional surrounding quotes stripped. No interpolation, no multi-line values — deliberately
 * a subset of what `dotenv` supports, sized to what a service `.env.local` actually needs here. */
export async function loadEnvFile(path: string): Promise<Record<string, string>> {
  const file = Bun.file(path);
  if (!(await file.exists())) return {};
  const env: Record<string, string> = {};
  for (const rawLine of (await file.text()).split("\n")) {
    const line = rawLine.trim();
    if (!line || line.startsWith("#")) continue;
    const match = /^(?:export\s+)?([A-Za-z_][A-Za-z0-9_]*)=(.*)$/.exec(line);
    if (!match) continue;
    const [, key, rawValue] = match as unknown as [string, string, string];
    const value = rawValue.trim();
    env[key] = (value.startsWith('"') && value.endsWith('"')) || (value.startsWith("'") && value.endsWith("'")) ? value.slice(1, -1) : value;
  }
  return env;
}

export type BaseEnvironmentOptions = {
  /** Login shell to resolve; defaults to `$SHELL`. Pass `false` to skip shell resolution entirely
   * (e.g. in tests, or when a caller already has a trustworthy environment). */
  shell?: string | false;
  shellTimeoutMs?: number;
  /** `.env`-style file, resolved relative to `root`. */
  envFile?: string;
  root?: string;
  /** Highest-priority overrides, applied last. */
  extra?: Record<string, string>;
};

/** Resolves the base environment a daemon should use for every process it spawns: `process.env` as
 * the floor, overlaid with the login shell's environment (unless disabled), then an optional
 * `.env` file, then explicit overrides — each layer only adding or replacing keys, never removing
 * ones the layer below already set. */
export async function resolveBaseEnvironment(options: BaseEnvironmentOptions = {}): Promise<Record<string, string>> {
  const base: Record<string, string> = { ...(process.env as Record<string, string>) };
  if (options.shell !== false) {
    Object.assign(base, await resolveLoginShellEnv(options.shell, options.shellTimeoutMs));
  }
  if (options.envFile) {
    const { join, isAbsolute } = await import("node:path");
    const path = isAbsolute(options.envFile) ? options.envFile : join(options.root ?? process.cwd(), options.envFile);
    Object.assign(base, await loadEnvFile(path));
  }
  if (options.extra) Object.assign(base, options.extra);
  return base;
}
