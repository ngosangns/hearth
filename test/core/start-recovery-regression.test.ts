import { describe, expect, test } from "bun:test";

import type { ServiceCatalog, ServiceId } from "../../src/core/catalog";
import type { ProcessIdentity, ServiceLifecycleState } from "../../src/core/state";
import { normalizeCommandFingerprint, ProcessSupervisor, type ManagedProcess, type ObservedProcess, type ProcessSignal, type SpawnInput, type SupervisorClock, type SupervisorOptions } from "../../src/core/supervisor";

// Regression tests for infra's start-recovery fixes (ported from the session that produced them):
//  1. adopting a still-alive identity must not be a dead end. A wrapper can outlive the server it
//     started (air keeps running after its child exits), so re-adopting it on every start produced a
//     permanent "Readiness timed out" that no amount of starting could clear.
//  2. fire-and-forget log forwarding must never reject into the daemon: Bun terminates a process on
//     an unhandled rejection, which orphans every service that daemon manages.

const serviceId: ServiceId = "sample-service";
const command = { command: { argv: ["serve"] }, cwd: "." };
const fingerprint = normalizeCommandFingerprint(command);
const catalog: ServiceCatalog = {
  startFailurePolicy: "stop-on-first-failure-keep-started",
  services: [{ id: serviceId, profiles: { run: { commandStatus: "verified", command, readiness: { kind: "tcp", port: 4242 } } } }],
  groups: {},
};

class FakeClock implements SupervisorClock {
  milliseconds = Date.parse("2026-09-19T00:00:00.000Z");
  now(): string {
    return new Date(this.milliseconds).toISOString();
  }
  async sleep(milliseconds: number): Promise<void> {
    this.milliseconds += milliseconds;
  }
}

type ProcessRecord = Extract<ObservedProcess, { pid: number }> & { exited: Promise<number> };
class FakeProcess {
  readonly inputs: SpawnInput[] = [];
  readonly signals: Array<{ pgid: number; signal: ProcessSignal }> = [];
  readonly records = new Map<number, ProcessRecord>();
  writtenOutput: string | undefined;
  #pid = 5_000;
  async spawn(input: SpawnInput): Promise<ManagedProcess> {
    this.inputs.push(input);
    const pid = this.#pid++;
    const record: ProcessRecord = { pid, pgid: pid, startIdentity: `start-${pid}`, commandFingerprint: input.commandFingerprint, alive: true, exited: new Promise<number>(() => undefined) };
    this.records.set(pid, record);
    return record;
  }
  async inspect(identity: ProcessIdentity): Promise<ObservedProcess | undefined> {
    if (!("pid" in identity)) return undefined;
    return this.records.get(identity.pid);
  }
  async signalGroup(pgid: number, signal: ProcessSignal): Promise<void> {
    this.signals.push({ pgid, signal });
    const record = this.records.get(pgid);
    if (record) record.alive = false;
  }
  /** Stands in for the file tailing the real adapter does; delivering through the supervisor's own
   * sink keeps this on the production path it is meant to cover. */
  attachOutput(_serviceId: ServiceId, onOutput: (data: string) => void): () => void {
    const output = this.writtenOutput;
    if (output !== undefined) queueMicrotask(() => onOutput(output));
    return () => undefined;
  }
}

class FakeHost {
  instanceId = "manager-a";
  readonly states = new Map<ServiceId, ServiceLifecycleState>();
  readonly backgroundErrors: Array<{ scope: string; error: unknown }> = [];
  readonly firstBackgroundError = Promise.withResolvers<{ scope: string; error: unknown }>();
  constructor(
    readonly catalog: ServiceCatalog,
    private readonly failAppendLog = false,
  ) {}
  serviceStates(): ServiceLifecycleState[] {
    return [...this.states.values()];
  }
  async setServiceState(next: ServiceLifecycleState): Promise<void> {
    this.states.set(next.serviceId, structuredClone(next));
  }
  async appendLog(): Promise<void> {
    if (this.failAppendLog) throw new Error("log write failed");
  }
  publish(): void {}
  recordBackgroundError(scope: string, error: unknown): void {
    const entry = { scope, error };
    this.backgroundErrors.push(entry);
    this.firstBackgroundError.resolve(entry);
  }
}

const adoptedIdentity = () => ({ managerInstanceId: "dead-daemon", serviceId, generation: 7, startedAt: "2026-09-19T00:00:00.000Z", pid: 4_242, pgid: 4_242, startIdentity: "zombie", commandFingerprint: fingerprint });
const adoptedState = (): ServiceLifecycleState => ({ serviceId, desiredState: "running", actualState: "ready", readiness: "ready", generation: 7, createdAt: "2026-09-19T00:00:00.000Z", updatedAt: "2026-09-19T00:00:00.000Z", identity: adoptedIdentity() });

const setup = (options: { failAppendLog?: boolean } = {}) => {
  const process = new FakeProcess();
  const host = new FakeHost(catalog, options.failAppendLog ?? false);
  process.records.set(4_242, { pid: 4_242, pgid: 4_242, startIdentity: "zombie", commandFingerprint: fingerprint, alive: true, exited: new Promise<number>(() => undefined) });
  host.states.set(serviceId, adoptedState());
  let adoptedReady = false;
  const supervisor = new ProcessSupervisor(host, {
    process,
    runBuild: async () => undefined,
    clock: new FakeClock(),
    readinessBackoffMs: 10,
    readinessTimeoutMs: 100,
    terminationGraceMs: 10,
    probes: { tcp: async () => adoptedReady || process.inputs.length > 0, http: async () => false, container: async () => false, tailnet: async () => false, portInUse: async () => false },
  } satisfies SupervisorOptions);
  return { host, markAdoptedReady: () => (adoptedReady = true), process, supervisor };
};

describe("start recovery for an adopted identity", () => {
  test("replaces an adopted process that is alive but still fails its readiness probe", async () => {
    const { host, process, supervisor } = setup();
    await supervisor.start(serviceId);

    // The zombie never answered, so it must be terminated and replaced — pre-fix the supervisor kept
    // it, failed readiness, and left the caller stuck on "Readiness timed out".
    expect(process.signals).toContainEqual({ pgid: 4_242, signal: "SIGTERM" });
    expect(process.inputs).toHaveLength(1);
    expect(host.states.get(serviceId)?.actualState).toBe("ready");
  });

  test("keeps an adopted process that still answers readiness, without restarting it", async () => {
    const { markAdoptedReady, process, supervisor } = setup();
    markAdoptedReady();
    await supervisor.start(serviceId);

    expect(process.inputs).toHaveLength(0);
    expect(process.signals).toHaveLength(0);
    expect(process.records.get(4_242)?.alive).toBe(true);
  });

  test("a rejected log append is reported to the host, not raised as an unhandled rejection", async () => {
    const { host, markAdoptedReady, process, supervisor } = setup({ failAppendLog: true });
    markAdoptedReady();
    process.writtenOutput = "chunk";
    await supervisor.start(serviceId);
    const reported = await host.firstBackgroundError.promise;

    expect(reported.scope).toBe(`service-log:${serviceId}`);
    expect(host.states.get(serviceId)?.actualState).toBe("ready");
  });
});
