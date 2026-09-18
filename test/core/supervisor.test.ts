import { afterEach, describe, expect, test } from "bun:test";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { defaultSupervisorOptions, normalizeCommandFingerprint, normalizeObservedCommandFingerprint, ProcessSupervisor, type ManagedProcess, type ObservedProcess, type SpawnInput, type SupervisorClock, type SupervisorOptions } from "../../src/core/supervisor";
import type { CommandSpec, ServiceCatalog, ServiceId } from "../../src/core/catalog";
import type { ProcessIdentity, ServiceLifecycleState } from "../../src/core/state";

const scratchDirs: string[] = [];
afterEach(async () => {
  await Promise.all(scratchDirs.splice(0).map((dir) => rm(dir, { recursive: true, force: true })));
});
async function scratchRoot(): Promise<string> {
  const dir = await mkdtemp(join(tmpdir(), "local-services-supervisor-"));
  scratchDirs.push(dir);
  return dir;
}

/** These fixtures never actually execute a shell — CommandSpec is just an opaque label carried
 * through spawn/build fakes, so tests can assert on it the same way the original (bare shell
 * string) tests did. */
const commandLabel = (spec: CommandSpec): string => ("argv" in spec ? spec.argv.join(" ") : spec.shell);
const shellCommand = (label: string, extra: Partial<{ containerName: string; dockerStopCommand: string; exec: boolean; environment: Record<string, string> }> = {}) => ({
  command: { shell: label, exec: extra.exec } satisfies CommandSpec,
  cwd: ".",
  containerName: extra.containerName,
  dockerStopCommand: extra.dockerStopCommand ? ({ shell: extra.dockerStopCommand } satisfies CommandSpec) : undefined,
  environment: extra.environment,
});

class FakeClock implements SupervisorClock {
  milliseconds = Date.parse("2026-09-08T00:00:00.000Z");
  now(): string {
    return new Date(this.milliseconds).toISOString();
  }
  async sleep(milliseconds: number): Promise<void> {
    this.milliseconds += milliseconds;
  }
}

type ProcessRecord = Extract<ObservedProcess, { pid: number }> & { exited: Promise<number>; exit(code: number): void };
class FakeProcess {
  readonly inputs: SpawnInput[] = [];
  readonly signals: Array<{ pgid: number; signal: "SIGTERM" | "SIGKILL" }> = [];
  readonly records = new Map<number, ProcessRecord>();
  readonly inspected: ProcessIdentity[] = [];
  #pid = 100;
  async spawn(input: SpawnInput): Promise<ManagedProcess> {
    this.inputs.push(input);
    const pid = this.#pid++;
    const resolvers = Promise.withResolvers<number>();
    const record: ProcessRecord = {
      pid,
      pgid: pid,
      startIdentity: `start-${pid}`,
      commandFingerprint: input.commandFingerprint,
      alive: true,
      exited: resolvers.promise,
      exit: (code) => {
        if (!record.alive) return;
        record.alive = false;
        resolvers.resolve(code);
      },
    };
    this.records.set(pid, record);
    return record;
  }
  async inspect(identity: ProcessIdentity): Promise<ObservedProcess | undefined> {
    this.inspected.push(identity);
    return "containerId" in identity ? { containerName: identity.containerName, containerId: identity.containerId, containerStartedAt: identity.containerStartedAt, commandFingerprint: identity.commandFingerprint, alive: true } : this.records.get(identity.pid);
  }
  async signalGroup(pgid: number, signal: "SIGTERM" | "SIGKILL"): Promise<void> {
    this.signals.push({ pgid, signal });
    this.records.get(pgid)?.exit(signal === "SIGTERM" ? 0 : -9);
  }
}

class FakeHost {
  instanceId = "manager-a";
  readonly states = new Map<ServiceId, ServiceLifecycleState>();
  readonly events: Array<{ type: string; data: Record<string, unknown> }> = [];
  constructor(readonly catalog: ServiceCatalog) {}
  serviceStates(): ServiceLifecycleState[] {
    return [...this.states.values()];
  }
  async setServiceState(next: ServiceLifecycleState): Promise<void> {
    this.states.set(next.serviceId, structuredClone(next));
  }
  async appendLog(): Promise<void> {}
  publish(type: string, data: Record<string, unknown>): void {
    this.events.push({ type, data });
  }
}

