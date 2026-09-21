import { createHash } from "node:crypto";
import { closeSync, fstatSync, ftruncateSync, mkdirSync, openSync, readSync, statSync } from "node:fs";
import { connect } from "node:net";
import { join } from "node:path";

import { isContainerCommand, type CommandSpec, type ReadinessSpec, type ServiceCatalog, type ServiceCommand, type ServiceDefinition, type ServiceId, type ServiceRunProfile, type VerifiedServiceRunProfile } from "./catalog";
import { logsDir, rawLogPath, resolveRuntimeDirectory } from "./paths";
import type { ActualServiceState, DockerContainerIdentity, PosixProcessIdentity, ProcessIdentity, ReadinessKind, ServiceLifecycleState, ServiceReadiness } from "./state";

export type ProcessSignal = "SIGTERM" | "SIGKILL";
export type PosixProcessRecord = Pick<PosixProcessIdentity, "pid" | "pgid" | "startIdentity" | "commandFingerprint">;
export type DockerContainerRecord = Pick<DockerContainerIdentity, "containerName" | "containerId" | "containerStartedAt" | "commandFingerprint">;
export type ObservedProcess = (PosixProcessRecord | DockerContainerRecord) & { alive: boolean };
export type ManagedProcess = (PosixProcessRecord | DockerContainerRecord) & { exited: Promise<number> };
export type SpawnInput = { command: ServiceCommand; commandFingerprint: string; serviceId: ServiceId; onOutput?: (data: string) => void };
export interface ProcessAdapter {
  spawn(input: SpawnInput): Promise<ManagedProcess>;
  inspect(identity: ProcessIdentity): Promise<ObservedProcess | undefined>;
  signalGroup(pgid: number, signal: ProcessSignal): Promise<void>;
  stopContainer?(command: ServiceCommand, onOutput?: (data: string) => void): Promise<void>;
  attachOutput?(serviceId: ServiceId, onOutput: (data: string) => void): () => void;
}
export interface PreparationAdapter {
  prepare(serviceId: ServiceId, steps: readonly string[]): Promise<void>;
}
export interface ProbeAdapter {
  tcp(port: number): Promise<boolean>;
  http(url: string): Promise<boolean>;
  container(containerName: string): Promise<boolean>;
  tailnet(): Promise<boolean>;
  portInUse?(port: number): Promise<boolean>;
  /** Backs `{ kind: "command" }` readiness. Optional so every existing `SupervisorOptions` fixture
   * (none of which exercise command readiness) keeps compiling; a catalog that declares one without
   * this adapter present simply never becomes ready (probe returns false, times out normally). */
  command?(command: CommandSpec, cwd?: string): Promise<boolean>;
}
export interface SupervisorClock {
  now(): string;
  sleep(milliseconds: number): Promise<void>;
}
export type SupervisorOptions = {
  process: ProcessAdapter;
  runBuild: (command: ServiceCommand, onOutput: (data: string) => void, signal: AbortSignal) => Promise<void>;
  probes: ProbeAdapter;
  preparation?: PreparationAdapter;
  clock?: SupervisorClock;
  readinessTimeoutMs?: number;
  readinessBackoffMs?: number;
  terminationGraceMs?: number;
  isClosing?: () => boolean;
};

type Host = {
  readonly instanceId: string;
  readonly catalog: ServiceCatalog;
  serviceStates(): ServiceLifecycleState[];
  setServiceState(next: ServiceLifecycleState): Promise<void>;
  appendLog(serviceId: ServiceId, data: string): Promise<void>;
  publish(type: string, data: Record<string, unknown>): void;
  /** Optional so a caller supplying its own host is never forced to grow this hook; when present it
   * receives errors from fire-and-forget work (log forwarding, external-state polling) that must not
   * take the daemon down. Without it those errors are dropped, which is strictly better than the
   * Bun default: an unhandled rejection terminates the process, orphaning every managed service. */
  recordBackgroundError?: (scope: string, error: unknown) => void;
};
type Active = { profile: VerifiedServiceRunProfile; app: ManagedProcess; identity: ProcessIdentity; token: number; stopped: boolean };
type Changes = Partial<ServiceLifecycleState> & { clear?: Array<"identity" | "error" | "exitCode" | "exitedAt" | "currentOperationId"> };
type ReadinessOutcome = { kind: "ready" } | { kind: "superseded" } | { kind: "exited"; message: string } | { kind: "timeout"; message: string; detail: string };
type ProcessTreeEntry = { pid: number; pgid: number; startIdentity: string };

const clock: SupervisorClock = { now: () => new Date().toISOString(), sleep: (milliseconds) => Bun.sleep(milliseconds) };
export const activeStates: readonly ActualServiceState[] = ["queued-start", "preparing", "starting", "running", "running-unready", "ready", "stopping"];
const isDockerIdentity = (identity: ProcessIdentity): identity is DockerContainerIdentity => "containerId" in identity;
const isDockerIdentityRecord = (record: ManagedProcess): record is DockerContainerRecord & { exited: Promise<number> } => "containerId" in record;
const isDockerObserved = (record: ObservedProcess): record is DockerContainerRecord & { alive: boolean } => "containerId" in record;
/** A "task" command has no long-lived managed process to track — it runs once to bring external
 * state (e.g. a tailnet serve config) into the desired shape. Generalizes infra's tailnet-task runner. */
const isTaskCommand = (readiness: ReadinessSpec): boolean => readiness.kind === "tailnet";
const readinessName = (readiness: ReadinessSpec): ReadinessKind => readiness.kind;

const commandArgv = (spec: CommandSpec): { argv: string[]; exec: boolean } => ("argv" in spec ? { argv: [...spec.argv], exec: false } : { argv: ["sh", "-c", spec.shell], exec: spec.exec ?? false });
// The fingerprint identifies the *logical* command (argv joined, or the bare shell text) — never the
// "sh -c" spawn wrapper itself — so it matches `normalizeObservedCommandFingerprint`, which strips
// that same wrapper back off a `ps`-observed command line.
export const normalizeCommandFingerprint = (command: ServiceCommand): string => {
  const text = "argv" in command.command ? command.command.argv.join(" ") : command.command.shell;
  return createHash("sha256").update(text.trim().replace(/\s+/g, " ")).digest("hex");
};
export const normalizeObservedCommandFingerprint = (command: string): string => createHash("sha256").update(command.replace(/^(?:\/bin\/)?sh\s+-(?:l)?c\s+/, "").trim().replace(/\s+/g, " ")).digest("hex");

const profileFor = (catalog: ServiceCatalog, serviceId: ServiceId): VerifiedServiceRunProfile => {
  const profile = catalog.services.find((service) => service.id === serviceId)?.profiles.run;
  if (!profile || profile.commandStatus !== "verified") throw new Error(`Unsupported service ${serviceId}`);
  return profile;
};
const definitionFor = (catalog: ServiceCatalog, serviceId: ServiceId): ServiceDefinition | undefined => catalog.services.find((service) => service.id === serviceId);

