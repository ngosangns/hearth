// Declarative catalog authoring: a `local-services.yaml` (or `.yml`/`.json`) file sitting in a
// project's root, mapped onto the same `ServiceCatalog` every other entry point takes. This is the
// one place in the package that has an opinion about a file format — `core`'s other modules stay
// catalog-shape-agnostic on purpose, so this module only *produces* a `ServiceCatalog`, it never
// changes what one is.
//
// TypeScript catalogs (`local-services.config.ts`) are not accepted. Anything a catalog needs to
// express has to fit this declarative shape (including `readinessTimeoutMs` and `preparationCommand`).
//
// Kept deliberately separate from `./env` (login-shell / `.env` resolution for the *daemon process
// itself*): `env`/`envFile` here are catalog-authored, per-service values baked into each spawned
// command, layered on top of whatever `./env` resolved as the daemon's own base environment.

import { isAbsolute, join, relative } from "node:path";

import type { CommandSpec, PreparationCommand, ReadinessSpec, ServiceCatalog, ServiceDefinition, ServiceId, ServiceKind, ServiceOwnership, ServicePort, ServiceUrl } from "./catalog";
import { validateCatalog } from "./catalog";
import { isRecord } from "./file-io";
import { loadEnvFile } from "./env";

export const configFileNames = ["local-services.yaml", "local-services.yml", "local-services.json"] as const;
export type ConfigFileName = (typeof configFileNames)[number];

export type ConfigFileLoadResult = { ok: true; catalog: ServiceCatalog; path: string } | { ok: false; path?: string; errors: string[] };

/** First existing candidate in `configFileNames` order, or `undefined` if none exists. */
export async function findConfigFile(root: string): Promise<string | undefined> {
  for (const name of configFileNames) {
    const path = join(root, name);
    if (await Bun.file(path).exists()) return path;
  }
  return undefined;
}

export async function loadCatalog(root: string): Promise<ConfigFileLoadResult> {
  const path = await findConfigFile(root);
  if (!path) {
    const leftoverTs = join(root, "local-services.config.ts");
    const hint = (await Bun.file(leftoverTs).exists()) ? "; local-services.config.ts is no longer accepted — author a local-services.yaml instead" : "";
    return { ok: false, errors: [`no config file found in ${root} (looked for ${configFileNames.join(", ")})${hint}`] };
  }
  return loadCatalogFromFile(path, root);
}

export async function loadCatalogFromFile(path: string, root: string = dirnameOf(path)): Promise<ConfigFileLoadResult> {
  if (path.endsWith(".ts")) {
    return { ok: false, path, errors: [`${path} is a TypeScript catalog; only local-services.yaml, .yml, or .json are accepted`] };
  }
  let raw: unknown;
  try {
    raw = Bun.YAML.parse(await Bun.file(path).text()); // JSON is valid YAML, so this handles .json too.
  } catch (cause) {
    return { ok: false, path, errors: [`failed to parse ${path}: ${cause instanceof Error ? cause.message : String(cause)}`] };
  }
  const mapped = await mapConfigFile(raw, root, path);
  if (!mapped.ok) return { ok: false, path, errors: mapped.errors };
  const validation = validateCatalog(mapped.catalog);
  if (validation.errors.length) return { ok: false, path, errors: validation.errors };
  return { ok: true, catalog: mapped.catalog, path };
}

function dirnameOf(path: string): string {
  const index = path.lastIndexOf("/");
  return index === -1 ? "." : path.slice(0, index);
}

// -------------------------------------------------------------------------------------------
// Declarative shape -> ServiceCatalog
// -------------------------------------------------------------------------------------------

const readinessKinds = ["process", "tcp", "http", "container", "tailnet", "command"] as const;
const serviceKinds: readonly ServiceKind[] = ["application", "infrastructure"];
const ownerships: readonly ServiceOwnership[] = ["daemon", "external"];

const isStringArray = (value: unknown): value is string[] => Array.isArray(value) && value.every((entry) => typeof entry === "string");
const isStringRecord = (value: unknown): value is Record<string, string> => isRecord(value) && Object.values(value).every((entry) => typeof entry === "string");

function readPositiveInteger(value: unknown, path: string, errors: string[]): number | undefined {
  if (value === undefined) return undefined;
  if (!Number.isInteger(value) || (value as number) <= 0) {
    errors.push(`${path} must be a positive integer`);
    return undefined;
  }
  return value as number;
}

/** A raw config-file command (`{argv}` or `{shell}`, same shape as `CommandSpec`) at `path`, or
 * `undefined` with an error pushed to `errors` if present-but-malformed. Absent is valid — callers
 * decide whether that means "no command" or "required". */