const catalog: ServiceCatalog = {
  startFailurePolicy: "stop-on-first-failure-keep-started",
  services: [{ id: "metadata", profiles: { run: { commandStatus: "verified", command: shellCommand("serve metadata"), readiness: { kind: "tcp", port: 1166 } }, build: { command: shellCommand("build metadata") } } }],
  groups: { one: ["metadata"] },
};

const setup = (overrides: Partial<SupervisorOptions> = {}, serviceCatalog = catalog) => {
  const process = new FakeProcess();
  const host = new FakeHost(serviceCatalog);
  const builds: string[] = [];
  const options: SupervisorOptions = {
    process,
    runBuild: async (command) => {
      builds.push(commandLabel(command.command));
    },
    clock: new FakeClock(),
    readinessBackoffMs: 10,
    readinessTimeoutMs: 100,
    terminationGraceMs: 10,
    probes: { tcp: async () => true, http: async () => false, container: async () => false, tailnet: async () => false, portInUse: async () => false },
    ...overrides,
  };
  return { builds, options, process, host, supervisor: new ProcessSupervisor(host, options) };
};

describe("default supervisor options — real process spawn/build", () => {
  test("normalizes supported shell wrappers around the direct installed runtime", () => {
    const command = shellCommand("cd -- 'portal/backend' && exec 'build/install/portal.backend/bin/portal.backend'");
    const expected = normalizeCommandFingerprint(command);
    expect(normalizeObservedCommandFingerprint("sh -c cd -- 'portal/backend' && exec 'build/install/portal.backend/bin/portal.backend'")).toBe(expected);
    expect(normalizeObservedCommandFingerprint("/bin/sh -lc cd -- 'portal/backend' && exec 'build/install/portal.backend/bin/portal.backend'")).toBe(expected);
  });

  test("waits for an exec runtime fingerprint while retaining its POSIX process identity", async () => {
    const root = await scratchRoot();
    const options = defaultSupervisorOptions(root);
    const command = shellCommand("exec sleep 30", { exec: true });
    const app = await options.process.spawn({ command, commandFingerprint: normalizeCommandFingerprint(command), serviceId: "metadata" });

    expect("pid" in app).toBeTrue();
    if (!("pid" in app)) throw new Error("Expected a POSIX process");
    expect(app.commandFingerprint).not.toBe(normalizeCommandFingerprint(command));
    expect(await options.process.inspect({ managerInstanceId: "test", serviceId: "metadata", generation: 1, startedAt: new Date().toISOString(), ...app })).toMatchObject({ pid: app.pid, pgid: app.pgid, startIdentity: app.startIdentity, commandFingerprint: app.commandFingerprint, alive: true });
    await options.process.signalGroup(app.pgid, "SIGTERM");
    await app.exited;
  }, 10_000);

  test("keeps the shell command fingerprint for a non-exec runtime", async () => {
    const root = await scratchRoot();
    const options = defaultSupervisorOptions(root);
    const command = shellCommand("sleep 30; :");
    const fingerprint = normalizeCommandFingerprint(command);
    const app = await options.process.spawn({ command, commandFingerprint: fingerprint, serviceId: "metadata" });

    expect("pid" in app).toBeTrue();
    if (!("pid" in app)) throw new Error("Expected a POSIX process");
    expect(app.commandFingerprint).toBe(fingerprint);
    await options.process.signalGroup(app.pgid, "SIGTERM");
    await app.exited;
  }, 10_000);

  test("terminates a real detached build process when cancelled", async () => {
    const root = await scratchRoot();
    const options = defaultSupervisorOptions(root);
    const controller = new AbortController();
    const build = options.runBuild({ command: { argv: ["sleep", "30"] }, cwd: "." }, () => undefined, controller.signal);
    controller.abort(new Error("cancelled by test"));
    await expect(build).rejects.toThrow("cancelled by test");
  }, 10_000);
});