export class ProcessSupervisor {
  private readonly active = new Map<ServiceId, Active>();
  private readonly tokens = new Map<ServiceId, number>();
  private readonly queues = new Map<ServiceId, Promise<void>>();
  private readonly buildAborts = new Map<ServiceId, AbortController>();
  private readonly buildSerials = new Map<string, Promise<void>>();
  private readonly preparationSerials = new Map<string, Promise<void>>();
  private readonly outputTails = new Map<ServiceId, () => void>();
  private composeStartTail = Promise.resolve();
  private readonly optionsClock: SupervisorClock;
  private readonly timeout: number;
  private readonly backoff: number;
  private readonly grace: number;

  constructor(
    private readonly host: Host,
    private readonly options: SupervisorOptions,
  ) {
    this.optionsClock = options.clock ?? clock;
    this.timeout = options.readinessTimeoutMs ?? 10_000;
    this.backoff = options.readinessBackoffMs ?? 100;
    this.grace = options.terminationGraceMs ?? 5_000;
  }

  async start(serviceId: ServiceId, operationId?: string): Promise<void> {
    return this.serial(serviceId, () => this.startLocked(serviceId, operationId));
  }
  async restart(serviceId: ServiceId, operationId?: string): Promise<void> {
    this.cancel(serviceId);
    this.abortBuild(serviceId);
    return this.serial(serviceId, async () => {
      await this.stopLocked(serviceId, operationId);
      await this.startLocked(serviceId, operationId);
    });
  }
  async stop(serviceId: ServiceId, operationId?: string): Promise<void> {
    this.cancel(serviceId);
    this.abortBuild(serviceId);
    return this.serial(serviceId, () => this.stopLocked(serviceId, operationId));
  }
  async startGroup(group: string, operationId?: string): Promise<void> {
    const members = this.host.catalog.groups[group];
    if (!members) throw new Error(`Unknown service group ${group}`);
    for (const serviceId of members) await this.start(serviceId, operationId);
  }
  beginShutdown(): void {
    for (const serviceId of new Set([...this.tokens.keys(), ...this.buildAborts.keys(), ...this.host.serviceStates().map((state) => state.serviceId)])) {
      this.cancel(serviceId);
      this.abortBuild(serviceId);
    }
  }
  async shutdown(): Promise<void> {
    this.beginShutdown();
    const daemonOwned = new Set(this.host.catalog.services.filter((service) => (service.ownership ?? "daemon") === "daemon").map((service) => service.id));
    await Promise.all(this.host.serviceStates().filter((state) => daemonOwned.has(state.serviceId) && activeStates.includes(state.actualState)).map((state) => this.stop(state.serviceId)));
    // A service can carry a verified POSIX identity from a prior generation while its current
    // actualState says 'externally-owned' (e.g. a port conflict was detected at last start) — the
    // first pass above skips it since that state isn't "active", but a lingering owned process must
    // still be reaped on shutdown so it never leaks past this manager's lifetime.
    await Promise.all(this.host.serviceStates().filter((state) => daemonOwned.has(state.serviceId)).map((state) => this.serial(state.serviceId, () => this.reapPersistedPosixIdentity(state.serviceId))));
  }
  private async reapPersistedPosixIdentity(serviceId: ServiceId): Promise<void> {
    const state = this.state(serviceId);
    if (!state?.identity || isDockerIdentity(state.identity) || !this.identityMatchesState(state)) return;
    await this.terminatePersistedPosixIdentity(state.identity);
  }
  private async terminatePersistedPosixIdentity(identity: Exclude<ProcessIdentity, DockerContainerIdentity>): Promise<void> {
    if (!(await this.observedMatches(identity))) return;
    const tree = await this.processTree(identity.pid, identity.startIdentity);
    await this.signalProcessTree(tree, identity.pgid, "SIGTERM");
    const deadline = Date.parse(this.optionsClock.now()) + this.grace;
    while (Date.parse(this.optionsClock.now()) < deadline) {
      if (!(await this.processTreeAlive(tree))) return;
      await this.optionsClock.sleep(this.backoff);
    }
    if (await this.processTreeAlive(tree)) await this.signalProcessTree(tree, identity.pgid, "SIGKILL");
  }

  async status(serviceId: ServiceId): Promise<void> {
    return this.serial(serviceId, async () => {
      const state = this.state(serviceId);
      if (!state?.identity || !activeStates.includes(state.actualState)) return;
      if (!(await this.owns(state))) return this.orphan(state);
      const profile = profileFor(this.host.catalog, serviceId);
      if (profile.readiness.kind !== "process" && !(await this.probe(profile.readiness, serviceId))) {
        await this.transition(serviceId, state.generation, "running-unready", "not-ready", { readinessKind: readinessName(profile.readiness), readinessDetail: "Readiness probe is currently unavailable" });
      }
    });
  }

  async reconcile(): Promise<void> {
    for (const state of this.host.serviceStates()) {
      if (!state.identity || !activeStates.includes(state.actualState)) continue;
      await this.serial(state.serviceId, async () => {
        const observed = await this.options.process.inspect(state.identity!);
        if (!observed?.alive) {
          return this.transition(state.serviceId, state.generation, state.desiredState === "running" ? "failed" : "stopped", "failed", { error: "Managed process is no longer alive", exitedAt: this.optionsClock.now() });
        }
        if (!this.identityMatchesState(state) || !(await this.observedMatches(state.identity!))) return this.orphan(state);
        const identity = { ...state.identity!, managerInstanceId: this.host.instanceId };
        await this.transition(state.serviceId, state.generation, "running-unready", "not-ready", { identity, clear: ["error"] });
        this.attachOutput(state.serviceId, identity);
        await this.readiness(state.serviceId, profileFor(this.host.catalog, state.serviceId), state.generation, identity, this.currentToken(state.serviceId), undefined, true);
      });
    }
  }

  /** Polls readiness of every `ownership: 'external'` service and adopts/releases it into this
   * manager's own state machine when its external readiness appears/disappears — generalizes
   * infra's docker/tailnet-task `syncExternalUnits`. */
  async syncExternalServices(): Promise<void> {
    for (const service of this.host.catalog.services) {
      if ((service.ownership ?? "daemon") !== "external") continue;
      if (this.active.has(service.id)) continue;
      await this.serial(service.id, async () => {
        if (this.active.has(service.id)) return;
        const state = this.state(service.id);
        const profile = service.profiles.run;
        if (profile.commandStatus !== "verified") return;
        const ready = await this.probe(profile.readiness, service.id);
        if (ready && (state?.actualState === "stopped" || state?.actualState === "failed")) {
          const generation = (state?.generation ?? 0) + 1;
          await this.transition(service.id, generation, "ready", "ready", {
            readinessKind: readinessName(profile.readiness),
            readinessDetail: "adopted from external state",
            desiredState: "running",
            clear: ["error", "exitCode", "exitedAt", "identity"],
          });
        } else if (!ready && state && (state.actualState === "ready" || state.actualState === "running-unready")) {
          await this.transition(service.id, state.generation, "stopped", "unknown", { desiredState: "stopped", exitedAt: this.optionsClock.now(), clear: ["error", "exitCode", "identity"] });
        }
      });
    }
  }