function readCommandSpec(value: unknown, path: string, errors: string[]): { spec: CommandSpec; exec?: boolean } | undefined {
  if (!isRecord(value)) {
    errors.push(`${path} must be an object with \`argv\` or \`shell\``);
    return undefined;
  }
  const hasArgv = "argv" in value;
  const hasShell = "shell" in value;
  if (hasArgv === hasShell) {
    errors.push(`${path} must set exactly one of \`argv\` or \`shell\``);
    return undefined;
  }
  if (hasArgv) {
    // A bare numeric/boolean argv element (`argv: [sleep, 30]`) parses as a YAML number/boolean, not
    // a string — extremely easy to author by accident (port numbers, `sleep 30`) and always safe to
    // coerce, since every argv element ends up as a string on `Bun.spawn`'s argv regardless.
    const argv = Array.isArray(value.argv) && value.argv.every((entry) => typeof entry === "string" || typeof entry === "number" || typeof entry === "boolean") ? value.argv.map((entry) => String(entry)) : undefined;
    if (!argv || argv.length === 0) {
      errors.push(`${path}.argv must be a non-empty array of strings (numbers/booleans are coerced to strings)`);
      return undefined;
    }
    return { spec: { argv } };
  }
  if (typeof value.shell !== "string" || !value.shell.trim()) {
    errors.push(`${path}.shell must be a non-empty string`);
    return undefined;
  }
  const exec = value.exec;
  if (exec !== undefined && typeof exec !== "boolean") {
    errors.push(`${path}.exec must be a boolean`);
    return undefined;
  }
  return { spec: { shell: value.shell, exec }, exec };
}

function readReadiness(value: unknown, path: string, errors: string[]): ReadinessSpec | undefined {
  if (!isRecord(value) || typeof value.kind !== "string") {
    errors.push(`${path}.kind is required (one of ${readinessKinds.join(", ")})`);
    return undefined;
  }
  const kind = value.kind;
  if (!(readinessKinds as readonly string[]).includes(kind)) {
    errors.push(`${path}.kind must be one of ${readinessKinds.join(", ")}, got ${JSON.stringify(kind)}`);
    return undefined;
  }
  if (kind === "process" || kind === "container" || kind === "tailnet") return { kind };
  if (kind === "tcp") {
    if (!Number.isInteger(value.port) || (value.port as number) <= 0) {
      errors.push(`${path}.port must be a positive integer`);
      return undefined;
    }
    return { kind: "tcp", port: value.port as number };
  }
  if (kind === "http") {
    if (typeof value.url !== "string" || !value.url) {
      errors.push(`${path}.url must be a non-empty string`);
      return undefined;
    }
    return { kind: "http", url: value.url };
  }
  // kind === "command"
  const command = readCommandSpec(value.command, `${path}.command`, errors);
  if (!command) return undefined;
  if (value.cwd !== undefined && typeof value.cwd !== "string") {
    errors.push(`${path}.cwd must be a string`);
    return undefined;
  }
  return { kind: "command", command: command.spec, cwd: value.cwd as string | undefined };
}

function readPreparationCommand(value: unknown, path: string, errors: string[]): PreparationCommand | undefined {
  if (!isRecord(value)) {
    errors.push(`${path} must be an object with \`command\` (and optional \`cwd\`)`);
    return undefined;
  }
  const command = readCommandSpec(value.command, `${path}.command`, errors);
  if (value.cwd !== undefined && typeof value.cwd !== "string") errors.push(`${path}.cwd must be a string`);
  if (value.serializationKey !== undefined && typeof value.serializationKey !== "string") errors.push(`${path}.serializationKey must be a string`);
  if (!command) return undefined;
  return { command: command.spec, cwd: value.cwd as string | undefined, serializationKey: value.serializationKey as string | undefined };
}

function readPorts(value: unknown, path: string, errors: string[]): ServicePort[] | undefined {
  if (value === undefined) return [];
  if (!Array.isArray(value)) {
    errors.push(`${path} must be an array`);
    return undefined;
  }
  const ports: ServicePort[] = [];
  value.forEach((entry, index) => {
    const entryPath = `${path}[${index}]`;
    if (!isRecord(entry) || !Number.isInteger(entry.port) || typeof entry.label !== "string" || !entry.label) {
      errors.push(`${entryPath} must be { port: number, label: string }`);
      return;
    }
    if (entry.requiresRunning !== undefined && typeof entry.requiresRunning !== "boolean") {
      errors.push(`${entryPath}.requiresRunning must be a boolean`);
      return;
    }
    ports.push({ port: entry.port as number, label: entry.label, requiresRunning: entry.requiresRunning as boolean | undefined });
  });
  return ports;
}