describe("process supervisor", () => {
  test("starts the single build-and-serve command and reaches ready", async () => {
    const fixture = setup();
    await fixture.supervisor.start("metadata");
    expect(fixture.builds).toEqual(["build metadata"]);
    expect(fixture.process.inputs).toHaveLength(1);
    expect(commandLabel(fixture.process.inputs[0]!.command.command)).toBe("serve metadata");
  });

  test("explicit restart stops then builds and serves a new generation", async () => {
    const fixture = setup();
    await fixture.supervisor.start("metadata");
    await fixture.supervisor.restart("metadata");
    expect(fixture.process.inputs).toHaveLength(2);
    expect(fixture.process.signals).toEqual([{ pgid: 100, signal: "SIGTERM" }]);
    expect(fixture.host.states.get("metadata")).toMatchObject({ actualState: "ready", generation: 2 });
    expect(fixture.builds).toEqual(["build metadata", "build metadata"]);
  });

  test("stops only owned processes", async () => {
    const fixture = setup();
    await fixture.supervisor.start("metadata");
    fixture.process.records.get(100)!.startIdentity = "reused-pid";
    await fixture.supervisor.stop("metadata");
    expect(fixture.process.signals).toEqual([]);
    expect(fixture.host.states.get("metadata")?.actualState).toBe("orphaned");
  });

  test("does not terminate a process whose command fingerprint changed", async () => {
    const fixture = setup();
    await fixture.supervisor.start("metadata");
    fixture.process.records.get(100)!.commandFingerprint = "different-command";
    await fixture.supervisor.stop("metadata");
    expect(fixture.process.signals).toEqual([]);
    expect(fixture.host.states.get("metadata")).toMatchObject({ actualState: "orphaned", readiness: "failed" });
  });

  test("settles a dead managed identity as stopped without signaling it", async () => {
    const fixture = setup();
    await fixture.supervisor.start("metadata");
    fixture.process.records.get(100)!.alive = false;
    await fixture.supervisor.stop("metadata");
    expect(fixture.process.signals).toEqual([]);
    expect(fixture.host.states.get("metadata")).toMatchObject({ actualState: "stopped", desiredState: "stopped", readiness: "unknown" });
  });

  test("cancels a queued-start service without treating its missing identity as orphaned", async () => {
    const fixture = setup();
    const timestamp = "2026-09-08T00:00:00.000Z";
    fixture.host.states.set("metadata", { serviceId: "metadata", desiredState: "running", actualState: "queued-start", readiness: "unknown", generation: 0, createdAt: timestamp, updatedAt: timestamp, currentOperationId: "start-1" });

    await fixture.supervisor.stop("metadata", "stop-1");

    expect(fixture.process.signals).toEqual([]);
    expect(fixture.host.states.get("metadata")).toMatchObject({ actualState: "stopped", desiredState: "stopped", readiness: "unknown", currentOperationId: "stop-1" });
  });

  test("persists the observed exec runtime fingerprint for later ownership checks", async () => {
    const process = new FakeProcess();
    const runtimeFingerprint = normalizeObservedCommandFingerprint("/Library/Java/JavaVirtualMachines/temurin-18.jdk/Contents/Home/bin/java -cp portal.lsession.jar portal.lsession.Launcher");
    const fixture = setup({
      process: {
        spawn: async (input) => {
          const app = await process.spawn(input);
          if ("pid" in app) process.records.get(app.pid)!.commandFingerprint = runtimeFingerprint;
          return app;
        },
        inspect: process.inspect.bind(process),
        signalGroup: process.signalGroup.bind(process),
      },
    });

    await fixture.supervisor.start("metadata");

    expect(fixture.host.states.get("metadata")).toMatchObject({ actualState: "ready", identity: { commandFingerprint: runtimeFingerprint } });
    await fixture.supervisor.stop("metadata");
    expect(process.signals).toEqual([{ pgid: 100, signal: "SIGTERM" }]);
  });

  test("fails instead of claiming ready when the readiness deadline expires", async () => {
    const fixture = setup({ readinessTimeoutMs: 0, probes: { tcp: async () => false, http: async () => false, container: async () => false, tailnet: async () => false, portInUse: async () => false } });
    await expect(fixture.supervisor.start("metadata")).rejects.toThrow("Readiness timed out");
    expect(fixture.host.states.get("metadata")).toMatchObject({ actualState: "failed", readiness: "failed", error: "Readiness timed out" });
  });

  test("does not spawn into a TCP port already held by another process", async () => {
    const fixture = setup({ probes: { tcp: async () => true, http: async () => false, container: async () => false, tailnet: async () => false, portInUse: async () => true } });
    await expect(fixture.supervisor.start("metadata")).rejects.toThrow("externally owned");
    expect(fixture.process.inputs).toEqual([]);
    expect(fixture.host.states.get("metadata")).toMatchObject({ actualState: "externally-owned", readiness: "failed" });
  });

  test("reaps a verified persisted detached POSIX identity during shutdown even when externally owned", async () => {
    const fixture = setup();
    await fixture.supervisor.start("metadata");
    const current = fixture.host.states.get("metadata")!;
    await fixture.host.setServiceState({ ...current, actualState: "externally-owned", readiness: "failed" });

    await fixture.supervisor.shutdown();

    expect(fixture.process.signals).toEqual([{ pgid: 100, signal: "SIGTERM" }]);
    expect(fixture.process.records.get(100)?.alive).toBe(false);
  });

  test("does not reap a persisted POSIX identity after PID reuse during shutdown", async () => {
    const fixture = setup();
    await fixture.supervisor.start("metadata");
    const current = fixture.host.states.get("metadata")!;
    fixture.process.records.get(100)!.startIdentity = "reused-pid";
    await fixture.host.setServiceState({ ...current, actualState: "externally-owned", readiness: "failed" });

    await fixture.supervisor.shutdown();

    expect(fixture.process.signals).toEqual([]);
    expect(fixture.process.records.get(100)?.alive).toBe(true);
  });

  test("does not reap an externally-owned service without a persisted manager identity", async () => {
    const fixture = setup();
    const timestamp = new FakeClock().now();
    await fixture.host.setServiceState({ serviceId: "metadata", desiredState: "running", actualState: "externally-owned", readiness: "failed", generation: 3, createdAt: timestamp, updatedAt: timestamp, error: "Port 1166 is held by an unowned process" });

    await fixture.supervisor.shutdown();

    expect(fixture.process.signals).toEqual([]);
    expect(fixture.process.inspected).toEqual([]);
  });

  test("stops an active service once during shutdown", async () => {
    const fixture = setup();
    await fixture.supervisor.start("metadata");

    await fixture.supervisor.shutdown();

    expect(fixture.process.signals).toEqual([{ pgid: 100, signal: "SIGTERM" }]);
    expect(fixture.host.states.get("metadata")).toMatchObject({ actualState: "stopped", desiredState: "stopped" });
  });

  test("reclaims a fully matched persisted POSIX child without rebuilding, conflicting on its port, or spawning", async () => {
    const fixture = setup();
    await fixture.supervisor.start("metadata");
    const current = fixture.host.states.get("metadata")!;
    await fixture.host.setServiceState({ ...current, actualState: "externally-owned", readiness: "failed" });
    fixture.host.instanceId = "manager-after-restart";
    const recovered = new ProcessSupervisor(fixture.host, { ...fixture.options, probes: { tcp: async () => true, http: async () => false, container: async () => false, tailnet: async () => false, portInUse: async () => true } });

    await recovered.start("metadata");

    expect(fixture.builds).toEqual(["build metadata"]);
    expect(fixture.process.inputs).toHaveLength(1);
    expect(fixture.host.states.get("metadata")).toMatchObject({ actualState: "ready", readiness: "ready", generation: current.generation, identity: { managerInstanceId: "manager-after-restart", pid: 100, pgid: 100, startIdentity: "start-100", commandFingerprint: current.identity?.commandFingerprint } });
  });

  test("keeps a pid-reused persisted identity externally owned when its TCP port is in use", async () => {
    const fixture = setup();
    await fixture.supervisor.start("metadata");
    const current = fixture.host.states.get("metadata")!;
    fixture.process.records.get(100)!.startIdentity = "reused-pid";
    await fixture.host.setServiceState({ ...current, actualState: "externally-owned", readiness: "failed" });
    const recovered = new ProcessSupervisor(fixture.host, { ...fixture.options, probes: { tcp: async () => true, http: async () => false, container: async () => false, tailnet: async () => false, portInUse: async () => true } });

    await expect(recovered.start("metadata")).rejects.toThrow("externally owned");

    expect(fixture.process.signals).toEqual([]);
    expect(fixture.host.states.get("metadata")).toMatchObject({ actualState: "externally-owned", readiness: "failed" });
  });

  test("stops a recovered verified POSIX child only while its full persisted identity still matches", async () => {
    const fixture = setup();
    await fixture.supervisor.start("metadata");
    const current = fixture.host.states.get("metadata")!;
    await fixture.host.setServiceState({ ...current, actualState: "externally-owned", readiness: "failed" });
    fixture.host.instanceId = "manager-after-restart";
    const recovered = new ProcessSupervisor(fixture.host, { ...fixture.options, probes: { tcp: async () => true, http: async () => false, container: async () => false, tailnet: async () => false, portInUse: async () => true } });
    await recovered.start("metadata");

    await recovered.stop("metadata");

    expect(fixture.process.signals).toEqual([{ pgid: 100, signal: "SIGTERM" }]);
    expect(fixture.host.states.get("metadata")).toMatchObject({ actualState: "stopped", desiredState: "stopped" });
  });

  test("reconcile marks a vanished managed child failed", async () => {
    const fixture = setup();
    await fixture.supervisor.start("metadata");
    fixture.process.records.get(100)!.alive = false;
    await fixture.supervisor.reconcile();
    expect(fixture.host.states.get("metadata")).toMatchObject({ actualState: "failed", readiness: "failed", error: "Managed process is no longer alive" });
  });

  test("does not spawn or start readiness when the build fails", async () => {
    const fixture = setup({
      runBuild: async () => {
        throw new Error("compile failed");
      },
    });
    await expect(fixture.supervisor.start("metadata")).rejects.toThrow("compile failed");
    expect(fixture.process.inputs).toEqual([]);
    expect(fixture.host.states.get("metadata")).toMatchObject({ actualState: "failed", readiness: "failed", error: "Build failed" });
  });

  test("serializes concurrent builds sharing a serializationKey and skips a cancelled queued build", async () => {
    const buildProfile = (label: string) => ({ command: shellCommand(label), serializationKey: "shared-toolchain" });
    const threeServices: ServiceCatalog = {
      ...catalog,
      services: [
        { id: "metadata", profiles: { run: { commandStatus: "verified", command: shellCommand("serve metadata"), readiness: { kind: "tcp", port: 1166 } }, build: buildProfile("build metadata") } },
        { id: "user", profiles: { run: { commandStatus: "verified", command: shellCommand("serve user"), readiness: { kind: "tcp", port: 1167 } }, build: buildProfile("build user") } },
        { id: "filestore", profiles: { run: { commandStatus: "verified", command: shellCommand("serve filestore"), readiness: { kind: "tcp", port: 1168 } }, build: buildProfile("build filestore") } },
      ],
    };
    const firstBuildStarted = Promise.withResolvers<void>();
    const secondBuildStarted = Promise.withResolvers<void>();
    const releaseFirstBuild = Promise.withResolvers<void>();
    const releaseSecondBuild = Promise.withResolvers<void>();
    const rawBuilds: string[] = [];
    let activeBuilds = 0;
    let maximumActiveBuilds = 0;
    const fixture = setup(
      {
        runBuild: async (command, _onOutput, signal) => {
          const label = commandLabel(command.command);
          rawBuilds.push(label);
          maximumActiveBuilds = Math.max(maximumActiveBuilds, ++activeBuilds);
          try {
            const release = label === "build metadata" ? releaseFirstBuild : releaseSecondBuild;
            if (label === "build metadata") firstBuildStarted.resolve();
            if (label === "build user") secondBuildStarted.resolve();
            await new Promise<void>((resolve, reject) => {
              release.promise.then(resolve);
              signal.addEventListener("abort", () => reject(signal.reason), { once: true });
            });
          } finally {
            activeBuilds--;
          }
        },
      },
      threeServices,
    );

    const first = fixture.supervisor.start("metadata");
    await firstBuildStarted.promise;
    const second = fixture.supervisor.start("user");
    releaseFirstBuild.resolve();
    await secondBuildStarted.promise;
    const cancelled = fixture.supervisor.start("filestore");
    await Promise.resolve();
    await Promise.resolve();
    await Promise.resolve();
    const stopping = fixture.supervisor.stop("filestore");
    releaseSecondBuild.resolve();

    await Promise.all([first, second, cancelled, stopping]);
    expect(rawBuilds).toEqual(["build metadata", "build user"]);
    expect(maximumActiveBuilds).toBe(1);
  });

  test("aborts an in-flight build before a queued restart can proceed", async () => {
    const buildStarted = Promise.withResolvers<void>();
    let attempts = 0;
    const fixture = setup({
      runBuild: async (_command, _onOutput, signal) => {
        if (++attempts > 1) return;
        await new Promise<void>((_resolve, reject) => {
          buildStarted.resolve();
          signal.addEventListener("abort", () => reject(signal.reason), { once: true });
        });
      },
    });
    const starting = fixture.supervisor.start("metadata");
    await buildStarted.promise;
    const restarting = fixture.supervisor.restart("metadata");
    await expect(starting).resolves.toBeUndefined();
    await restarting;
    expect(fixture.process.inputs).toHaveLength(1);
  });

  test("stops a preparing service cleanly after cancelling its build", async () => {
    const buildStarted = Promise.withResolvers<void>();
    const fixture = setup({
      runBuild: async (_command, _onOutput, signal) =>
        await new Promise<void>((_resolve, reject) => {
          buildStarted.resolve();
          signal.addEventListener("abort", () => reject(signal.reason), { once: true });
        }),
    });
    const starting = fixture.supervisor.start("metadata");
    await buildStarted.promise;
    await fixture.supervisor.stop("metadata");
    await expect(starting).resolves.toBeUndefined();
    expect(fixture.host.states.get("metadata")).toMatchObject({ actualState: "stopped", desiredState: "stopped", readiness: "unknown" });
    expect(fixture.process.inputs).toEqual([]);
  });

  test("serializes concurrent Docker Compose starts before container readiness", async () => {
    const dockerCatalog: ServiceCatalog = {
      startFailurePolicy: "stop-on-first-failure-keep-started",
      services: [
        { id: "mongo", profiles: { run: { commandStatus: "verified", command: shellCommand("docker compose up -d mongo", { containerName: "mongo" }), readiness: { kind: "container" } } } },
        { id: "redis", profiles: { run: { commandStatus: "verified", command: shellCommand("docker compose up -d redis", { containerName: "redis" }), readiness: { kind: "container" } } } },
      ],
      groups: { infrastructure: ["mongo", "redis"] },
    };
    const firstSpawned = Promise.withResolvers<void>();
    const releaseFirst = Promise.withResolvers<void>();
    const started: string[] = [];
    let activeSpawns = 0;
    let maximumActiveSpawns = 0;
    const records = new Map<string, Extract<ObservedProcess, { containerId: string }>>();
    const fixture = setup(
      {
        process: {
          spawn: async (input) => {
            const containerName = input.command.containerName!;
            started.push(containerName);
            maximumActiveSpawns = Math.max(maximumActiveSpawns, ++activeSpawns);
            if (containerName === "mongo") {
              firstSpawned.resolve();
              await releaseFirst.promise;
            }
            activeSpawns--;
            const record = { containerName, containerId: `container-${containerName}`, containerStartedAt: "2026-09-08T00:00:00.000Z", commandFingerprint: input.commandFingerprint, alive: true };
            records.set(containerName, record);
            return { ...record, exited: new Promise<number>(() => undefined) };
          },
          inspect: async (identity) => ("containerId" in identity ? records.get(identity.containerName) : undefined),
          signalGroup: async () => undefined,
        },
        probes: { tcp: async () => false, http: async () => false, container: async () => true, tailnet: async () => false, portInUse: async () => false },
      },
      dockerCatalog,
    );

    const mongo = fixture.supervisor.start("mongo");
    await firstSpawned.promise;
    const redis = fixture.supervisor.start("redis");
    await Promise.resolve();
    await Promise.resolve();
    expect(started).toEqual(["mongo"]);
    releaseFirst.resolve();
    await Promise.all([mongo, redis]);
    expect(started).toEqual(["mongo", "redis"]);
    expect(maximumActiveSpawns).toBe(1);
  });

  test("uses the configured catalog container identity for container readiness", async () => {
    const probes: string[] = [];
    const containerCatalog: ServiceCatalog = {
      startFailurePolicy: "stop-on-first-failure-keep-started",
      services: [{ id: "jitsi", profiles: { run: { commandStatus: "verified", command: shellCommand("docker compose up -d jitsi-prosody", { containerName: "configured-jitsi" }), readiness: { kind: "container" } } } }],
      groups: { one: ["jitsi"] },
    };
    const fixture = setup(
      {
        probes: {
          tcp: async () => false,
          http: async () => false,
          container: async (containerName) => {
            probes.push(containerName);
            return true;
          },
          tailnet: async () => false,
          portInUse: async () => false,
        },
      },
      containerCatalog,
    );
    await fixture.supervisor.start("jitsi");
    expect(probes).toEqual(["configured-jitsi"]);
  });

  test("fails closed when container readiness has no configured identity", async () => {
    let probes = 0;
    const containerCatalog: ServiceCatalog = {
      startFailurePolicy: "stop-on-first-failure-keep-started",
      services: [{ id: "jitsi", profiles: { run: { commandStatus: "verified", command: shellCommand("docker compose up -d jitsi-prosody"), readiness: { kind: "container" } } } }],
      groups: { one: ["jitsi"] },
    };
    const fixture = setup(
      { readinessTimeoutMs: 0, probes: { tcp: async () => false, http: async () => false, container: async () => { probes++; return true; }, tailnet: async () => false, portInUse: async () => false } },
      containerCatalog,
    );
    await expect(fixture.supervisor.start("jitsi")).rejects.toThrow("Readiness timed out");
    expect(probes).toBe(0);
  });

  test("reconciles a Docker container by persistent container identity without a host PID", async () => {
    const dockerCatalog: ServiceCatalog = {
      startFailurePolicy: "stop-on-first-failure-keep-started",
      services: [{ id: "jitsi", profiles: { run: { commandStatus: "verified", command: shellCommand("docker compose up -d jitsi-prosody", { containerName: "jitsi-prosody", dockerStopCommand: "docker compose stop jitsi-prosody" }), readiness: { kind: "container" } } } }],
      groups: { one: ["jitsi"] },
    };
    const process = new FakeProcess();
    const host = new FakeHost(dockerCatalog);
    host.states.set("jitsi", {
      serviceId: "jitsi",
      desiredState: "running",
      actualState: "starting",
      readiness: "not-ready",
      generation: 3,
      identity: { managerInstanceId: "manager-before-restart", serviceId: "jitsi", generation: 3, startedAt: "2026-09-08T00:00:00.000Z", commandFingerprint: "command", containerName: "jitsi-prosody", containerId: "sha256:container", containerStartedAt: "2026-09-08T00:00:00.000Z" },
      createdAt: "2026-09-08T00:00:00.000Z",
      updatedAt: "2026-09-08T00:00:00.000Z",
    });
    const supervisor = new ProcessSupervisor(host, { process, runBuild: async () => undefined, clock: new FakeClock(), probes: { tcp: async () => false, http: async () => false, container: async () => true, tailnet: async () => false } });
    await supervisor.reconcile();
    expect(process.inspected).toHaveLength(3);
    expect(process.inspected.every((identity) => "containerId" in identity && !("pid" in identity))).toBe(true);
    expect(host.states.get("jitsi")).toMatchObject({ actualState: "ready", readiness: "ready", identity: { managerInstanceId: "manager-a", containerId: "sha256:container", containerStartedAt: "2026-09-08T00:00:00.000Z" } });
  });

  test("persists failed when spawning throws", async () => {
    const process = new FakeProcess();
    const fixture = setup({ process: { spawn: async () => { throw new Error("Docker service container jitsi-prosody is not running after restart"); }, inspect: process.inspect.bind(process), signalGroup: process.signalGroup.bind(process) } });
    await expect(fixture.supervisor.start("metadata")).rejects.toThrow("Docker service container jitsi-prosody is not running after restart");
    expect(fixture.host.states.get("metadata")).toMatchObject({ actualState: "failed", readiness: "failed", error: "Docker service container jitsi-prosody is not running after restart" });
    expect(fixture.host.states.get("metadata")?.identity).toBeUndefined();
  });
});
