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
  | { readonly kind: "custom"; readonly name: string; readonly probe: (ctx: ReadinessProbeContext) => Promise<"ready" | "not-ready" | "failed"> };

export type ServiceOwnership = "daemon" | "external";
export type ServiceKind = "application" | "infrastructure";

export type VerifiedServiceRunProfile = {
  readonly command: ServiceCommand;
  readonly commandStatus: "verified";
  readonly readiness: ReadinessSpec;
  readonly readinessTimeoutMs?: number;
  readonly preparation?: readonly string[];
};
export type UnresolvedServiceRunProfile = {
  readonly command?: undefined;
  readonly commandStatus: "unresolved";
  readonly readiness: ReadinessSpec;
  readonly readinessTimeoutMs?: number;
  readonly preparation?: readonly string[];
};
export type ServiceRunProfile = VerifiedServiceRunProfile | UnresolvedServiceRunProfile;

/** Opt-in compile/build step run once before the run command starts (generalizes viclass's Gradle/Nest
 * build phase). Services that share a `serializationKey` run their builds one at a time — set this when
 * the underlying build tool (e.g. a shared Gradle daemon) cannot run concurrent builds safely. */
export type ServiceBuildProfile = { readonly command: ServiceCommand; readonly timeoutMs?: number; readonly serializationKey?: string };

export type ServicePort = { readonly port: number; readonly label: string; readonly requiresRunning?: boolean };

export type ServiceDefinition = {
  readonly id: ServiceId;
  readonly label?: string;
  readonly kind?: ServiceKind;
  /** 'daemon' (default): this manager owns start/stop. 'external': this manager never spawns or stops
   * the service itself but periodically probes its readiness and adopts/releases it into its own state
   * machine when detected — generalizes infra's `syncExternalUnits` carve-out for docker/tailnet units
   * that can also be brought up outside the daemon (e.g. `task local:up`). */
  readonly ownership?: ServiceOwnership;
  readonly dependencies?: readonly ServiceId[];
  readonly profiles: { readonly run: ServiceRunProfile; readonly build?: ServiceBuildProfile };
  /** Additional ports this service exposes beyond its readiness port (generalizes infra's `tailnetPorts`). */
  readonly ports?: readonly ServicePort[];
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
  for (const service of catalog.services) {
    for (const dependency of service.dependencies ?? []) {
      if (!services.has(dependency)) errors.push(`${service.id} depends on unknown service ${dependency}`);
    }
  }
  const visiting = new Set<ServiceId>();
  const visited = new Set<ServiceId>();
  const visit = (serviceId: ServiceId, path: ServiceId[]): void => {
    if (visiting.has(serviceId)) {
      const start = path.indexOf(serviceId);
      errors.push(`dependency cycle: ${[...path.slice(start), serviceId].join(" -> ")}`);
      return;
    }
    if (visited.has(serviceId)) return;
    visiting.add(serviceId);
    const service = services.get(serviceId);
    for (const dependency of service?.dependencies ?? []) if (services.has(dependency)) visit(dependency, [...path, serviceId]);
    visiting.delete(serviceId);
    visited.add(serviceId);
  };
  for (const service of catalog.services) visit(service.id, []);

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
    if (profile.readiness.kind === "tcp") {
      const existing = verifiedPorts.get(profile.readiness.port);
      if (existing && existing !== service.id) errors.push(`port ${profile.readiness.port} is shared by ${existing} and ${service.id}`);
      verifiedPorts.set(profile.readiness.port, service.id);
    }
  }
  return { errors, warnings };
}

export function dependencyLevels(catalog: ServiceCatalog, targets: readonly ServiceId[]): ServiceId[][] {
  const validation = validateCatalog(catalog);
  if (validation.errors.length) throw new Error(`Invalid service catalog: ${validation.errors.join("; ")}`);
  const services = new Map(catalog.services.map((service) => [service.id, service]));
  const selected = new Set<ServiceId>();
  const include = (serviceId: ServiceId): void => {
    if (selected.has(serviceId)) return;
    selected.add(serviceId);
    for (const dependency of services.get(serviceId)!.dependencies ?? []) include(dependency);
  };
  for (const target of targets) include(target);
  const remaining = new Map<ServiceId, number>();
  for (const serviceId of selected) remaining.set(serviceId, (services.get(serviceId)!.dependencies ?? []).filter((dependency) => selected.has(dependency)).length);
  const levels: ServiceId[][] = [];
  while (remaining.size) {
    const ready = catalog.services.map((service) => service.id).filter((serviceId) => remaining.get(serviceId) === 0);
    if (!ready.length) throw new Error("Invalid service catalog: dependency cycle");
    levels.push(ready);
    for (const serviceId of ready) remaining.delete(serviceId);
    for (const serviceId of remaining.keys()) {
      const dependencies = services.get(serviceId)!.dependencies ?? [];
      remaining.set(serviceId, dependencies.filter((dependency) => remaining.has(dependency)).length);
    }
  }
  return levels;
}

export function isContainerCommand(command: ServiceCommand): boolean {
  return command.containerName !== undefined;
}
