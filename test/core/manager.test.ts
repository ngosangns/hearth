import { mkdtemp, readdir, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { describe, expect, test } from "bun:test";

import { createFileIo } from "../../src/core/file-io";
import { AtomicStateStore, LocalServicesManager, ManagerEventStore, managerProtocolVersion, OperationScheduler } from "../../src/core/manager";
import type { ServiceCatalog } from "../../src/core/catalog";
import type { ManagedProcess, ObservedProcess, SupervisorOptions } from "../../src/core/supervisor";
import type { Operation, ProcessIdentity } from "../../src/core/state";

const graphCatalog = (): ServiceCatalog => ({
  startFailurePolicy: "stop-on-first-failure-keep-started",
  services: [
    { id: "nginx", profiles: { run: { command: { command: { argv: ["nginx"] }, cwd: "." }, commandStatus: "verified", readiness: { kind: "tcp", port: 80 } } } },
    { id: "mongo", profiles: { run: { command: { command: { argv: ["mongo"] }, cwd: "." }, commandStatus: "verified", readiness: { kind: "tcp", port: 27017 } } } },
    { id: "redis", profiles: { run: { command: { command: { argv: ["redis"] }, cwd: "." }, commandStatus: "verified", readiness: { kind: "tcp", port: 6379 } } } },
    { id: "kafka", profiles: { run: { command: { command: { argv: ["kafka"] }, cwd: "." }, commandStatus: "verified", readiness: { kind: "tcp", port: 9092 } } } },
    { id: "nats", profiles: { run: { command: { command: { argv: ["nats"] }, cwd: "." }, commandStatus: "verified", readiness: { kind: "tcp", port: 4222 } } } },
    { id: "metadata", profiles: { run: { command: { command: { argv: ["metadata"] }, cwd: "." }, commandStatus: "verified", readiness: { kind: "tcp", port: 1166 } } } },
    { id: "configurations", profiles: { run: { command: { command: { argv: ["configurations"] }, cwd: "." }, commandStatus: "verified", readiness: { kind: "tcp", port: 1144 } } } },
    { id: "ccs", profiles: { run: { command: { command: { argv: ["ccs"] }, cwd: "." }, commandStatus: "verified", readiness: { kind: "tcp", port: 8000 } } } },
    { id: "syncer", profiles: { run: { command: { command: { argv: ["syncer"] }, cwd: "." }, commandStatus: "verified", readiness: { kind: "tcp", port: 6060 } } } },
    { id: "portal", profiles: { run: { command: { command: { argv: ["portal"] }, cwd: "." }, commandStatus: "verified", readiness: { kind: "tcp", port: 19000 } } } },
  ],
  groups: { core: ["portal", "syncer"] },
});

const graphSupervisor = (started: string[], fail = new Set<string>(), onSpawn?: (serviceId: string) => Promise<void>, tcpProbe: (port: number) => Promise<boolean> = async () => true): SupervisorOptions => {
  let pid = 1;
  const records = new Map<number, Extract<ObservedProcess, { pid: number }>>();
  return {
    process: {
      spawn: async (input): Promise<ManagedProcess> => {
        started.push(input.serviceId);
        await onSpawn?.(input.serviceId);
        if (fail.has(input.serviceId)) throw new Error(`${input.serviceId} failed`);
        const record = { pid: pid++, pgid: pid - 1, startIdentity: String(pid - 1), commandFingerprint: input.commandFingerprint, alive: true };
        records.set(record.pid, record);
        return { ...record, exited: new Promise<number>(() => undefined) };
      },
      inspect: async (identity: ProcessIdentity): Promise<ObservedProcess | undefined> => ("containerId" in identity ? undefined : records.get(identity.pid)),
      signalGroup: async (pgid) => {
        const record = records.get(pgid);
        if (record) record.alive = false;
      },
    },
    runBuild: async () => undefined,
    probes: { tcp: tcpProbe, http: async () => false, container: async () => false, tailnet: async () => false, portInUse: async () => false },
  };
};

const waitForOperation = async (manager: LocalServicesManager, operation: Operation): Promise<Operation> => {
  const scheduled = manager.operations.get(operation.id)!;
  await manager.operations.wait(scheduled);
  return scheduled;
};

const startOperation = async (manager: LocalServicesManager, serviceId: string): Promise<Operation> => {
  const response = await fetch(`${manager.baseUrl}/v1/operations`, {
    method: "POST",
    headers: { authorization: `Bearer ${manager.bearerToken}`, "x-local-services-protocol": String(managerProtocolVersion), "content-type": "application/json" },
    body: JSON.stringify({ requestId: crypto.randomUUID(), serviceId, action: "start" }),
  });
  return ((await response.json()) as { operation: Operation }).operation;
};

const bulkStartOperation = async (manager: LocalServicesManager, targets: string[]): Promise<Response> =>
  fetch(`${manager.baseUrl}/v1/operations/bulk-start`, {
    method: "POST",
    headers: { authorization: `Bearer ${manager.bearerToken}`, "x-local-services-protocol": String(managerProtocolVersion), "content-type": "application/json" },
    body: JSON.stringify({ requestId: crypto.randomUUID(), targets }),
  });

const tempRuntime = async (prefix: string): Promise<string> => mkdtemp(join(tmpdir(), prefix));

describe("local services manager", () => {
  test("advertises a positive protocol version that every request must match", () => {
    expect(managerProtocolVersion).toBeGreaterThan(0);
    expect(Number.isInteger(managerProtocolVersion)).toBe(true);
  });

  test("rejects profile-bearing operation input", async () => {
    const runtime = await tempRuntime("local-services-strict-body-");
    const manager = await LocalServicesManager.bootstrap({ runtimeDirectory: runtime, catalog: graphCatalog(), supervisor: graphSupervisor([]) });
    try {
      const response = await fetch(`${manager.baseUrl}/v1/operations`, {
        method: "POST",
        headers: { authorization: `Bearer ${manager.bearerToken}`, "x-local-services-protocol": String(managerProtocolVersion), "content-type": "application/json" },
        body: JSON.stringify({ requestId: "request-1", serviceId: "metadata", action: "start", profile: "dev" }),
      });
      expect(response.status).toBe(400);
    } finally {
      await manager.shutdown("stop-services");
      await rm(runtime, { recursive: true, force: true });
    }
  });

  test("lists every catalog service as stopped on a fresh runtime directory", async () => {
    const runtime = await tempRuntime("local-services-catalog-");
    try {
      const manager = await LocalServicesManager.bootstrap({ runtimeDirectory: runtime, catalog: graphCatalog(), supervisor: graphSupervisor([]) });
      try {
        const response = await fetch(`${manager.baseUrl}/v1/services`, { headers: { authorization: `Bearer ${manager.bearerToken}`, "x-local-services-protocol": String(managerProtocolVersion) } });
        const body = (await response.json()) as { services: Array<{ serviceId: string; actualState: string; generation: number }> };
        expect(response.ok).toBe(true);
        expect(body.services.length).toBeGreaterThan(0);
        expect(body.services.every((service) => service.actualState === "stopped" && service.generation === 0)).toBe(true);
      } finally {
        await manager.shutdown("stop-services");
      }
    } finally {
      await rm(runtime, { recursive: true, force: true });
    }
  });

  test("bulk start brings up every requested target independently, with no start ordering between them", async () => {
    const runtime = await tempRuntime("local-services-graph-");
    const started: string[] = [];
    const manager = await LocalServicesManager.bootstrap({ runtimeDirectory: runtime, catalog: graphCatalog(), supervisor: graphSupervisor(started) });
    try {
      const targets = ["nginx", "mongo", "redis", "kafka", "nats", "metadata", "configurations", "ccs", "portal", "syncer"];
      const response = await bulkStartOperation(manager, targets);
      expect(response.status).toBe(202);
      const operation = await waitForOperation(manager, ((await response.json()) as { operation: Operation }).operation);
      expect(operation.status).toBe("succeeded");
      expect(started).toEqual(expect.arrayContaining(targets));
      expect(manager.serviceStates().filter((service) => targets.includes(service.serviceId)).every((service) => service.actualState === "ready")).toBe(true);
    } finally {
      await manager.shutdown("stop-services");
      await rm(runtime, { recursive: true, force: true });
    }
  });

  test("starts independent bulk targets concurrently, not waiting on each other's readiness", async () => {
    const runtime = await tempRuntime("local-services-independent-concurrency-");
    const started: string[] = [];
    const syncerReadinessProbe = Promise.withResolvers<void>();
    const releaseSyncerReadiness = Promise.withResolvers<void>();
    const portalStarted = Promise.withResolvers<void>();
    const manager = await LocalServicesManager.bootstrap({
      runtimeDirectory: runtime,
      catalog: graphCatalog(),
      supervisor: graphSupervisor(
        started,
        new Set(),
        async (serviceId) => {
          if (serviceId === "portal") portalStarted.resolve();
        },
        async (port) => {
          if (port !== 6060) return true;
          syncerReadinessProbe.resolve();
          await releaseSyncerReadiness.promise;
          return true;
        },
      ),
    });
    try {
      const response = await bulkStartOperation(manager, ["portal", "syncer"]);
      expect(response.status).toBe(202);
      const accepted = ((await response.json()) as { operation: Operation }).operation;
      await syncerReadinessProbe.promise;
      await portalStarted.promise;
      expect(started).toContain("portal");
      expect(started).toContain("syncer");
      const syncer = manager.serviceStates().find((service) => service.serviceId === "syncer");
      expect(syncer).toMatchObject({ actualState: "running-unready", readiness: "not-ready" });
      releaseSyncerReadiness.resolve();
      const operation = await waitForOperation(manager, accepted);
      expect(operation.status).toBe("succeeded");
    } finally {
      releaseSyncerReadiness.resolve();
      await manager.shutdown("stop-services");
      await rm(runtime, { recursive: true, force: true });
    }
  });

  test("reports services waiting behind a queued bulk start as queued-start", async () => {
    const runtime = await tempRuntime("local-services-queued-start-");
    const readiness = Promise.withResolvers<void>();
    const manager = await LocalServicesManager.bootstrap({
      runtimeDirectory: runtime,
      catalog: graphCatalog(),
      supervisor: graphSupervisor([], new Set(), undefined, async (port) => {
        if (port !== 8000) return true;
        await readiness.promise;
        return true;
      }),
    });
    try {
      const first = await bulkStartOperation(manager, ["portal"]);
      expect(first.status).toBe(202);
      const second = await bulkStartOperation(manager, ["syncer"]);
      expect(second.status).toBe(202);
      const accepted = ((await second.json()) as { operation: Operation }).operation;

      const response = await fetch(`${manager.baseUrl}/v1/services`, { headers: { authorization: `Bearer ${manager.bearerToken}`, "x-local-services-protocol": String(managerProtocolVersion) } });
      const services = ((await response.json()) as { services: Array<{ serviceId: string; actualState: string; currentOperationId?: string }> }).services;
      expect(services.find((service) => service.serviceId === "syncer")).toMatchObject({ actualState: "queued-start", currentOperationId: accepted.id });
    } finally {
      readiness.resolve();
      await manager.shutdown("stop-services");
      await rm(runtime, { recursive: true, force: true });
    }
  });

  test("traces the failure when a single-service start fails", async () => {
    const runtime = await tempRuntime("local-services-single-failure-");
    const started: string[] = [];
    const manager = await LocalServicesManager.bootstrap({ runtimeDirectory: runtime, catalog: graphCatalog(), supervisor: graphSupervisor(started, new Set(["ccs"])) });
    try {
      const accepted = await startOperation(manager, "ccs");
      const operation = await waitForOperation(manager, accepted);

      expect(operation.status).toBe("failed");
      expect(started).toEqual(["ccs"]);
      expect(operation.trace.map((entry) => entry.message)).toEqual(expect.arrayContaining(["Failed: ccs (ccs failed)"]));
      expect(manager.serviceStates().find((service) => service.serviceId === "ccs")).toMatchObject({ actualState: "failed", desiredState: "running" });
    } finally {
      await manager.shutdown("stop-services");
      await rm(runtime, { recursive: true, force: true });
    }
  });

  test("validates strict bulk start requests", async () => {
    const runtime = await tempRuntime("local-services-bulk-request-");
    const manager = await LocalServicesManager.bootstrap({ runtimeDirectory: runtime, catalog: graphCatalog(), supervisor: graphSupervisor([]) });
    try {
      const response = await fetch(`${manager.baseUrl}/v1/operations/bulk-start`, {
        method: "POST",
        headers: { authorization: `Bearer ${manager.bearerToken}`, "x-local-services-protocol": String(managerProtocolVersion), "content-type": "application/json" },
        body: JSON.stringify({ requestId: "request-1", targets: ["portal", "portal"] }),
      });
      expect(response.status).toBe(400);
      expect(await response.json()).toEqual({ error: { code: "invalid_targets", message: "targets must be a non-empty set of catalog services" } });
    } finally {
      await manager.shutdown("stop-services");
      await rm(runtime, { recursive: true, force: true });
    }
  });

  test("looks up bulk operations through the HTTP route and returns 404 for unknown IDs", async () => {
    const runtime = await tempRuntime("local-services-operation-lookup-");
    const manager = await LocalServicesManager.bootstrap({ runtimeDirectory: runtime, catalog: graphCatalog(), supervisor: graphSupervisor([]) });
    const headers = { authorization: `Bearer ${manager.bearerToken}`, "x-local-services-protocol": String(managerProtocolVersion) };
    try {
      const acceptedResponse = await bulkStartOperation(manager, ["portal"]);
      const accepted = ((await acceptedResponse.json()) as { operation: Operation }).operation;
      const operationResponse = await fetch(`${manager.baseUrl}/v1/operations/${accepted.id}`, { headers });
      const unknownResponse = await fetch(`${manager.baseUrl}/v1/operations/unknown-operation`, { headers });

      expect(acceptedResponse.status).toBe(202);
      expect(operationResponse.status).toBe(200);
      expect(((await operationResponse.json()) as { operation: Operation }).operation).toMatchObject({ id: accepted.id, kind: "bulk-start" });
      expect(unknownResponse.status).toBe(404);
      expect(await unknownResponse.json()).toEqual({ error: { code: "operation_not_found", message: "Operation not found" } });
    } finally {
      await manager.shutdown("stop-services");
      await rm(runtime, { recursive: true, force: true });
    }
  });

  test("reuses an identical bulk request and rejects a reused requestId with different targets", async () => {
    const runtime = await tempRuntime("local-services-bulk-request-id-");
    const manager = await LocalServicesManager.bootstrap({ runtimeDirectory: runtime, catalog: graphCatalog(), supervisor: graphSupervisor([]) });
    const request = (targets: string[]): Promise<Response> =>
      fetch(`${manager.baseUrl}/v1/operations/bulk-start`, {
        method: "POST",
        headers: { authorization: `Bearer ${manager.bearerToken}`, "x-local-services-protocol": String(managerProtocolVersion), "content-type": "application/json" },
        body: JSON.stringify({ requestId: "bulk-request-1", targets }),
      });
    try {
      const firstResponse = await request(["portal"]);
      const first = ((await firstResponse.json()) as { operation: Operation }).operation;
      const replayResponse = await request(["portal"]);
      const replay = ((await replayResponse.json()) as { operation: Operation }).operation;
      const conflictResponse = await request(["metadata"]);

      expect(firstResponse.status).toBe(202);
      expect(replayResponse.status).toBe(202);
      expect(replay.id).toBe(first.id);
      expect(conflictResponse.status).toBe(409);
      expect(await conflictResponse.json()).toEqual({ error: { code: "request_id_conflict", message: "requestId is already used by a different operation" } });
    } finally {
      await manager.shutdown("stop-services");
      await rm(runtime, { recursive: true, force: true });
    }
  });

  test("clears queued-start services when the manager rejects a queued bulk start", async () => {
    const runtime = await tempRuntime("local-services-queued-rejected-");
    const manager = await LocalServicesManager.bootstrap({ runtimeDirectory: runtime, catalog: graphCatalog(), supervisor: graphSupervisor([]) });
    const running = Promise.withResolvers<void>();
    const release = Promise.withResolvers<void>();
    try {
      const blocker = manager.operations.schedule({ requestId: "blocker", kind: "manager-shutdown" }, async () => {
        running.resolve();
        await release.promise;
      });
      await running.promise;
      const response = await bulkStartOperation(manager, ["syncer"]);
      const accepted = ((await response.json()) as { operation: Operation }).operation;
      expect(manager.serviceStates().find((service) => service.serviceId === "syncer")).toMatchObject({ actualState: "queued-start", currentOperationId: accepted.id });

      manager.operations.closeMutations();
      release.resolve();
      const operation = await waitForOperation(manager, accepted);

      expect(operation).toMatchObject({ status: "failed", error: { code: "manager_closing" } });
      expect(manager.serviceStates().find((service) => service.serviceId === "syncer")).toMatchObject({ actualState: "stopped", desiredState: "stopped" });
      await manager.operations.wait(blocker);
    } finally {
      release.resolve();
      await manager.shutdown("stop-services");
      await rm(runtime, { recursive: true, force: true });
    }
  });

  test("allows shutdown without stop-services while a queued-start service is awaiting execution", async () => {
    const runtime = await tempRuntime("local-services-queued-shutdown-");
    const manager = await LocalServicesManager.bootstrap({ runtimeDirectory: runtime, catalog: graphCatalog(), supervisor: graphSupervisor([]) });
    try {
      const timestamp = new Date().toISOString();
      await manager.setServiceState({ serviceId: "syncer", desiredState: "running", actualState: "queued-start", readiness: "unknown", generation: 0, createdAt: timestamp, updatedAt: timestamp, currentOperationId: "start-1" });
      const response = await fetch(`${manager.baseUrl}/v1/manager/shutdown`, {
        method: "POST",
        headers: { authorization: `Bearer ${manager.bearerToken}`, "x-local-services-protocol": String(managerProtocolVersion), "content-type": "application/json" },
        body: JSON.stringify({ requestId: "shutdown-1", mode: "refuse-if-active" }),
      });

      expect(response.status).toBe(202);
      await expect(manager.shutdownCompletion).resolves.toBeUndefined();
    } finally {
      await manager.shutdown("stop-services");
      await rm(runtime, { recursive: true, force: true });
    }
  });

  test("rejects a queued bulk start when scheduler mutations close", async () => {
    const scheduler = new OperationScheduler(new ManagerEventStore());
    const started = Promise.withResolvers<void>();
    const gate = Promise.withResolvers<void>();
    let queuedExecuteCalled = false;
    const running = scheduler.schedule({ requestId: "bulk-running", kind: "bulk-start", targetServiceIds: ["portal"] }, async () => {
      started.resolve();
      await gate.promise;
    });
    await started.promise;
    const queued = scheduler.schedule({ requestId: "bulk-queued", kind: "bulk-start", targetServiceIds: ["metadata"] }, async () => {
      queuedExecuteCalled = true;
    });

    scheduler.closeMutations();
    gate.resolve();
    await scheduler.wait(queued);

    expect(running.status).toBe("succeeded");
    expect(queued.status).toBe("failed");
    expect(queued.error).toEqual({ code: "manager_closing", message: "Manager is shutting down" });
    expect(queuedExecuteCalled).toBe(false);
  });

  test("starts independent bulk targets even when one of them fails", async () => {
    const runtime = await tempRuntime("local-services-bulk-failure-");
    const started: string[] = [];
    const manager = await LocalServicesManager.bootstrap({ runtimeDirectory: runtime, catalog: graphCatalog(), supervisor: graphSupervisor(started, new Set(["metadata"])) });
    try {
      const response = await bulkStartOperation(manager, ["metadata", "syncer"]);
      const accepted = ((await response.json()) as { operation: Operation }).operation;
      const operation = await waitForOperation(manager, accepted);
      expect(operation.status).toBe("failed");
      expect(started).toEqual(expect.arrayContaining(["metadata", "syncer"]));
      expect(operation.trace.map((entry) => entry.message)).toContain("Failed: metadata (metadata failed)");
      expect(manager.serviceStates().find((service) => service.serviceId === "syncer")).toMatchObject({ actualState: "ready", desiredState: "running" });
      expect(manager.serviceStates().find((service) => service.serviceId === "metadata")).toMatchObject({ actualState: "failed", desiredState: "running" });
    } finally {
      await manager.shutdown("stop-services");
      await rm(runtime, { recursive: true, force: true });
    }
  });

  test("quarantines a state.json with the wrong version instead of trusting its contents", async () => {
    const runtime = await tempRuntime("local-services-bad-version-");
    try {
      await writeFile(join(runtime, "state.json"), JSON.stringify({ version: 999, services: { metadata: { serviceId: "metadata", profile: "dev" } } }), { mode: 0o600 });
      const state = await new AtomicStateStore(createFileIo(true), runtime).load();
      expect(state).toEqual({ version: 1, services: {} });
      expect((await readdir(runtime)).some((name) => name.startsWith("state.json.corrupt-"))).toBe(true);
    } finally {
      await rm(runtime, { recursive: true, force: true });
    }
  });

  test("uses the lifecycle generation when reading a rotated legacy log stream", async () => {
    const runtime = await tempRuntime("local-services-log-generation-");
    try {
      const portalOnlyCatalog: ServiceCatalog = {
        startFailurePolicy: "stop-on-first-failure-keep-started",
        groups: {},
        services: [{ id: "portal", profiles: { run: { commandStatus: "verified", command: { command: { argv: ["portal"] }, cwd: "." }, readiness: { kind: "process" } } } }],
      };
      const manager = await LocalServicesManager.bootstrap({ runtimeDirectory: runtime, catalog: portalOnlyCatalog, logTailBytes: 8, logMaxBytes: 8 });
      try {
        const timestamp = new Date().toISOString();
        await manager.setServiceState({ serviceId: "portal", desiredState: "running", actualState: "ready", readiness: "ready", generation: 8, createdAt: timestamp, updatedAt: timestamp });
        await manager.appendLog("portal", "legacy\n");
        await manager.appendLog("portal", "fresh\n");

        const headers = { authorization: `Bearer ${manager.bearerToken}`, "x-local-services-protocol": String(managerProtocolVersion) };
        const initialResponse = await fetch(`${manager.baseUrl}/v1/logs/portal?generation=2&cursor=0&limit=8`, { headers });
        const initial = (await initialResponse.json()) as { serviceId: string; generation: number; cursor: number; nextCursor: number; data: string; reset: boolean; truncated: boolean };
        expect(initialResponse.ok).toBe(true);
        expect(initial).toEqual({ serviceId: "portal", generation: 8, cursor: 0, nextCursor: 6, data: "fresh\n", reset: true, truncated: false });

        const followUpResponse = await fetch(`${manager.baseUrl}/v1/logs/portal?generation=${initial.generation}&cursor=${initial.nextCursor}&limit=8`, { headers });
        const followUp = (await followUpResponse.json()) as { serviceId: string; generation: number; cursor: number; nextCursor: number; data: string; reset: boolean; truncated: boolean };
        expect(followUpResponse.ok).toBe(true);
        expect(followUp).toEqual({ serviceId: "portal", generation: 8, cursor: 6, nextCursor: 6, data: "", reset: false, truncated: true });
      } finally {
        await manager.shutdown("stop-services");
      }
    } finally {
      await rm(runtime, { recursive: true, force: true });
    }
  });

  test("accepts echoing back generation 0 for a service that has never started", async () => {
    const runtime = await tempRuntime("local-services-log-generation-zero-");
    try {
      const portalOnlyCatalog: ServiceCatalog = {
        startFailurePolicy: "stop-on-first-failure-keep-started",
        groups: {},
        services: [{ id: "portal", profiles: { run: { commandStatus: "verified", command: { command: { argv: ["portal"] }, cwd: "." }, readiness: { kind: "process" } } } }],
      };
      const manager = await LocalServicesManager.bootstrap({ runtimeDirectory: runtime, catalog: portalOnlyCatalog, logTailBytes: 8, logMaxBytes: 8 });
      try {
        const headers = { authorization: `Bearer ${manager.bearerToken}`, "x-local-services-protocol": String(managerProtocolVersion) };
        // A service that has never had a lifecycle transition has no entry in `state.services`, so
        // `lifecycleGeneration` legitimately returns 0 and the response echoes `generation: 0` — a
        // client (the TUI) that stores this and sends it back on the next poll must not be rejected.
        const initialResponse = await fetch(`${manager.baseUrl}/v1/logs/portal?limit=8`, { headers });
        const initial = (await initialResponse.json()) as { generation: number };
        expect(initialResponse.ok).toBe(true);
        expect(initial.generation).toBe(0);

        const followUpResponse = await fetch(`${manager.baseUrl}/v1/logs/portal?generation=0&cursor=0&limit=8`, { headers });
        expect(followUpResponse.ok).toBe(true);
        expect(((await followUpResponse.json()) as { generation: number }).generation).toBe(0);
      } finally {
        await manager.shutdown("stop-services");
      }
    } finally {
      await rm(runtime, { recursive: true, force: true });
    }
  });
});