  private async startLocked(serviceId: ServiceId, operationId?: string): Promise<void> {
    if (this.options.isClosing?.()) return;
    const profile = profileFor(this.host.catalog, serviceId);
    const current = this.state(serviceId);
    const existing = this.active.get(serviceId);
    if (existing && !existing.stopped && current?.actualState !== "failed") return;
    if (existing) await this.finalize(existing, true);
    if (current?.identity && activeStates.includes(current.actualState) && (await this.owns(current))) return;
    const retainedIdentity = current?.identity && !isDockerIdentity(current.identity) && this.identityMatchesState(current) && (await this.observedMatches(current.identity)) ? current.identity : undefined;
    const generation = retainedIdentity?.generation ?? (current?.generation ?? 0) + 1;
    let token = this.cancel(serviceId);
    if (retainedIdentity) {
      const identity = { ...retainedIdentity, managerInstanceId: this.host.instanceId };
      await this.transition(serviceId, generation, "running-unready", "not-ready", { desiredState: "running", currentOperationId: operationId, identity, clear: ["error", "exitCode", "exitedAt"] });
      this.attachOutput(serviceId, identity);
      // An adopted identity is only worth keeping while it still answers its readiness probe. A live
      // wrapper whose real server already died (air keeps the wrapper alive after its child exits) or
      // that never came up would otherwise be re-adopted on every start, fail readiness, and leave the
      // caller stuck with a permanent "Readiness timed out" and an unkillable zombie process.
      const outcome = await this.awaitReadiness(serviceId, profile, generation, identity, token, true);
      if (outcome.kind === "ready" || outcome.kind === "superseded") return;
      token = this.cancel(serviceId);
      await this.terminate(identity, profile.command);
    }
    await this.transition(serviceId, generation, "preparing", "unknown", { desiredState: "running", currentOperationId: operationId, clear: ["error", "exitCode", "exitedAt", "identity"] });
    try {
      if (profile.preparation?.length) await this.options.preparation?.prepare(serviceId, profile.preparation);
      if (profile.preparationCommand) {
        const { command, cwd, serializationKey } = profile.preparationCommand;
        const run = () => this.options.probes.command?.(command, cwd) ?? Promise.resolve(undefined);
        const ok = serializationKey ? await this.serializedPreparationCommand(serializationKey, run) : await run();
        if (ok !== true) throw new Error("preparation command failed");
      }
    } catch {
      await this.transitionIfCurrent(serviceId, generation, token, "failed", "failed", { error: "Preparation failed", currentOperationId: operationId });
      throw new Error(`Preparation failed for ${serviceId}`);
    }
    if (!this.valid(serviceId, generation, token) || this.options.isClosing?.()) return;
    const definition = definitionFor(this.host.catalog, serviceId);
    if (definition?.profiles.build) await this.build(serviceId, definition, generation, token, operationId);
    if (!this.valid(serviceId, generation, token) || this.options.isClosing?.()) return;
    if (profile.readiness.kind === "tcp" && (await this.options.probes.portInUse?.(profile.readiness.port))) {
      await this.transition(serviceId, generation, "externally-owned", "failed", { error: `Port ${profile.readiness.port} is held by an unowned process` });
      throw new Error(`Port ${profile.readiness.port} is externally owned`);
    }
    await this.spawnAndWait(serviceId, profile, generation, token, operationId);
  }

  private async stopLocked(serviceId: ServiceId, operationId?: string): Promise<void> {
    const state = this.state(serviceId);
    if (!state || state.actualState === "stopped" || state.actualState === "externally-owned") return;
    const active = this.active.get(serviceId);
    if (!state.identity) {
      if (state.actualState === "queued-start" || state.actualState === "preparing" || state.actualState === "starting" || this.options.isClosing?.()) {
        await this.transition(serviceId, state.generation, "stopped", "unknown", { desiredState: "stopped", currentOperationId: operationId, exitedAt: this.optionsClock.now(), clear: ["error", "exitCode"] });
      } else {
        await this.orphan(state, operationId);
      }
      return;
    }
    if (!(await this.owns(state))) {
      const observed = await this.options.process.inspect(state.identity);
      if (!observed?.alive) {
        if (active) this.active.delete(serviceId);
        await this.transition(serviceId, state.generation, "stopped", "unknown", { desiredState: "stopped", exitedAt: this.optionsClock.now(), clear: ["error", "exitCode"] });
        return;
      }
      return this.orphan(state, operationId);
    }
    await this.transition(serviceId, state.generation, "stopping", state.readiness, { desiredState: "stopped", currentOperationId: operationId });
    if (active) active.stopped = true;
    await this.terminate(state.identity, profileFor(this.host.catalog, serviceId).command);
    if (active) this.active.delete(serviceId);
    await this.transition(serviceId, state.generation, "stopped", "unknown", { desiredState: "stopped", exitedAt: this.optionsClock.now(), clear: ["error", "exitCode"] });
  }

