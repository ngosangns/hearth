import { access, mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { describe, expect, test } from "bun:test";

import type { ServiceCatalog } from "../../src/core/catalog";
import { DaemonLifecycle, terminateAfterManagerShutdown } from "../../src/core/daemon";
import { LocalServicesManager, managerProtocolVersion } from "../../src/core/manager";
import type { ObservedProcess, SupervisorOptions } from "../../src/core/supervisor";
import type { ProcessIdentity, ServiceLifecycleState } from "../../src/core/state";

const catalog: ServiceCatalog = {
  startFailurePolicy: "stop-on-first-failure-keep-started",
  services: [{ id: "metadata", profiles: { run: { commandStatus: "verified", command: { command: { argv: ["metadata"] }, cwd: "." }, readiness: { kind: "process" } } } }],
  groups: {},
};

const shutdownHeaders = (manager: LocalServicesManager): Record<string, string> => ({
  authorization: `Bearer ${manager.bearerToken}`,
  "content-type": "application/json",
  "x-local-services-protocol": String(managerProtocolVersion),
});

describe("daemon lifecycle", () => {
  test("terminates the source daemon once a remote manager shutdown ('stop-services', mode='stop-services') has reaped its child and released the lock", async () => {
    const runtimeDirectory = await mkdtemp(join(tmpdir(), "daemon-lifecycle-"));
    const process = { alive: true, signals: [] as Array<{ pgid: number; signal: string }> };
    const supervisor: SupervisorOptions = {
      process: {
        spawn: async () => {
          throw new Error("not used");
        },
        inspect: async (identity: ProcessIdentity): Promise<ObservedProcess | undefined> => ("containerId" in identity ? undefined : { pid: identity.pid, pgid: identity.pgid, startIdentity: identity.startIdentity, commandFingerprint: identity.commandFingerprint, alive: process.alive }),
        signalGroup: async (pgid, signal) => {
          process.signals.push({ pgid, signal });
          process.alive = false;
        },
      },
      runBuild: async () => undefined,
      probes: { tcp: async () => false, http: async () => false, container: async () => false, tailnet: async () => false },
      terminationGraceMs: 0,
    };
    const manager = await LocalServicesManager.bootstrap({ runtimeDirectory, catalog, supervisor });
    try {
      const timestamp = new Date().toISOString();
      const state: ServiceLifecycleState = {
        serviceId: "metadata",
        desiredState: "running",
        actualState: "ready",
        readiness: "ready",
        generation: 1,
        identity: { managerInstanceId: manager.instanceId, serviceId: "metadata", generation: 1, pid: 42, pgid: 42, startedAt: timestamp, startIdentity: "process-42", commandFingerprint: "metadata" },
        createdAt: timestamp,
        updatedAt: timestamp,
      };
      await manager.setServiceState(state);
      // Explicit stop-services mode here (unlike DaemonLifecycle's own SIGINT/SIGTERM default of
      // refuse-if-active) — this test wants the identity actually reaped, not left running.
      const lifecycle = new DaemonLifecycle(manager, "stop-services");
      const exitCodes: number[] = [];
      const terminate = terminateAfterManagerShutdown(lifecycle, (exitCode) => {
        exitCodes.push(exitCode);
      });
      const response = await fetch(`${manager.baseUrl}/v1/manager/shutdown`, { method: "POST", headers: shutdownHeaders(manager), body: JSON.stringify({ requestId: "shutdown-1", mode: "stop-services" }) });
      const firstSignal = lifecycle.shutdown();
      const secondSignal = lifecycle.shutdown();

      expect(response.status).toBe(202);
      expect(firstSignal).toBe(secondSignal);
      await terminate;
      expect(process.signals).toEqual([{ pgid: 42, signal: "SIGTERM" }]);
      expect(exitCodes).toEqual([0]);
      await expect(access(join(runtimeDirectory, "manager.lock"))).rejects.toThrow();
    } finally {
      await manager.shutdown("stop-services");
      await rm(runtimeDirectory, { recursive: true, force: true });
    }
  });

  test("terminates the daemon once when manager shutdown fails", async () => {
    const completion = Promise.withResolvers<void>();
    const lifecycle = new DaemonLifecycle({ shutdown: async () => undefined, shutdownCompletion: completion.promise } as Pick<LocalServicesManager, "shutdown" | "shutdownCompletion">);
    const exitCodes: number[] = [];
    const termination = terminateAfterManagerShutdown(lifecycle, (exitCode) => {
      exitCodes.push(exitCode);
    });

    completion.reject(new Error("lock release failed"));
    await termination;

    expect(exitCodes).toEqual([1]);
  });

  test("default signal mode ('refuse-if-active') leaves an already-running service alone so a daemon restart never kills in-flight work", async () => {
    const runtimeDirectory = await mkdtemp(join(tmpdir(), "daemon-lifecycle-refuse-"));
    const signals: Array<{ pgid: number; signal: string }> = [];
    const supervisor: SupervisorOptions = {
      process: {
        spawn: async () => {
          throw new Error("not used");
        },
        inspect: async (identity: ProcessIdentity): Promise<ObservedProcess | undefined> => ("containerId" in identity ? undefined : { pid: identity.pid, pgid: identity.pgid, startIdentity: identity.startIdentity, commandFingerprint: identity.commandFingerprint, alive: true }),
        signalGroup: async (pgid, signal) => {
          signals.push({ pgid, signal });
        },
      },
      runBuild: async () => undefined,
      probes: { tcp: async () => false, http: async () => false, container: async () => false, tailnet: async () => false },
    };
    const manager = await LocalServicesManager.bootstrap({ runtimeDirectory, catalog, supervisor });
    try {
      const timestamp = new Date().toISOString();
      await manager.setServiceState({
        serviceId: "metadata",
        desiredState: "running",
        actualState: "ready",
        readiness: "ready",
        generation: 1,
        identity: { managerInstanceId: manager.instanceId, serviceId: "metadata", generation: 1, pid: 42, pgid: 42, startedAt: timestamp, startIdentity: "process-42", commandFingerprint: "metadata" },
        createdAt: timestamp,
        updatedAt: timestamp,
      });
      const lifecycle = new DaemonLifecycle(manager);
      await lifecycle.shutdown();
      await lifecycle.waitForManagerShutdown();
      expect(signals).toEqual([]);
    } finally {
      await manager.shutdown("stop-services");
      await rm(runtimeDirectory, { recursive: true, force: true });
    }
  });
});
