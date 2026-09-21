// Public config/catalog API. A consumer authors one `ServiceCatalog` value describing its own
// services and passes it to every other entry point (`LocalServicesManager.bootstrap`, `runDoctor`,
// `runTui`, `createLocalServicesMcpServer`) — nothing in this package imports a catalog directly.

export type ServiceId = string;

/** How to invoke a command. `argv` is injection-safe (spawned directly, no shell); `shell` supports
 * the chaining (`cd -- '...' && exec ...`) some build tools need — set `exec: true` when the shell
 * command itself execs into the long-running process, so the shell doesn't linger as a wrapper. */
export type CommandSpec = { readonly argv: readonly string[] } | { readonly shell: string; readonly exec?: boolean };

export type ServiceCommand = {
  readonly command: CommandSpec;
  readonly cwd: string;
  readonly environment?: Readonly<Record<string, string>>;
  readonly containerName?: string;
  readonly dockerStopCommand?: CommandSpec;
};

export type ReadinessProbeContext = { readonly serviceId: ServiceId };
export type ReadinessSpec =
  | { readonly kind: "process" }
  | { readonly kind: "tcp"; readonly port: number }
  | { readonly kind: "http"; readonly url: string }
  | { readonly kind: "container" }
  | { readonly kind: "tailnet" }
  /** A declarative, JSON-serializable stand-in for `custom` (exit code 0 = ready, anything else =
   * not-ready-yet — never "failed", so it retries the same way tcp/http do until the readiness
   * timeout). Exists so a config-file-authored catalog (no closures) can still express an arbitrary
   * check, and so that catalog can travel over `POST /v1/manager/reload` as plain JSON. `cwd` is
   * relative to the manager's root; omitted defaults to the root itself. */
  | { readonly kind: "command"; readonly command: CommandSpec; readonly cwd?: string }
  | { readonly kind: "custom"; readonly name: string; readonly probe: (ctx: ReadinessProbeContext) => Promise<"ready" | "not-ready" | "failed"> };

export type ServiceOwnership = "daemon" | "external";
export type ServiceKind = "application" | "infrastructure";

/** A declarative, JSON-serializable stand-in for a bespoke `PreparationAdapter` (exit code 0 =
 * prepared, anything else = failed) — exists for exactly the same reason `{ kind: "command" }`
 * readiness exists: a config-file-authored catalog has no closures, and this needs to travel over
 * `POST /v1/manager/reload` or a YAML file as plain JSON. Runs via `ProbeAdapter.command` (the same
 * adapter `{ kind: "command" }` readiness already uses), independently of the opaque `preparation`
 * marker list above — a service may use either, both, or neither. `cwd` is relative to the manager's
 * root; omitted defaults to the root itself. */
export type PreparationCommand = {
  readonly command: CommandSpec;
  readonly cwd?: string;
  /** Services that share a `serializationKey` run their preparation command one at a time — same
   * idea, same mechanism, as `ServiceBuildProfile.serializationKey`. Set this when preparation
   * touches shared, non-concurrency-safe state (e.g. a check-then-generate shared cert/config file
   * with no locking of its own). */
  readonly serializationKey?: string;
};

export type VerifiedServiceRunProfile = {
  readonly command: ServiceCommand;
  readonly commandStatus: "verified";
  readonly readiness: ReadinessSpec;
  readonly readinessTimeoutMs?: number;
  readonly preparation?: readonly string[];
  readonly preparationCommand?: PreparationCommand;
};
export type UnresolvedServiceRunProfile = {
  readonly command?: undefined;
  readonly commandStatus: "unresolved";
  readonly readiness: ReadinessSpec;
  readonly readinessTimeoutMs?: number;
  readonly preparation?: readonly string[];
  readonly preparationCommand?: PreparationCommand;
};
export type ServiceRunProfile = VerifiedServiceRunProfile | UnresolvedServiceRunProfile;

/** Opt-in compile/build step run once before the run command starts (generalizes viclass's Gradle/Nest
 * build phase). Services that share a `serializationKey` run their builds one at a time — set this when
 * the underlying build tool (e.g. a shared Gradle daemon) cannot run concurrent builds safely. */
export type ServiceBuildProfile = { readonly command: ServiceCommand; readonly timeoutMs?: number; readonly serializationKey?: string };

export type ServicePort = { readonly port: number; readonly label: string; readonly requiresRunning?: boolean };

/** A URL where a service can be reached, surfaced by every client (CLI `urls`, TUI, MCP `status`,
 * the macOS app) so nobody has to remember which port or nginx path a service lives behind.
 *
 * `url` may contain placeholders from `SERVICE_URL_PLACEHOLDERS`, resolved by the daemon at request
 * time — `{tailnetHost}` becomes this machine's Tailscale DNS name, so a catalog shared across
 * machines never hardcodes one machine's hostname. `requiresRunning: false` marks a URL that works
 * even while this service is stopped (e.g. a storefront another service also serves); clients may
 * dim the others when the service is not running. */
export type ServiceUrl = { readonly url: string; readonly label?: string; readonly requiresRunning?: boolean };

/** Placeholders a `ServiceUrl.url` may contain. Anything else in braces is a validation error, so a
 * typo'd placeholder fails the catalog load instead of rendering a dead link. */
export const SERVICE_URL_PLACEHOLDERS = ["tailnetHost"] as const;
export type ServiceUrlPlaceholder = (typeof SERVICE_URL_PLACEHOLDERS)[number];

/** Names of every `{placeholder}` in a URL template, in order of appearance. */
export function serviceUrlPlaceholders(url: string): string[] {
  return [...url.matchAll(/\{([^{}]*)\}/g)].map((match) => match[1] ?? "");
}