  private async spawnAndWait(serviceId: ServiceId, profile: VerifiedServiceRunProfile, generation: number, token: number, operationId?: string): Promise<void> {
    await this.transition(serviceId, generation, "starting", "not-ready", { desiredState: "running", currentOperationId: operationId, clear: ["identity"] });
    if (!this.valid(serviceId, generation, token) || this.options.isClosing?.()) return;
    const fingerprint = normalizeCommandFingerprint(profile.command);
    let app: ManagedProcess;
    try {
      const input: SpawnInput = { command: profile.command, commandFingerprint: fingerprint, serviceId, onOutput: (data) => { if (this.valid(serviceId, generation, token)) this.appendOutput(serviceId, data); } };
      if (isContainerCommand(profile.command)) {
        const previous = this.composeStartTail.catch(() => undefined);
        let release!: () => void;
        const turn = new Promise<void>((resolve) => {
          release = resolve;
        });
        this.composeStartTail = previous.then(() => turn);
        try {
          await previous;
          app = await this.options.process.spawn(input);
        } finally {
          release();
        }
      } else {
        app = await this.options.process.spawn(input);
      }
    } catch (error) {
      if (!this.valid(serviceId, generation, token) || this.options.isClosing?.()) return;
      const message = error instanceof Error && error.message ? error.message : "Service process failed to start";
      await this.transitionIfCurrent(serviceId, generation, token, "failed", "failed", { error: message, currentOperationId: operationId, clear: ["identity"] });
      throw error;
    }
    if (isTaskCommand(profile.readiness) && !isDockerIdentityRecord(app)) {
      await this.transition(serviceId, generation, "running-unready", "not-ready", { clear: ["identity"] });
      await this.readiness(serviceId, profile, generation, undefined, token, operationId);
      return;
    }
    const identity: ProcessIdentity = isDockerIdentityRecord(app)
      ? { managerInstanceId: this.host.instanceId, serviceId, generation, startedAt: this.optionsClock.now(), containerName: app.containerName, containerId: app.containerId, containerStartedAt: app.containerStartedAt, commandFingerprint: app.commandFingerprint }
      : { managerInstanceId: this.host.instanceId, serviceId, generation, startedAt: this.optionsClock.now(), pid: app.pid, pgid: app.pgid, startIdentity: app.startIdentity, commandFingerprint: app.commandFingerprint };
    if (!this.valid(serviceId, generation, token)) {
      await this.terminate(identity, profile.command);
      return;
    }
    const active: Active = { profile, app, identity, token, stopped: false };
    this.active.set(serviceId, active);
    this.attachOutput(serviceId, identity);
    void app.exited.then((code) => this.onExit(serviceId, generation, token, code)).catch(() => this.onExit(serviceId, generation, token, -1));
    await this.transition(serviceId, generation, "running-unready", "not-ready", { identity });
    await this.readiness(serviceId, profile, generation, identity, token, operationId);
  }

  private async readiness(serviceId: ServiceId, profile: VerifiedServiceRunProfile, generation: number, identity: ProcessIdentity | undefined, token: number, operationId?: string, adopted = false): Promise<void> {
    const outcome = await this.awaitReadiness(serviceId, profile, generation, identity, token, adopted);
    if (outcome.kind === "ready" || outcome.kind === "superseded") return;
    await this.fail(serviceId, generation, token, identity, outcome.message, operationId, outcome.kind === "timeout" ? { readinessKind: readinessName(profile.readiness), readinessDetail: outcome.detail } : {});
    throw new Error(outcome.message);
  }
  /** Non-throwing readiness wait. `superseded` means a newer operation (or a closing manager) took
   * over this service's token, so the caller must stop quietly instead of reporting a failure. */
  private async awaitReadiness(serviceId: ServiceId, profile: VerifiedServiceRunProfile, generation: number, identity: ProcessIdentity | undefined, token: number, adopted: boolean): Promise<ReadinessOutcome> {
    if (profile.readiness.kind === "process") {
      if (!this.valid(serviceId, generation, token)) return { kind: "superseded" };
      if (identity && !(await this.ownsIdentity(identity))) {
        return this.valid(serviceId, generation, token) ? { kind: "exited", message: "Process exited before liveness check" } : { kind: "superseded" };
      }
      await this.transitionIfCurrent(serviceId, generation, token, "running-unready", "not-ready", { identity, readinessKind: "process", readinessDetail: "process-liveness-only" });
      return { kind: "ready" };
    }
    const timeout = profile.readinessTimeoutMs ?? this.timeout;
    const deadline = Date.parse(this.optionsClock.now()) + timeout;
    while (Date.parse(this.optionsClock.now()) <= deadline) {
      if (!this.valid(serviceId, generation, token)) return { kind: "superseded" };
      if (identity && !(await this.ownsIdentity(identity))) {
        return this.valid(serviceId, generation, token) ? { kind: "exited", message: "Process exited before readiness" } : { kind: "superseded" };
      }
      if (await this.probe(profile.readiness, serviceId)) {
        await this.transitionIfCurrent(serviceId, generation, token, "ready", "ready", { identity, readinessKind: readinessName(profile.readiness), readinessDetail: adopted ? "adopted readiness verified" : "readiness verified", clear: ["error"] });
        return { kind: "ready" };
      }
      await this.optionsClock.sleep(this.backoff);
    }
    return { kind: "timeout", message: "Readiness timed out", detail: `Readiness ${readinessName(profile.readiness)} probe timed out after ${timeout}ms` };
  }

