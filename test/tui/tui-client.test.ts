import { describe, expect, test } from "bun:test";

import { managerProtocolVersion } from "../../src/core/manager";
import type { ManagerMetadata, Operation } from "../../src/core/state";
import type { LocalctlOptions } from "../../src/cli/localctl";
import { ManagerTuiClient } from "../../src/tui/tui-client";
import { keyboardAction } from "../../src/tui/tui-actions";

const metadata: ManagerMetadata = { version: 1, protocolVersion: managerProtocolVersion, instanceId: "manager-a", pid: 42, port: 40123, startedAt: "2026-09-08T00:00:00.000Z" };
const options: LocalctlOptions = { catalog: { services: [], groups: {}, startFailurePolicy: "stop-on-first-failure-keep-started" }, spawnDaemon: () => {} };

describe("TUI manager client", () => {
  test("waits beyond the former two-minute deadline using the injected clock", async () => {
    const requested: string[] = [];
    let polls = 0;
    let now = 0;
    const client = new ManagerTuiClient("/fake", options, {
      discover: async () => ({ kind: "live", client: { root: "/fake", runtimeDirectory: "/fake/.local-services/runtime-v1", metadata, token: "token" } }),
      request: async (_client, path) => {
        requested.push(path);
        if (path === "/v1/manager") return {};
        return { operation: { id: "op-1", requestId: "op-1", kind: "service", serviceId: "mongo", action: "start", status: ++polls > 1_200 ? "succeeded" : "running", createdAt: "2026-09-08T00:00:00.000Z", updatedAt: "2026-09-08T00:00:00.000Z", trace: [] } };
      },
      now: () => now,
      sleep: async () => {
        now += 100;
      },
    });
    await expect(client.waitOperation("op-1")).resolves.toMatchObject({ status: "succeeded" });
    expect(now).toBe(120_000);
    expect(requested.filter((path) => path === "/v1/operations/op-1")).toHaveLength(1_201);
  });

  test("submits start all as one bulk manager operation", async () => {
    const requests: Array<{ path: string; body?: string }> = [];
    const client = new ManagerTuiClient("/fake", options, {
      discover: async () => ({ kind: "live", client: { root: "/fake", runtimeDirectory: "/fake/.local-services/runtime-v1", metadata, token: "token" } }),
      requestId: () => "request-1",
      request: async (_client, path, init) => {
        requests.push({ path, body: init?.body?.toString() });
        return path === "/v1/manager" ? {} : { operation: { id: "bulk-1" } };
      },
    });
    await expect(client.bulkStart(["mongo", "portal"])).resolves.toMatchObject({ id: "bulk-1" });
    expect(requests).toEqual([
      { path: "/v1/manager", body: undefined },
      { path: "/v1/operations/bulk-start", body: JSON.stringify({ requestId: "request-1", targets: ["mongo", "portal"] }) },
    ]);
  });

  test("rejects an incompatible-protocol manager before submitting Start All", async () => {
    const requests: string[] = [];
    const client = new ManagerTuiClient("/fake", options, {
      discover: async () => ({ kind: "incompatible", client: { root: "/fake", runtimeDirectory: "/fake/.local-services/runtime-v1", metadata: { ...metadata, protocolVersion: metadata.protocolVersion + 1 }, token: "token" } }),
      request: async (_client, path) => {
        requests.push(path);
        return { operation: { id: "bulk-1" } };
      },
    });

    await expect(client.bulkStart(["mongo", "portal"])).rejects.toMatchObject({ name: "LocalctlError", exitCode: 4, message: "local services manager protocol is incompatible" });
    expect(requests).toEqual([]);
  });

  test("preserves the manager error detail from a failed bulk operation", async () => {
    const client = new ManagerTuiClient("/fake", options, {
      discover: async () => ({ kind: "live", client: { root: "/fake", runtimeDirectory: "/fake/.local-services/runtime-v1", metadata, token: "token" } }),
      request: async (_client, path) =>
        path === "/v1/manager"
          ? {}
          : {
              operation: {
                id: "bulk-1",
                requestId: "request-1",
                kind: "bulk-start",
                targetServiceIds: ["mongo"],
                status: "failed",
                createdAt: "2026-09-08T00:00:00.000Z",
                updatedAt: "2026-09-08T00:00:00.000Z",
                trace: [],
                error: { code: "operation_failed", message: "Mongo startup failed: port is in use" },
              },
            },
    });

    await expect(client.waitOperation("bulk-1")).resolves.toMatchObject({ status: "failed", error: { message: "Mongo startup failed: port is in use" } });
  });
});

describe("TUI keyboard actions", () => {
  test("r and R rebuild and restart only the focused service", () => {
    const selected = { name: "metadata", state: "ready" };
    expect(keyboardAction("r", selected)).toBe("restart");
    expect(keyboardAction("R", selected)).toBe("restart");
    expect(keyboardAction("R", selected)).not.toBe("restart-all");
  });
  test("Enter and Space start a queued service rather than stopping it", () => {
    const selected = { name: "syncer", state: "queued-start" };

    expect(keyboardAction("\r", selected)).toBe("start");
    expect(keyboardAction(" ", selected)).toBe("start");
  });

  test("x stops exactly the focused service through the manager operation API", async () => {
    const selected = { name: "mongo", kind: "infrastructure" as const, state: "ready" };
    const action = keyboardAction("x", selected);
    const requests: Array<{ path: string; body?: string }> = [];
    const client = new ManagerTuiClient("/fake", options, {
      discover: async () => ({ kind: "live", client: { root: "/fake", runtimeDirectory: "/fake/.local-services/runtime-v1", metadata, token: "token" } }),
      requestId: () => "request-1",
      request: async (_client, path, init) => {
        requests.push({ path, body: init?.body?.toString() });
        return path === "/v1/manager" ? {} : { operation: { id: "stop-1" } };
      },
    });

    expect(action).toBe("stop");
    await client.action(selected.name, action as "stop");
    expect(requests).toEqual([
      { path: "/v1/manager", body: undefined },
      { path: "/v1/operations", body: JSON.stringify({ requestId: "request-1", serviceId: "mongo", action: "stop" }) },
    ]);
    expect(keyboardAction("x", undefined)).toBe("stop");
  });
});