/** `urls` accepts a bare string or `{ url, label?, requiresRunning? }` per entry. Only the shape is
 * checked here; the URL format and its placeholders are checked by `validateCatalog`, which runs for
 * every catalog source, not just config files. */
function readUrls(value: unknown, path: string, errors: string[]): ServiceUrl[] | undefined {
  if (value === undefined) return [];
  if (!Array.isArray(value)) {
    errors.push(`${path} must be an array`);
    return undefined;
  }
  const urls: ServiceUrl[] = [];
  value.forEach((entry, index) => {
    const entryPath = `${path}[${index}]`;
    if (typeof entry === "string") {
      urls.push({ url: entry });
      return;
    }
    if (!isRecord(entry) || typeof entry.url !== "string") {
      errors.push(`${entryPath} must be a URL string or { url: string, label?: string, requiresRunning?: boolean }`);
      return;
    }
    if (entry.label !== undefined && typeof entry.label !== "string") {
      errors.push(`${entryPath}.label must be a string`);
      return;
    }
    if (entry.requiresRunning !== undefined && typeof entry.requiresRunning !== "boolean") {
      errors.push(`${entryPath}.requiresRunning must be a boolean`);
      return;
    }
    urls.push({ url: entry.url, label: entry.label as string | undefined, requiresRunning: entry.requiresRunning as boolean | undefined });
  });
  return urls;
}

/** `cwd` is authored relative to the project root and must stay inside it — a service definition is
 * as trusted as arbitrary code (it names a command to run), but a `cwd` that walks out of the
 * project via `..` or an absolute path has no legitimate use here and is easy to author by mistake
 * (e.g. copy-pasting an absolute path from a shell history). This is a lexical check on the
 * resolved path, not a symlink-escape defense — see `AGENTS.md` for the general trust posture
 * a desktop app wrapping this loader still needs on top. */
function resolveServiceCwd(root: string, cwd: string | undefined, path: string, errors: string[]): string | undefined {
  const relativeCwd = cwd ?? ".";
  if (isAbsolute(relativeCwd)) {
    errors.push(`${path}.cwd must be a relative path, got ${relativeCwd}`);
    return undefined;
  }
  const resolved = join(root, relativeCwd);
  const rel = relative(root, resolved);
  if (rel.startsWith("..")) {
    errors.push(`${path}.cwd escapes the project root: ${relativeCwd}`);
    return undefined;
  }
  return relativeCwd;
}

// A key outside these sets is a typo (e.g. `command:` for `run:`), which would otherwise be silently
// dropped and surface much later as an unrelated error. `x-`-prefixed keys stay free for YAML anchors.
const topLevelKeys = ["version", "env", "envFile", "runtimeDirectory", "privateFileGuard", "groups", "services"];
const serviceKeys = ["label", "kind", "ownership", "env", "container", "cwd", "run", "stop", "build", "readiness", "readinessTimeoutMs", "preparationCommand", "ports", "urls"];
function checkKnownKeys(value: Record<string, unknown>, known: readonly string[], path: string, errors: string[]): void {
  for (const key of Object.keys(value)) {
    if (!known.includes(key) && !key.startsWith("x-")) errors.push(`${path} has unknown key "${key}" (known: ${known.join(", ")})`);
  }
}