  private async fail(serviceId: ServiceId, generation: number, token: number, identity: ProcessIdentity | undefined, error: string, operationId?: string, changes: Pick<Changes, "readinessKind" | "readinessDetail"> = {}): Promise<void> {
    if (!this.valid(serviceId, generation, token)) return;
    await this.transitionIfCurrent(serviceId, generation, token, "failed", "failed", { identity, error, currentOperationId: operationId, ...changes });
    this.cancel(serviceId);
    const active = this.active.get(serviceId);
    if (active?.identity.generation === generation) await this.finalize(active, true);
  }
  private async onExit(serviceId: ServiceId, generation: number, token: number, code: number): Promise<void> {
    await this.serial(serviceId, async () => {
      if (!this.valid(serviceId, generation, token)) return;
      const state = this.state(serviceId);
      const active = this.active.get(serviceId);
      if (!state || state.generation !== generation || !active || active.stopped) return;
      await this.transitionIfCurrent(serviceId, generation, token, "failed", "failed", { exitCode: code, exitedAt: this.optionsClock.now(), error: `Process exited with code ${code}` });
      if (this.active.get(serviceId)?.identity.generation === generation) this.active.delete(serviceId);
    });
  }
  private async finalize(active: Active, terminate: boolean): Promise<void> {
    active.stopped = true;
    if (terminate && (await this.ownsIdentity(active.identity))) await this.terminate(active.identity, active.profile.command);
    if (this.active.get(active.identity.serviceId) === active) this.active.delete(active.identity.serviceId);
  }
  private async terminate(identity: ProcessIdentity, command: ServiceCommand): Promise<void> {
    if (!(await this.ownsIdentity(identity))) return;
    if (isDockerIdentity(identity)) {
      if (!this.options.process.stopContainer) throw new Error("Docker container stop is unavailable");
      await this.options.process.stopContainer(command);
      return;
    }
    this.outputTails.get(identity.serviceId)?.();
    this.outputTails.delete(identity.serviceId);
    const tree = await this.processTree(identity.pid, identity.startIdentity);
    await this.signalProcessTree(tree, identity.pgid, "SIGTERM");
    const deadline = Date.parse(this.optionsClock.now()) + this.grace;
    while (Date.parse(this.optionsClock.now()) < deadline) {
      if (!(await this.processTreeAlive(tree))) return;
      await this.optionsClock.sleep(this.backoff);
    }
    if (await this.processTreeAlive(tree)) await this.signalProcessTree(tree, identity.pgid, "SIGKILL");
  }
  /** Snapshot of the managed process's whole tree, taken while the caller still owns the leader.
   * Needed because a wrapper can hand its real server its own process group: `air` runs the built
   * binary in a fresh pgid, so signalling only the tracked pgid leaves that server alive holding its
   * port. Snapshotting first also survives the leader dying, which reparents its children to pid 1. */
  private async processTree(leaderPid: number, leaderStartIdentity: string): Promise<ProcessTreeEntry[]> {
    const { code, stdout } = await captureCommand(["ps", "-Ao", "pid=,ppid=,pgid=,lstart="]);
    if (code !== 0) return [];
    const rows: (ProcessTreeEntry & { ppid: number })[] = [];
    for (const line of stdout.split("\n")) {
      const match = /^(\d+)\s+(\d+)\s+(\d+)\s+(.{24})/.exec(line.trimStart());
      if (match) rows.push({ pid: Number(match[1]), ppid: Number(match[2]), pgid: Number(match[3]), startIdentity: match[4]!.trim() });
    }
    // Walk the tree only when the OS table itself still identifies `leaderPid` as the process we
    // recorded: a caller-supplied pid that no longer matches must never pull an unrelated live tree
    // (pid reuse, or a test/stub identity) into a signal or into the wait-for-death loop.
    if (rows.find((row) => row.pid === leaderPid)?.startIdentity !== leaderStartIdentity) return [];
    const byPid = new Map(rows.map((row) => [row.pid, row]));
    const byParent = new Map<number, (ProcessTreeEntry & { ppid: number })[]>();
    for (const row of rows) byParent.set(row.ppid, [...(byParent.get(row.ppid) ?? []), row]);
    const tree: ProcessTreeEntry[] = [];
    const seen = new Set<number>([leaderPid]);
    const queue = [leaderPid];
    while (queue.length > 0) {
      const pid = queue.shift()!;
      const row = byPid.get(pid);
      if (row) tree.push({ pid: row.pid, pgid: row.pgid, startIdentity: row.startIdentity });
      for (const child of byParent.get(pid) ?? []) {
        if (seen.has(child.pid)) continue;
        seen.add(child.pid);
        queue.push(child.pid);
      }
    }
    return tree;
  }
  /** A pid is only counted as still-ours when its start time matches the snapshot, so a recycled pid
   * can never keep a stop waiting (or, worse, earn a SIGKILL). */
  private async processTreeAlive(tree: readonly ProcessTreeEntry[]): Promise<boolean> {
    if (tree.length === 0) return false;
    const { code, stdout } = await captureCommand(["ps", "-Ao", "pid=,lstart="]);
    if (code !== 0) return false;
    const alive = new Map<number, string>();
    for (const line of stdout.split("\n")) {
      const match = /^(\d+)\s+(.{24})/.exec(line.trimStart());
      if (match) alive.set(Number(match[1]), match[2]!.trim());
    }
    return tree.some((entry) => alive.get(entry.pid) === entry.startIdentity);
  }
  private async signalProcessTree(tree: readonly ProcessTreeEntry[], leaderPgid: number, signal: ProcessSignal): Promise<void> {
    await this.options.process.signalGroup(leaderPgid, signal);
    const groups = new Map<number, ProcessTreeEntry[]>();
    for (const entry of tree) {
      if (entry.pgid === leaderPgid || entry.pgid <= 1) continue;
      groups.set(entry.pgid, [...(groups.get(entry.pgid) ?? []), entry]);
    }
    for (const [pgid, members] of groups) {
      let stillOurs = false;
      for (const member of members) {
        const observed = await observedSystemProcess(member.pid);
        if (observed && !isDockerObserved(observed) && observed.startIdentity === member.startIdentity) {
          stillOurs = true;
          break;
        }
      }
      if (stillOurs) await this.options.process.signalGroup(pgid, signal);
    }
  }
  private async build(serviceId: ServiceId, definition: ServiceDefinition, generation: number, token: number, operationId?: string): Promise<void> {
    const build = definition.profiles.build!;
    const controller = new AbortController();
    this.buildAborts.set(serviceId, controller);
    const timeout = setTimeout(() => controller.abort(new Error("Build timed out")), build.timeoutMs ?? 15 * 60_000);
    try {
      const run = () => this.options.runBuild(build.command, (data) => { if (this.valid(serviceId, generation, token)) this.appendOutput(serviceId, data); }, controller.signal);
      if (build.serializationKey) await this.serializedBuild(build.serializationKey, controller.signal, run);
      else await run();
    } catch (error) {
      if (!this.valid(serviceId, generation, token) || this.options.isClosing?.()) return;
      const message = controller.signal.aborted ? "Build timed out" : "Build failed";
      await this.transitionIfCurrent(serviceId, generation, token, "failed", "failed", { error: message, currentOperationId: operationId });
      throw error;
    } finally {
      clearTimeout(timeout);
      if (this.buildAborts.get(serviceId) === controller) this.buildAborts.delete(serviceId);
    }
  }
  private async serializedBuild(key: string, signal: AbortSignal, run: () => Promise<void>): Promise<void> {
    const previous = (this.buildSerials.get(key) ?? Promise.resolve()).catch(() => undefined);
    let release!: () => void;
    const turn = new Promise<void>((resolve) => {
      release = resolve;
    });
    this.buildSerials.set(key, previous.then(() => turn));
    await new Promise<void>((resolve, reject) => {
      const abort = (): void => {
        signal.removeEventListener("abort", abort);
        reject(signal.reason);
      };
      if (signal.aborted) return abort();
      signal.addEventListener("abort", abort, { once: true });
      void previous.then(async () => {
        signal.removeEventListener("abort", abort);
        if (signal.aborted) return reject(signal.reason);
        try {
          resolve(await run());
        } catch (error) {
          reject(error);
        }
      });
    }).finally(release);
  }
  /** Preparation analogue of `serializedBuild`, minus the cancellation plumbing (unlike a build,
   * nothing today cancels an in-flight preparation command). */
  private async serializedPreparationCommand(key: string, run: () => Promise<boolean | undefined>): Promise<boolean | undefined> {
    const previous = (this.preparationSerials.get(key) ?? Promise.resolve()).catch(() => undefined);
    let release!: () => void;
    const turn = new Promise<void>((resolve) => {
      release = resolve;
    });
    this.preparationSerials.set(key, previous.then(() => turn));
    try {
      await previous;
      return await run();
    } finally {
      release();
    }
  }
  private abortBuild(serviceId: ServiceId): void {
    this.buildAborts.get(serviceId)?.abort(new Error("Build cancelled"));
  }
  private async probe(readiness: ReadinessSpec, serviceId: ServiceId): Promise<boolean> {
    if (readiness.kind === "tcp") return this.options.probes.tcp(readiness.port);
    if (readiness.kind === "http") return this.options.probes.http(readiness.url);
    if (readiness.kind === "tailnet") return this.options.probes.tailnet();
    if (readiness.kind === "custom") return (await readiness.probe({ serviceId })) === "ready";
    if (readiness.kind === "command") return (await this.options.probes.command?.(readiness.command, readiness.cwd)) ?? false;
    if (readiness.kind === "container") {
      const profile = profileFor(this.host.catalog, serviceId);
      return profile.command.containerName ? this.options.probes.container(profile.command.containerName) : false;
    }
    return false;
  }
  private attachOutput(serviceId: ServiceId, identity: ProcessIdentity): void {
    if (isDockerIdentity(identity)) return;
    this.outputTails.get(serviceId)?.();
    this.outputTails.delete(serviceId);
    const stop = this.options.process.attachOutput?.(serviceId, (data) => this.appendOutput(serviceId, data));
    if (stop) this.outputTails.set(serviceId, stop);
  }
  /** Log forwarding is fire-and-forget by design: a dropped or rejected append must never reject into
   * the caller (Bun terminates the process on an unhandled rejection, orphaning every managed
   * service). Failures surface through the host's background-error hook instead. */
  private appendOutput(serviceId: ServiceId, data: string): void {
    this.host.appendLog(serviceId, data).catch((error: unknown) => this.host.recordBackgroundError?.(`service-log:${serviceId}`, error));
  }
  private state(serviceId: ServiceId): ServiceLifecycleState | undefined {
    return this.host.serviceStates().find((state) => state.serviceId === serviceId);
  }
  private currentToken(serviceId: ServiceId): number {
    return this.tokens.get(serviceId) ?? 0;
  }
  private cancel(serviceId: ServiceId): number {
    const token = this.currentToken(serviceId) + 1;
    this.tokens.set(serviceId, token);
    return token;
  }
  private valid(serviceId: ServiceId, generation: number, token: number): boolean {
    return this.currentToken(serviceId) === token && this.state(serviceId)?.generation === generation;
  }
  private identityMatchesState(state: ServiceLifecycleState): boolean {
    return state.identity !== undefined && state.identity.serviceId === state.serviceId && state.identity.generation === state.generation;
  }
  private async owns(state: ServiceLifecycleState): Promise<boolean> {
    return this.identityMatchesState(state) && (await this.ownsIdentity(state.identity!));
  }
  private async observedMatches(identity: ProcessIdentity): Promise<boolean> {
    const observed = await this.options.process.inspect(identity);
    if (!observed?.alive || observed.commandFingerprint !== identity.commandFingerprint) return false;
    if (isDockerIdentity(identity)) return isDockerObserved(observed) && observed.containerName === identity.containerName && observed.containerId === identity.containerId && observed.containerStartedAt === identity.containerStartedAt;
    return !isDockerObserved(observed) && observed.pid === identity.pid && observed.pgid === identity.pgid && observed.startIdentity === identity.startIdentity;
  }
  private async ownsIdentity(identity: ProcessIdentity): Promise<boolean> {
    return identity.managerInstanceId === this.host.instanceId && (await this.observedMatches(identity));
  }
  private async orphan(state: ServiceLifecycleState, operationId?: string): Promise<void> {
    await this.transition(state.serviceId, state.generation, "orphaned", "failed", { error: "Process ownership identity no longer matches", currentOperationId: operationId });
  }
  private async transitionIfCurrent(serviceId: ServiceId, generation: number, token: number, actual: ActualServiceState, readiness: ServiceReadiness, changes: Changes): Promise<void> {
    if (this.valid(serviceId, generation, token)) await this.transition(serviceId, generation, actual, readiness, changes);
  }
  private async transition(serviceId: ServiceId, generation: number, actualState: ActualServiceState, readiness: ServiceReadiness, changes: Changes): Promise<void> {
    const previous = this.state(serviceId);
    const clear = new Set(changes.clear ?? []);
    const field = <K extends "error" | "exitCode" | "exitedAt" | "currentOperationId">(key: K): ServiceLifecycleState[K] => (clear.has(key) ? undefined : (changes[key] ?? previous?.[key])) as ServiceLifecycleState[K];
    const next: ServiceLifecycleState = {
      serviceId,
      desiredState: changes.desiredState ?? previous?.desiredState ?? "running",
      actualState,
      readiness,
      generation,
      createdAt: previous?.createdAt ?? this.optionsClock.now(),
      updatedAt: this.optionsClock.now(),
      identity: clear.has("identity") ? undefined : (changes.identity ?? previous?.identity),
      readinessKind: changes.readinessKind ?? previous?.readinessKind,
      readinessDetail: changes.readinessDetail ?? previous?.readinessDetail,
      exitedAt: field("exitedAt"),
      exitCode: field("exitCode"),
      error: field("error"),
      currentOperationId: field("currentOperationId"),
    };
    await this.host.setServiceState(next);
    this.host.publish("service.lifecycle", { serviceId, actualState, readiness, generation, operationId: next.currentOperationId ?? null });
    if (next.error) await this.host.appendLog(serviceId, `${next.updatedAt} ${next.error}\n`);
  }
  private serial<T>(serviceId: ServiceId, work: () => Promise<T>): Promise<T> {
    const previous = this.queues.get(serviceId) ?? Promise.resolve();
    const next = previous.catch(() => undefined).then(work);
    this.queues.set(
      serviceId,
      next.then(
        () => undefined,
        () => undefined,
      ),
    );
    return next;
  }
}