/** A service URL with its placeholders substituted, as served by `GET /v1/urls`. `requiresRunning`
 * is `false` only when the catalog said the URL works while the service is stopped. */
export type ResolvedServiceUrl = { readonly serviceId: ServiceId; readonly label?: string; readonly url: string; readonly requiresRunning: boolean };
/** A URL that could not be resolved because a placeholder had no value on this machine (e.g.
 * `{tailnetHost}` with Tailscale not running) — reported rather than dropped silently. */
export type UnresolvedServiceUrl = { readonly serviceId: ServiceId; readonly url: string; readonly placeholder: string };

/** Substitutes every placeholder in every service URL, in catalog order. `lookup` answers one
 * placeholder name; it is injected so this stays testable without a real Tailscale. */
export function resolveServiceUrls(catalog: ServiceCatalog, lookup: (placeholder: string) => string | undefined): { urls: ResolvedServiceUrl[]; unresolved: UnresolvedServiceUrl[] } {
  const urls: ResolvedServiceUrl[] = [];
  const unresolved: UnresolvedServiceUrl[] = [];
  for (const service of catalog.services) {
    entries: for (const entry of service.urls ?? []) {
      let url = entry.url;
      for (const name of serviceUrlPlaceholders(entry.url)) {
        const value = lookup(name);
        if (value === undefined) {
          unresolved.push({ serviceId: service.id, url: entry.url, placeholder: name });
          continue entries;
        }
        url = url.replaceAll(`{${name}}`, value);
      }
      urls.push({ serviceId: service.id, ...(entry.label !== undefined ? { label: entry.label } : {}), url, requiresRunning: entry.requiresRunning !== false });
    }
  }
  return { urls, unresolved };
}

export type ServiceDefinition = {
  readonly id: ServiceId;
  readonly label?: string;
  readonly kind?: ServiceKind;
  /** 'daemon' (default): this manager owns start/stop. 'external': this manager never spawns or stops
   * the service itself but periodically probes its readiness and adopts/releases it into its own state
   * machine when detected — generalizes infra's `syncExternalUnits` carve-out for docker/tailnet units
   * that can also be brought up outside the daemon (e.g. `task local:up`). */
  readonly ownership?: ServiceOwnership;
  readonly profiles: { readonly run: ServiceRunProfile; readonly build?: ServiceBuildProfile };
  /** Additional ports this service exposes beyond its readiness port (generalizes infra's `tailnetPorts`). */
  readonly ports?: readonly ServicePort[];
  /** Where this service can be reached — see `ServiceUrl`. */
  readonly urls?: readonly ServiceUrl[];
};

export type ServiceCatalog = {
  readonly services: readonly ServiceDefinition[];
  readonly groups: Readonly<Record<string, readonly ServiceId[]>>;
  readonly composeFile?: string;
  /** Runtime state directory, relative to the manager's root. Default: `.local-services/runtime-v1`. */
  readonly runtimeDirectory?: string;
  readonly startFailurePolicy: "stop-on-first-failure-keep-started";
  /** viclass's O_NOFOLLOW + dev/ino private-file guard layer for every lock/state/log file operation.
   * Default on; a single-user dev machine that doesn't need symlink-attack defense (infra's case) may
   * opt out. */
  readonly privateFileGuard?: boolean;
};

export type CatalogValidation = { errors: string[]; warnings: string[] };

export function validateCatalog(catalog: ServiceCatalog): CatalogValidation {
  const errors: string[] = [];
  const warnings: string[] = [];
  const services = new Map<ServiceId, ServiceDefinition>();
  for (const service of catalog.services) {
    if (services.has(service.id)) errors.push(`duplicate service ${service.id}`);
    services.set(service.id, service);
  }
  for (const [groupName, members] of Object.entries(catalog.groups)) {
    for (const member of members) {
      if (!services.has(member)) errors.push(`group ${groupName} references unknown service ${member}`);
    }
  }
  const verifiedPorts = new Map<number, ServiceId>();
  for (const service of catalog.services) {
    const profile = service.profiles.run;
    if (!profile.readiness) errors.push(`${service.id}:run is missing a readiness policy`);
    if (profile.commandStatus === "verified") {
      if (!profile.command) errors.push(`${service.id}:run is missing a verified command`);
    } else {
      warnings.push(`${service.id}:run command is unresolved`);
    }
    if (service.profiles.build?.timeoutMs !== undefined && (!Number.isInteger(service.profiles.build.timeoutMs) || service.profiles.build.timeoutMs <= 0)) {
      errors.push(`${service.id}:build has an invalid timeout`);
    }
    for (const [index, entry] of (service.urls ?? []).entries()) {
      const where = `${service.id}:urls[${index}]`;
      if (typeof entry.url !== "string" || !/^https?:\/\/\S+$/.test(entry.url)) {
        errors.push(`${where} must be an http:// or https:// URL`);
        continue;
      }
      for (const name of serviceUrlPlaceholders(entry.url)) {
        if (!(SERVICE_URL_PLACEHOLDERS as readonly string[]).includes(name)) errors.push(`${where} has unknown placeholder {${name}} (known: ${SERVICE_URL_PLACEHOLDERS.map((known) => `{${known}}`).join(", ")})`);
      }
      if (entry.label !== undefined && (typeof entry.label !== "string" || !entry.label.trim())) errors.push(`${where}.label must be a non-empty string`);
    }
    if (profile.readiness.kind === "tcp") {
      const existing = verifiedPorts.get(profile.readiness.port);
      if (existing && existing !== service.id) errors.push(`port ${profile.readiness.port} is shared by ${existing} and ${service.id}`);
      verifiedPorts.set(profile.readiness.port, service.id);
    }
  }
  return { errors, warnings };
}

export function isContainerCommand(command: ServiceCommand): boolean {
  return command.containerName !== undefined;
}
