import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { LocalServicesManager, type ServiceCatalog } from "../../src/core";

// `ownership: 'external'` has no equivalent test in either source repo — it's this package's own
// generalization of infra's docker/tailnet-task `syncExternalUnits` carve-out (see report §3.2 and
// §4 test-strategy item 3: "a single catalog mixing ownership:'daemon' and ownership:'external'
// services (today only infra has external adoption, and infra's tests don't cover it)").

let root: string;
let externallyReady: boolean;

beforeEach(async () => {
  root = await mkdtemp(join(tmpdir(), "lsvc-external-"));
  externallyReady = false;
});
afterEach(async () => {
  await rm(root, { recursive: true, force: true });
});

function catalog(): ServiceCatalog {
  return {
    startFailurePolicy: "stop-on-first-failure-keep-started",
    groups: { all: ["sleeper", "sidecar"] },
    services: [
      {
        id: "sleeper",
        profiles: { run: { commandStatus: "verified", command: { command: { argv: ["sleep", "30"] }, cwd: "." }, readiness: { kind: "process" } } },
      },
      {
        id: "sidecar",
        ownership: "external",
        profiles: {
          run: {
            commandStatus: "verified",
            command: { command: { argv: ["true"] }, cwd: "." },
            readiness: { kind: "custom", name: "sidecar-external", probe: async () => (externallyReady ? "ready" : "not-ready") },
          },
        },
      },
    ],
  };
}

describe("ownership: 'external'", () => {
  test("adopts a service once its readiness probe reports ready, and releases it once the probe stops reporting ready", async () => {
    const manager = await LocalServicesManager.bootstrap({ root, catalog: catalog() });
    try {
      expect(manager.serviceStates().find((s) => s.serviceId === "sidecar")?.actualState).toBe("stopped");

      externallyReady = true;
      await manager.supervisor.syncExternalServices();
      const adopted = manager.serviceStates().find((s) => s.serviceId === "sidecar");
      expect(adopted?.actualState).toBe("ready");
      expect(adopted?.readiness).toBe("ready");
      expect(adopted?.desiredState).toBe("running");
      expect(adopted?.readinessDetail).toBe("adopted from external state");

      externallyReady = false;
      await manager.supervisor.syncExternalServices();
      const released = manager.serviceStates().find((s) => s.serviceId === "sidecar");
      expect(released?.actualState).toBe("stopped");
      expect(released?.desiredState).toBe("stopped");
    } finally {
      await manager.shutdown("stop-services");
      await manager.shutdownCompletion;
    }
  }, 15_000);

  test("an externally-owned service never blocks or is touched by a manager shutdown", async () => {
    const manager = await LocalServicesManager.bootstrap({ root, catalog: catalog() });
    try {
      externallyReady = true;
      await manager.supervisor.syncExternalServices();
      expect(manager.serviceStates().find((s) => s.serviceId === "sidecar")?.actualState).toBe("ready");

      const headers = { authorization: `Bearer ${manager.bearerToken}`, "x-local-services-protocol": String(manager.info.protocolVersion), "content-type": "application/json" };
      // refuse-if-active would normally refuse while any daemon-owned service is active, but this
      // catalog has no other active daemon-owned service — the externally-owned "ready" sidecar must
      // not itself count toward that refusal.
      const response = await fetch(`${manager.baseUrl}/v1/manager/shutdown`, { method: "POST", headers, body: JSON.stringify({ requestId: "shutdown-1", mode: "refuse-if-active" }) });
      expect(response.status).toBe(202);

      await manager.shutdownCompletion;
      // Shutdown must not have attempted to stop the externally-owned service — it should still read
      // as 'ready' rather than being force-transitioned to 'stopped' by the daemon's own stop path.
      expect(manager.serviceStates().find((s) => s.serviceId === "sidecar")?.actualState).toBe("ready");
    } catch (error) {
      await manager.shutdown("stop-services").catch(() => undefined);
      throw error;
    }
  }, 15_000);
});
