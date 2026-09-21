import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterEach, describe, expect, test } from "bun:test";

import { LocalServicesManager, managerProtocolVersion } from "../../src/core/manager";
import type { ServiceCatalog } from "../../src/core/catalog";
import type { ManagedProcess, ObservedProcess, SupervisorOptions } from "../../src/core/supervisor";
import type { ProcessIdentity } from "../../src/core/state";

const scratchDirs: string[] = [];
afterEach(async () => {
  await Promise.all(scratchDirs.splice(0).map((dir) => rm(dir, { recursive: true, force: true })));
});
async function tempRuntime(): Promise<string> {
  const dir = await mkdtemp(join(tmpdir(), "local-services-reload-"));
  scratchDirs.push(dir);
  return dir;
}

const headers = (manager: LocalServicesManager) => ({ authorization: `Bearer ${manager.bearerToken}`, "x-local-services-protocol": String(managerProtocolVersion), "content-type": "application/json" });

/** A supervisor whose spawned processes run forever (until signalled) and whose tcp probe is always
 * ready — services reach `ready` quickly and stay there until explicitly stopped, so a test can put
 * a service in an "active" state before reloading the catalog out from under it. */
const foreverSupervisor = (): SupervisorOptions & { signalled: string[] } => {
  let pid = 1;
  const records = new Map<number, Extract<ObservedProcess, { pid: number }>>();
  const signalled: string[] = [];
  return {
    signalled,
    process: {
      spawn: async (input): Promise<ManagedProcess> => {
        const record = { pid: pid++, pgid: pid - 1, startIdentity: String(pid - 1), commandFingerprint: input.commandFingerprint, alive: true };
        records.set(record.pid, record);
        return { ...record, exited: new Promise<number>(() => undefined) };
      },
      inspect: async (identity: ProcessIdentity): Promise<ObservedProcess | undefined> => ("containerId" in identity ? undefined : records.get(identity.pid)),
      signalGroup: async (pgid) => {
        const record = records.get(pgid);
        if (record) {
          record.alive = false;
          signalled.push(String(pgid));
        }
      },
    },
    runBuild: async () => undefined,
    probes: { tcp: async () => true, http: async () => false, container: async () => false, tailnet: async () => false, portInUse: async () => false },
  };
};

const service = (id: string, port: number, overrides: Partial<ServiceCatalog["services"][number]> = {}): ServiceCatalog["services"][number] => ({
  id,
  profiles: { run: { commandStatus: "verified", command: { command: { argv: [id] }, cwd: "." }, readiness: { kind: "tcp", port } } },
  ...overrides,
});

describe("GET /v1/catalog", () => {
  test("returns the catalog the manager was bootstrapped with", async () => {
    const runtime = await tempRuntime();
    const catalog: ServiceCatalog = { startFailurePolicy: "stop-on-first-failure-keep-started", groups: { all: ["a"] }, services: [service("a", 9001)] };
    const manager = await LocalServicesManager.bootstrap({ runtimeDirectory: runtime, catalog, supervisor: foreverSupervisor() });
    try {
      const response = await fetch(`${manager.baseUrl}/v1/catalog`, { headers: headers(manager) });
      expect(response.ok).toBe(true);
      expect(((await response.json()) as { catalog: ServiceCatalog }).catalog).toEqual(catalog);
    } finally {
      await manager.shutdown("stop-services");
    }
  });
});