// -------------------------------------------------------------------------------------------
// Default (production) adapters — POSIX/Docker process management via Bun + system tools.
// -------------------------------------------------------------------------------------------

type PosixChild = { pid: number; exited: Promise<number>; kill(signal: ProcessSignal): void };
const stopUnverifiedChild = async (child: PosixChild): Promise<void> => {
  child.kill("SIGTERM");
  await Promise.race([child.exited.then(() => undefined), Bun.sleep(500)]);
};
export const forwardStream = async (stream: ReadableStream<Uint8Array> | null, onOutput?: (data: string) => void): Promise<void> => {
  if (!stream) return;
  const reader = stream.getReader();
  const decoder = new TextDecoder();
  try {
    for (;;) {
      const { done, value } = await reader.read();
      if (done) break;
      const chunk = decoder.decode(value, { stream: true });
      if (chunk) onOutput?.(chunk);
    }
    const final = decoder.decode();
    if (final) onOutput?.(final);
  } finally {
    reader.releaseLock();
  }
};
const RAW_LOG_POLL_MS = 200;
export function tailFile(path: string, onOutput: ((data: string) => void) | undefined): { stop: () => void } {
  let stopped = false;
  let offset = 0;
  const decoder = new TextDecoder();
  const drainOnce = (): void => {
    try {
      const size = statSync(path, { throwIfNoEntry: false })?.size ?? 0;
      if (size <= offset) return;
      const fd = openSync(path, "r");
      try {
        const length = size - offset;
        const buffer = Buffer.alloc(length);
        readSync(fd, buffer, 0, length, offset);
        const text = decoder.decode(buffer, { stream: true });
        if (text) onOutput?.(text);
      } finally {
        closeSync(fd);
      }
      // copytruncate: shrink the capture file back to empty so a long-lived service's raw output
      // never grows unbounded between polls. The writer's fd is opened O_APPEND, so its next
      // write always lands at the (now shorter) current end of file, never a stale offset.
      //
      // Only truncate when the file is STILL exactly the size that was just read. The child writes
      // to it continuously and independently, so anything it appended between the read above and
      // this call would be destroyed by an unconditional truncate — silently losing log lines on
      // every poll of a chatty service. When it has grown, leave the bytes in place and just
      // advance the offset; whichever later poll catches the file quiescent truncates it.
      const writeFd = openSync(path, "r+");
      try {
        if ((fstatSync(writeFd).size ?? 0) === size) {
          ftruncateSync(writeFd, 0);
          offset = 0;
        } else {
          offset = size;
        }
      } finally {
        closeSync(writeFd);
      }
    } catch {}
  };
  const pump = async (): Promise<void> => {
    while (!stopped) {
      drainOnce();
      await Bun.sleep(RAW_LOG_POLL_MS);
    }
  };
  void pump();
  return {
    stop: () => {
      stopped = true;
      drainOnce();
    },
  };
}
const tcpProbe = (port: number): Promise<boolean> =>
  new Promise((resolve) => {
    const socket = connect({ host: "127.0.0.1", port });
    const done = (ready: boolean) => {
      socket.destroy();
      resolve(ready);
    };
    socket.once("connect", () => done(true));
    socket.once("error", () => done(false));
    socket.setTimeout(250, () => done(false));
  });
