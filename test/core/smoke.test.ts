import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { LocalServicesManager, type ServiceCatalog } from "../../src/core";

let root: string;

beforeEach(async () => {
  root = await mkdtemp(join(tmpdir(), "lsvc-smoke-"));
});
afterEach(async () => {
  await rm(root, { recursive: true, force: true });
});

function catalog(): ServiceCatalog {
  return {
    startFailurePolicy: "stop-on-first-failure-keep-started",
    groups: { all: ["sleeper"] },
    services: [
      {
        id: "sleeper",
        profiles: {
          run: {
            commandStatus: "verified",
            command: { command: { argv: ["sleep", "30"] }, cwd: "." },
            readiness: { kind: "process" },
          },
        },
      },
    ],
  };
}

describe("LocalServicesManager smoke test", () => {
  test("bootstraps, starts a process service, and shuts it down", async () => {
    const manager = await LocalServicesManager.bootstrap({ root, catalog: catalog() });
    try {
      expect(manager.info.protocolVersion).toBeGreaterThan(0);
      await manager.supervisor.start("sleeper");
      const state = manager.serviceStates().find((s) => s.serviceId === "sleeper");
      expect(state?.actualState).toBe("running-unready");
      expect(state?.readinessKind).toBe("process");
      expect(state?.identity).toBeDefined();
    } finally {
      await manager.shutdown("stop-services");
    }
    const after = await manager.shutdownCompletion;
    expect(after).toBeUndefined();
  }, 15_000);

  test("HTTP API round-trips a start operation", async () => {
    const manager = await LocalServicesManager.bootstrap({ root, catalog: catalog() });
    try {
      const headers = { authorization: `Bearer ${manager.bearerToken}`, "x-local-services-protocol": String(manager.info.protocolVersion), "content-type": "application/json" };
      const response = await fetch(`${manager.baseUrl}/v1/operations`, { method: "POST", headers, body: JSON.stringify({ requestId: "r1", serviceId: "sleeper", action: "start" }) });
      expect(response.status).toBe(202);
      const { operation } = (await response.json()) as { operation: { id: string; status: string } };
      let current = operation;
      for (let i = 0; i < 50 && (current.status === "queued" || current.status === "running"); i++) {
        await Bun.sleep(100);
        const poll = await fetch(`${manager.baseUrl}/v1/operations/${current.id}`, { headers });
        current = ((await poll.json()) as { operation: typeof current }).operation;
      }
      expect(current.status).toBe("succeeded");
    } finally {
      await manager.shutdown("stop-services");
      await manager.shutdownCompletion;
    }
  }, 15_000);
});