describe("POST /v1/manager/reload", () => {
  test("swaps in a valid catalog and it is reflected by /v1/catalog", async () => {
    const runtime = await tempRuntime();
    const catalog: ServiceCatalog = { startFailurePolicy: "stop-on-first-failure-keep-started", groups: {}, services: [service("a", 9001)] };
    const manager = await LocalServicesManager.bootstrap({ runtimeDirectory: runtime, catalog, supervisor: foreverSupervisor() });
    try {
      const nextCatalog: ServiceCatalog = { startFailurePolicy: "stop-on-first-failure-keep-started", groups: {}, services: [service("a", 9001), service("b", 9002)] };
      const response = await fetch(`${manager.baseUrl}/v1/manager/reload`, { method: "POST", headers: headers(manager), body: JSON.stringify({ requestId: "r1", catalog: nextCatalog }) });
      expect(response.status).toBe(200);
      expect(await response.json()).toEqual({ stopped: [], changed: [] });
      const catalogResponse = await fetch(`${manager.baseUrl}/v1/catalog`, { headers: headers(manager) });
      expect(((await catalogResponse.json()) as { catalog: ServiceCatalog }).catalog.services.map((s) => s.id)).toEqual(["a", "b"]);
    } finally {
      await manager.shutdown("stop-services");
    }
  });

  test("stops an active daemon-owned service that was removed from the catalog, using the old catalog to do it", async () => {
    const runtime = await tempRuntime();
    const catalog: ServiceCatalog = { startFailurePolicy: "stop-on-first-failure-keep-started", groups: {}, services: [service("a", 9001), service("b", 9002)] };
    const supervisorOptions = foreverSupervisor();
    const manager = await LocalServicesManager.bootstrap({ runtimeDirectory: runtime, catalog, supervisor: supervisorOptions });
    try {
      await manager.supervisor.start("b");
      const beforeStates = manager.serviceStates();
      expect(beforeStates.find((s) => s.serviceId === "b")?.actualState).toBe("ready");

      const nextCatalog: ServiceCatalog = { startFailurePolicy: "stop-on-first-failure-keep-started", groups: {}, services: [service("a", 9001)] };
      const response = await fetch(`${manager.baseUrl}/v1/manager/reload`, { method: "POST", headers: headers(manager), body: JSON.stringify({ requestId: "r1", catalog: nextCatalog }) });
      expect(response.status).toBe(200);
      expect(await response.json()).toEqual({ stopped: ["b"], changed: [] });
      expect(supervisorOptions.signalled.length).toBeGreaterThan(0); // the process tree was actually signalled
      const afterCatalog = await fetch(`${manager.baseUrl}/v1/catalog`, { headers: headers(manager) });
      expect(((await afterCatalog.json()) as { catalog: ServiceCatalog }).catalog.services.map((s) => s.id)).toEqual(["a"]);
    } finally {
      await manager.shutdown("stop-services");
    }
  });

  test("never touches an external-owned service that was removed while active", async () => {
    const runtime = await tempRuntime();
    const catalog: ServiceCatalog = { startFailurePolicy: "stop-on-first-failure-keep-started", groups: {}, services: [service("a", 9001), service("ext", 9002, { ownership: "external" })] };
    const supervisorOptions = foreverSupervisor();
    const manager = await LocalServicesManager.bootstrap({ runtimeDirectory: runtime, catalog, supervisor: supervisorOptions });
    try {
      const timestamp = new Date().toISOString();
      await manager.setServiceState({ serviceId: "ext", desiredState: "running", actualState: "externally-owned", readiness: "ready", generation: 1, createdAt: timestamp, updatedAt: timestamp });

      const nextCatalog: ServiceCatalog = { startFailurePolicy: "stop-on-first-failure-keep-started", groups: {}, services: [service("a", 9001)] };
      const response = await fetch(`${manager.baseUrl}/v1/manager/reload`, { method: "POST", headers: headers(manager), body: JSON.stringify({ requestId: "r1", catalog: nextCatalog }) });
      expect(response.status).toBe(200);
      expect(await response.json()).toEqual({ stopped: [], changed: [] });
      expect(supervisorOptions.signalled).toEqual([]);
    } finally {
      await manager.shutdown("stop-services");
    }
  });

  test("leaves an active service running when only its definition changed, and reports it in `changed`", async () => {
    const runtime = await tempRuntime();
    const catalog: ServiceCatalog = { startFailurePolicy: "stop-on-first-failure-keep-started", groups: {}, services: [service("a", 9001)] };
    const supervisorOptions = foreverSupervisor();
    const manager = await LocalServicesManager.bootstrap({ runtimeDirectory: runtime, catalog, supervisor: supervisorOptions });
    try {
      await manager.supervisor.start("a");
      const nextCatalog: ServiceCatalog = {
        startFailurePolicy: "stop-on-first-failure-keep-started",
        groups: {},
        services: [{ id: "a", profiles: { run: { commandStatus: "verified", command: { command: { argv: ["a", "--new-flag"] }, cwd: "." }, readiness: { kind: "tcp", port: 9001 } } } }],
      };
      const response = await fetch(`${manager.baseUrl}/v1/manager/reload`, { method: "POST", headers: headers(manager), body: JSON.stringify({ requestId: "r1", catalog: nextCatalog }) });
      expect(response.status).toBe(200);
      expect(await response.json()).toEqual({ stopped: [], changed: ["a"] });
      expect(supervisorOptions.signalled).toEqual([]); // never restarted out from under the developer
      expect(manager.serviceStates().find((s) => s.serviceId === "a")?.actualState).toBe("ready");
    } finally {
      await manager.shutdown("stop-services");
    }
  });

  test("rejects an invalid catalog (duplicate service id) and keeps the previous catalog live", async () => {
    const runtime = await tempRuntime();
    const catalog: ServiceCatalog = { startFailurePolicy: "stop-on-first-failure-keep-started", groups: {}, services: [service("a", 9001)] };
    const manager = await LocalServicesManager.bootstrap({ runtimeDirectory: runtime, catalog, supervisor: foreverSupervisor() });
    try {
      const duplicateCatalog: ServiceCatalog = { startFailurePolicy: "stop-on-first-failure-keep-started", groups: {}, services: [service("a", 9001), service("a", 9002)] };
      const response = await fetch(`${manager.baseUrl}/v1/manager/reload`, { method: "POST", headers: headers(manager), body: JSON.stringify({ requestId: "r1", catalog: duplicateCatalog }) });
      expect(response.status).toBe(422);
      expect(((await response.json()) as { error: { code: string } }).error.code).toBe("invalid_catalog");
      const catalogResponse = await fetch(`${manager.baseUrl}/v1/catalog`, { headers: headers(manager) });
      expect(((await catalogResponse.json()) as { catalog: ServiceCatalog }).catalog).toEqual(catalog);
    } finally {
      await manager.shutdown("stop-services");
    }
  });

  test("rejects a structurally malformed body", async () => {
    const runtime = await tempRuntime();
    const catalog: ServiceCatalog = { startFailurePolicy: "stop-on-first-failure-keep-started", groups: {}, services: [service("a", 9001)] };
    const manager = await LocalServicesManager.bootstrap({ runtimeDirectory: runtime, catalog, supervisor: foreverSupervisor() });
    try {
      const response = await fetch(`${manager.baseUrl}/v1/manager/reload`, { method: "POST", headers: headers(manager), body: JSON.stringify({ requestId: "r1", catalog: { services: "not-an-array" } }) });
      expect(response.status).toBe(400);
    } finally {
      await manager.shutdown("stop-services");
    }
  });
});