const execIdentitySettleMs = 1_000;
/** Every diagnostic shell-out goes through an async spawn: this runs on the daemon's own event loop
 * next to the loopback HTTP server, so a synchronous `Bun.spawnSync` (docker inspect x N per poll
 * tick, tailscale, ps) stalls readiness probes and the manager's own health endpoint long enough for
 * a client to conclude the daemon is dead and start a competing one. */
const captureCommand = async (argv: readonly string[]): Promise<{ code: number; stdout: string }> => {
  const child = Bun.spawn([...argv], { stdout: "pipe", stderr: "ignore" });
  const stdout = await new Response(child.stdout).text();
  return { code: await child.exited, stdout };
};
const observedSystemProcess = async (pid: number): Promise<ObservedProcess | undefined> => {
  const result = await captureCommand(["ps", "-o", "pid=", "-o", "pgid=", "-o", "lstart=", "-o", "command=", "-p", String(pid)]);
  if (result.code !== 0) return undefined;
  const match = /^(\d+)\s+(\d+)\s+(.{24})\s+(.+)$/.exec(result.stdout.trim());
  if (!match) return undefined;
  return { pid: Number(match[1]), pgid: Number(match[2]), startIdentity: match[3]!.trim(), commandFingerprint: normalizeObservedCommandFingerprint(match[4]!), alive: true };
};
const samePosixProcess = (expected: PosixProcessRecord, observed: ObservedProcess | undefined): observed is PosixProcessRecord & { alive: boolean } =>
  observed !== undefined && !isDockerObserved(observed) && observed.pid === expected.pid && observed.pgid === expected.pgid && observed.startIdentity === expected.startIdentity;
const observedStableExecProcess = async (child: PosixChild, expectedFingerprint: string): Promise<PosixProcessRecord> => {
  let candidate: PosixProcessRecord | undefined;
  for (let attempt = 0; attempt < 20; attempt++) {
    await Bun.sleep(25);
    const observed = await observedSystemProcess(child.pid);
    if (!observed || isDockerObserved(observed) || observed.commandFingerprint === expectedFingerprint) {
      candidate = undefined;
      continue;
    }
    if (!candidate || !samePosixProcess(candidate, observed) || candidate.commandFingerprint !== observed.commandFingerprint) {
      candidate = observed;
      continue;
    }
    await Bun.sleep(execIdentitySettleMs);
    const settled = await observedSystemProcess(child.pid);
    if (settled && !isDockerObserved(settled) && samePosixProcess(candidate, settled) && settled.commandFingerprint === candidate.commandFingerprint) return settled;
    candidate = undefined;
  }
  await stopUnverifiedChild(child);
  throw new Error("Unable to establish stable POSIX exec process identity");
};
const processInspectionAvailable = async (): Promise<boolean> => (await captureCommand(["ps", "-o", "pid=", "-p", String(process.pid)])).code === 0;
const containerRunning = async (containerName: string): Promise<boolean> => (await captureCommand(["docker", "inspect", "-f", "{{.State.Running}}", containerName])).stdout.trim() === "true";
const containerRecord = async (containerName: string, commandFingerprint: string): Promise<DockerContainerRecord | undefined> => {
  const output = (await captureCommand(["docker", "inspect", "-f", "{{.Id}}\t{{.State.Running}}\t{{.State.StartedAt}}", containerName])).stdout.trim().split("\t");
  return output.length === 3 && output[1] === "true" && output[0] && output[2] ? { containerName, containerId: output[0], containerStartedAt: output[2], commandFingerprint } : undefined;
};
const sameContainerInstance = (expected: DockerContainerRecord, observed: DockerContainerRecord | undefined): boolean =>
  observed !== undefined && observed.containerName === expected.containerName && observed.containerId === expected.containerId && observed.containerStartedAt === expected.containerStartedAt;
const tailnetServing = async (): Promise<boolean> => {
  const result = await captureCommand(["tailscale", "serve", "status", "--json"]);
  if (result.code !== 0) return false;
  try {
    const parsed = JSON.parse(result.stdout) as { Web?: Record<string, unknown> };
    return Object.keys(parsed.Web ?? {}).length > 0;
  } catch {
    return false;
  }
};
const runCommand = async (argv: readonly string[], cwd: string, env?: Record<string, string | undefined>, onOutput?: (data: string) => void): Promise<number> => {
  const child = Bun.spawn([...argv], { cwd, env, stdout: "pipe", stderr: "pipe" });
  void Promise.all([forwardStream(child.stdout, onOutput), forwardStream(child.stderr, onOutput)]).catch(() => undefined);
  return child.exited;
};

/** `baseEnvironment` defaults to `process.env` — i.e. whatever spawned the daemon. A daemon launched
 * from a GUI (Finder/Dock/LaunchAgent) inherits a bare `PATH` with no login-shell customization, so a
 * generic host process should resolve one itself (see `resolveBaseEnvironment` in `./env`) and pass
 * it here rather than relying on this default. */