async function mapConfigFile(raw: unknown, root: string, path: string): Promise<{ ok: true; catalog: ServiceCatalog } | { ok: false; errors: string[] }> {
  const errors: string[] = [];
  if (!isRecord(raw)) return { ok: false, errors: [`${path} must contain a YAML/JSON object`] };
  checkKnownKeys(raw, topLevelKeys, "config file", errors);
  if (raw.version !== 1) errors.push(`version must be 1, got ${JSON.stringify(raw.version)}`);
  if (raw.env !== undefined && !isStringRecord(raw.env)) errors.push("env must be a map of string to string");
  if (raw.envFile !== undefined && typeof raw.envFile !== "string") errors.push("envFile must be a string");
  if (raw.runtimeDirectory !== undefined && typeof raw.runtimeDirectory !== "string") errors.push("runtimeDirectory must be a string");
  if (raw.privateFileGuard !== undefined && typeof raw.privateFileGuard !== "boolean") errors.push("privateFileGuard must be a boolean");
  const groupsRaw = raw.groups;
  if (groupsRaw !== undefined && (!isRecord(groupsRaw) || !Object.values(groupsRaw).every(isStringArray))) errors.push("groups must be a map of string to string[]");
  if (!isRecord(raw.services) || Object.keys(raw.services).length === 0) errors.push("services must be a non-empty map of service id to service definition");
  if (errors.length) return { ok: false, errors };

  const globalEnv = (raw.env as Record<string, string> | undefined) ?? {};
  const fileEnv = raw.envFile ? await loadEnvFile(isAbsolute(raw.envFile as string) ? (raw.envFile as string) : join(root, raw.envFile as string)) : {};
  const baseEnv = { ...fileEnv, ...globalEnv };

  const services: ServiceDefinition[] = [];
  for (const [id, value] of Object.entries(raw.services as Record<string, unknown>)) {
    const svcPath = `services.${id}`;
    if (!isRecord(value)) {
      errors.push(`${svcPath} must be an object`);
      continue;
    }
    checkKnownKeys(value, serviceKeys, svcPath, errors);
    if (value.kind !== undefined && !serviceKinds.includes(value.kind as ServiceKind)) errors.push(`${svcPath}.kind must be one of ${serviceKinds.join(", ")}`);
    if (value.ownership !== undefined && !ownerships.includes(value.ownership as ServiceOwnership)) errors.push(`${svcPath}.ownership must be one of ${ownerships.join(", ")}`);
    if (value.env !== undefined && !isStringRecord(value.env)) errors.push(`${svcPath}.env must be a map of string to string`);
    if (value.container !== undefined && typeof value.container !== "string") errors.push(`${svcPath}.container must be a string`);

    const cwd = resolveServiceCwd(root, value.cwd as string | undefined, svcPath, errors);
    const readiness = readReadiness(value.readiness, `${svcPath}.readiness`, errors);
    const ports = readPorts(value.ports, `${svcPath}.ports`, errors);
    const urls = readUrls(value.urls, `${svcPath}.urls`, errors);
    const readinessTimeoutMs = readPositiveInteger(value.readinessTimeoutMs, `${svcPath}.readinessTimeoutMs`, errors);

    const preparationCommand = value.preparationCommand === undefined ? undefined : readPreparationCommand(value.preparationCommand, `${svcPath}.preparationCommand`, errors);

    let profileRun: ServiceDefinition["profiles"]["run"] | undefined;
    if (value.run === undefined) {
      if (readiness) profileRun = { commandStatus: "unresolved", readiness, readinessTimeoutMs, preparationCommand };
    } else {
      const run = readCommandSpec(value.run, `${svcPath}.run`, errors);
      const stop = value.stop === undefined ? undefined : readCommandSpec(value.stop, `${svcPath}.stop`, errors);
      if (run && readiness && cwd !== undefined) {
        const environment = { ...baseEnv, ...(value.env as Record<string, string> | undefined) };
        profileRun = {
          commandStatus: "verified",
          readiness,
          readinessTimeoutMs,
          preparationCommand,
          command: { command: run.spec, cwd, environment: Object.keys(environment).length ? environment : undefined, containerName: value.container as string | undefined, dockerStopCommand: stop?.spec },
        };
      }
    }

    let profileBuild: ServiceDefinition["profiles"]["build"] | undefined;
    if (value.build !== undefined) {
      if (!isRecord(value.build)) errors.push(`${svcPath}.build must be an object`);
      else {
        const build = readCommandSpec(value.build, `${svcPath}.build`, errors);
        if (value.build.timeoutMs !== undefined && (!Number.isInteger(value.build.timeoutMs) || (value.build.timeoutMs as number) <= 0)) errors.push(`${svcPath}.build.timeoutMs must be a positive integer`);
        if (value.build.serializationKey !== undefined && typeof value.build.serializationKey !== "string") errors.push(`${svcPath}.build.serializationKey must be a string`);
        if (build && cwd !== undefined) profileBuild = { command: { command: build.spec, cwd }, timeoutMs: value.build.timeoutMs as number | undefined, serializationKey: value.build.serializationKey as string | undefined };
      }
    }

    if (!profileRun) continue; // Already recorded a more specific error above (bad run/readiness/cwd).
    services.push({
      id,
      label: typeof value.label === "string" ? value.label : undefined,
      kind: value.kind as ServiceKind | undefined,
      ownership: value.ownership as ServiceOwnership | undefined,
      profiles: { run: profileRun, build: profileBuild },
      ports: ports?.length ? ports : undefined,
      urls: urls?.length ? urls : undefined,
    });
  }
  if (errors.length) return { ok: false, errors };

  const catalog: ServiceCatalog = {
    startFailurePolicy: "stop-on-first-failure-keep-started",
    services,
    groups: (raw.groups as Record<string, ServiceId[]> | undefined) ?? {},
    runtimeDirectory: raw.runtimeDirectory as string | undefined,
    privateFileGuard: raw.privateFileGuard as boolean | undefined,
  };
  return { ok: true, catalog };
}