export const defaultSupervisorOptions = (root: string = process.cwd(), runtimeDirectory: string = resolveRuntimeDirectory(root), baseEnvironment: Record<string, string> = process.env as Record<string, string>): SupervisorOptions => ({
  process: {
    spawn: async ({ command, commandFingerprint, serviceId, onOutput }) => {
      const { argv, exec } = commandArgv(command.command);
      const env = command.environment ? { ...baseEnvironment, ...command.environment } : baseEnvironment;
      if (isContainerCommand(command)) {
        const result = await runCommand(argv, join(root, command.cwd), env, onOutput);
        if (result !== 0) throw new Error(`Docker service command exited with ${result}`);
        const containerName = command.containerName!;
        const record = await containerRecord(containerName, commandFingerprint);
        if (!record) throw new Error(`Docker container ${containerName} is not running after start`);
        let finish: ((code: number) => void) | undefined;
        const exited = new Promise<number>((resolve) => {
          finish = resolve;
        });
        const timer = setInterval(() => {
          void containerRecord(containerName, commandFingerprint)
            .catch(() => undefined)
            .then((current) => {
              if (sameContainerInstance(record, current)) return;
              clearInterval(timer);
              finish?.(0);
            });
        }, 500);
        return { ...record, exited };
      }
      if (!(await processInspectionAvailable())) throw new Error("POSIX process inspection is unavailable; refusing to start an unverified service process");
      // Managed dev processes must outlive the daemon that spawned them. Piping their stdout/stderr
      // straight into this daemon made that a lie: once the daemon exits for any reason, the pipe's
      // read end closes, and the child's next log write earns it a SIGPIPE — a real (non-Node)
      // process almost always dies from that by default. Redirect to a plain file instead so the
      // child's survival never depends on anyone draining a pipe; attachOutput tails that file.
      mkdirSync(logsDir(runtimeDirectory), { recursive: true });
      const raw = rawLogPath(runtimeDirectory, serviceId);
      closeSync(openSync(raw, "w"));
      const rawFd = openSync(raw, "a");
      let child: ReturnType<typeof Bun.spawn>;
      try {
        child = Bun.spawn(argv, { cwd: join(root, command.cwd), env, stdout: rawFd, stderr: rawFd, detached: true });
      } finally {
        closeSync(rawFd);
      }
      if (exec) {
        const record = await observedStableExecProcess(child, commandFingerprint);
        return { ...record, exited: child.exited };
      }
      // A freshly spawned child can still be mid-`execve` when `ps` is asked about it, and macOS
      // then reports a placeholder command line — literally `(sh)` — instead of the real one
      // (reproducible: ~15% of spawns under load). Taking that first readable row as the
      // authoritative fingerprint poisons the identity for the rest of the service's life: once
      // exec completes, `ps` reports the real command, `ownsIdentity` compares unequal, and the
      // daemon disowns and orphans the service it just started — after which the next start fails
      // with `Port N is held by an unowned process`.
      //
      // The observed value (not the expected one) still has to be what gets stored: for an argv
      // command, `ps` reports the resolved binary path where `argv[0]` may have been a bare name,
      // and later `inspect` comparisons are made against `ps` output. So this waits for an
      // observation that is trustworthy instead of substituting the expected fingerprint:
      // either it already equals what we spawned, or it repeats identically across two polls —
      // which a mid-exec placeholder never does.
      let observed: ObservedProcess | undefined;
      let previous: ObservedProcess | undefined;
      for (let attempt = 0; attempt < 8; attempt++) {
        const current = await observedSystemProcess(child.pid);
        if (current && !isDockerObserved(current)) {
          if (current.commandFingerprint === commandFingerprint) {
            observed = current;
            break;
          }
          if (previous && !isDockerObserved(previous) && previous.pid === current.pid && previous.pgid === current.pgid && previous.startIdentity === current.startIdentity && previous.commandFingerprint === current.commandFingerprint) {
            observed = current;
            break;
          }
        }
        previous = current;
        if (attempt < 7) await Bun.sleep(25);
      }
      if (!observed || isDockerObserved(observed)) {
        await stopUnverifiedChild(child);
        throw new Error("Unable to establish POSIX process ownership identity after 8 inspections");
      }
      return { ...observed, exited: child.exited };
    },
    inspect: async (identity) => {
      if (isDockerIdentity(identity)) {
        const record = await containerRecord(identity.containerName, identity.commandFingerprint);
        return record && sameContainerInstance(identity, record) ? { ...record, alive: true } : undefined;
      }
      if (identity.pid === 0) return { pid: 0, pgid: 0, startIdentity: "", commandFingerprint: identity.commandFingerprint, alive: false };
      return await observedSystemProcess(identity.pid);
    },
    signalGroup: async (pgid, signal) => {
      if (pgid === 0) return;
      try {
        process.kill(-pgid, signal);
      } catch {}
    },
    stopContainer: async (command, onOutput) => {
      const { argv } = commandArgv(command.dockerStopCommand ?? { argv: ["docker", "compose", "stop"] });
      const code = await runCommand(argv, root, baseEnvironment, onOutput);
      if (code !== 0) throw new Error(`Docker service stop exited with ${code}`);
    },
    attachOutput: (serviceId, onOutput) => tailFile(rawLogPath(runtimeDirectory, serviceId), onOutput).stop,
  },
  runBuild: async (command, onOutput, signal) => {
    const { argv } = commandArgv(command.command);
    const env = command.environment ? { ...baseEnvironment, ...command.environment } : baseEnvironment;
    if (signal.aborted) throw signal.reason;
    const child = Bun.spawn(argv, { cwd: join(root, command.cwd), env, stdout: "pipe", stderr: "pipe", detached: true });
    void Promise.all([forwardStream(child.stdout, onOutput), forwardStream(child.stderr, onOutput)]).catch(() => undefined);
    let stopping: Promise<void> | undefined;
    const stop = (): void => {
      stopping ??= (async () => {
        try {
          process.kill(-child.pid, "SIGTERM");
        } catch {
          child.kill("SIGTERM");
        }
        await Promise.race([child.exited.then(() => undefined), Bun.sleep(5_000)]);
        try {
          process.kill(-child.pid, "SIGKILL");
        } catch {
          child.kill("SIGKILL");
        }
        await child.exited;
      })();
    };
    signal.addEventListener("abort", stop, { once: true });
    const code = await child.exited;
    signal.removeEventListener("abort", stop);
    if (signal.aborted) throw signal.reason;
    if (code !== 0) throw new Error(`Build command exited with ${code}`);
  },
  probes: {
    tcp: tcpProbe,
    http: async (url) => { try { return (await fetch(url, { signal: AbortSignal.timeout(250) })).ok; } catch { return false; } },
    container: (containerName) => containerRunning(containerName),
    tailnet: () => tailnetServing(),
    portInUse: tcpProbe,
    command: async (command, cwd) => (await runCommand(commandArgv(command).argv, join(root, cwd ?? "."), baseEnvironment)) === 0,
  },
});
