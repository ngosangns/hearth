import { describe, expect, test } from "bun:test";
import { createHash, createHmac, randomBytes } from "node:crypto";
import { chmod, mkdir, mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { discover, localctlExit, main, ManagerRequestError, managerRequestTimeoutMs, parseCommandFlags, request, targets, waitOperation, type Client, type Discovery, type LocalctlOptions, type LocalctlRuntime } from "../../src/cli/localctl";
import { managerProtocolVersion } from "../../src/core/manager";
import type { ManagerMetadata, Operation } from "../../src/core/state";
import type { ServiceCatalog } from "../../src/core/catalog";

function catalog(): ServiceCatalog {
  return {
    startFailurePolicy: "stop-on-first-failure-keep-started",
    groups: { infra: ["mongo", "redis", "kafka", "nats"], core: ["metadata"], all: ["metadata", "mongo", "redis", "kafka", "nats"] },
    services: [
      { id: "metadata", profiles: { run: { commandStatus: "verified", command: { command: { argv: ["true"] }, cwd: "." }, readiness: { kind: "tcp", port: 11660 } } } },
      ...(["mongo", "redis", "kafka", "nats"] as const).map((id) => ({
        id,
        kind: "infrastructure" as const,
        profiles: { run: { commandStatus: "verified" as const, command: { command: { argv: ["true"] }, cwd: ".", containerName: id }, readiness: { kind: "container" as const } } },
      })),
    ],
  };
}
const options: LocalctlOptions = { catalog: catalog(), spawnDaemon: () => {} };

const metadata: ManagerMetadata = { version: 1, protocolVersion: managerProtocolVersion, instanceId: "manager-a", pid: 42, port: 40123, startedAt: "2026-09-08T00:00:00.000Z" };
const client: Client = { root: "/fake", runtimeDirectory: "/fake/.local-services/runtime-v1", metadata, token: "never-render-this-token" };
const live = (): Discovery => ({ kind: "live", client });
const operation = (action: "start" | "stop" | "restart" = "start"): Operation => ({ id: "operation-1", requestId: "request-1", kind: "service", serviceId: "metadata", action, status: "succeeded", createdAt: metadata.startedAt, updatedAt: metadata.startedAt, trace: [] });

describe("localctl", () => {
  test("rejects the removed profile flag", () => {
    expect(() => parseCommandFlags(["--profile", "dev"], [])).toThrow("unknown flag: --profile");
  });

  test("sends profile-free single-service operations", async () => {
    const requests: Array<{ path: string; body?: string }> = [];
    const runtime: LocalctlRuntime = {
      discover: async () => live(),
      request: async (_client, path, init) => {
        requests.push({ path, body: init?.body?.toString() });
        return { operation: operation("restart") };
      },
    };
    expect(await main(options, ["restart", "metadata", "--wait"], runtime)).toBe(0);
    expect(JSON.parse(requests[0]!.body!)).toEqual({ requestId: expect.any(String), serviceId: "metadata", action: "restart" });
  });

  test("stops an authenticated incompatible manager using its own protocol before upgrade", async () => {
    const legacyClient: Client = { ...client, metadata: { ...metadata, protocolVersion: 1 } };
    const requests: Array<{ path: string; protocol?: string }> = [];
    const runtime: LocalctlRuntime = {
      discover: async () => ({ kind: "incompatible", client: legacyClient }),
      request: async (_client, path, init) => {
        requests.push({ path, protocol: new Headers(init?.headers).get("x-local-services-protocol") ?? undefined });
        return { operation: operation("stop") };
      },
    };
    expect(await main(options, ["manager", "stop", "--json"], runtime)).toBe(0);
    expect(requests).toEqual([{ path: "/v1/manager/shutdown", protocol: "1" }]);
  });

  test("manager reload POSTs the current in-process catalog and prints the result", async () => {
    const requests: Array<{ path: string; body?: string }> = [];
    const runtime: LocalctlRuntime = {
      discover: async () => live(),
      request: async (_client, path, init) => {
        requests.push({ path, body: init?.body?.toString() });
        if (path === "/v1/manager") return {};
        return { stopped: [], changed: ["metadata"] };
      },
    };
    expect(await main(options, ["manager", "reload", "--json"], runtime)).toBe(0);
    const reloadRequest = requests.find((r) => r.path === "/v1/manager/reload")!;
    expect(JSON.parse(reloadRequest.body!)).toEqual({ requestId: expect.any(String), catalog: options.catalog });
  });

  test("prints non-json object results as readable JSON, not [object Object]", async () => {
    const output: string[] = [];
    const runtime: LocalctlRuntime = {
      discover: async () => live(),
      request: async () => ({ operation: operation("stop") }),
      output: (line) => output.push(line),
    };
    expect(await main(options, ["manager", "stop"], runtime)).toBe(0);
    expect(output).toEqual([JSON.stringify(operation("stop"), null, 2)]);
    expect(output.join("")).not.toContain("[object Object]");
  });

  test("discovers an authenticated v3 lock as incompatible, not unavailable", async () => {
    const root = await mkdtemp(join(tmpdir(), "localctl-"));
    try {
      const runtimeDirectory = join(root, ".local-services/runtime-v1");
      const lock = join(runtimeDirectory, "manager.lock");
      const legacyMetadata = { ...metadata, protocolVersion: 3 };
      const token = randomBytes(32).toString("base64url");
      const key = randomBytes(32).toString("base64url");
      const payload = JSON.stringify({
        metadata: { instanceId: legacyMetadata.instanceId, pid: legacyMetadata.pid, port: legacyMetadata.port, protocolVersion: legacyMetadata.protocolVersion, startedAt: legacyMetadata.startedAt, version: legacyMetadata.version },
        tokenDigest: createHash("sha256").update(token).digest("hex"),
      });
      const proof = { version: 1, metadata: legacyMetadata, tokenDigest: createHash("sha256").update(token).digest("hex"), signature: createHmac("sha256", key).update(payload).digest("hex") };
      await mkdir(lock, { recursive: true, mode: 0o700 });
      await Promise.all([writeFile(join(runtimeDirectory, "ownership.key"), key, { mode: 0o600 }), writeFile(join(lock, "metadata.json"), JSON.stringify(legacyMetadata), { mode: 0o600 }), writeFile(join(lock, "token"), token, { mode: 0o600 }), writeFile(join(lock, "ownership.json"), JSON.stringify(proof), { mode: 0o600 })]);
      await Promise.all([chmod(runtimeDirectory, 0o700), chmod(lock, 0o700)]);
      expect(await discover(root, options)).toMatchObject({ kind: "incompatible", client: { metadata: legacyMetadata } });
    } finally {
      await rm(root, { recursive: true, force: true });
    }
  });

  test("routes group start through one bulk manager operation", async () => {
    const requests: Array<{ path: string; body?: string }> = [];
    const runtime: LocalctlRuntime = {
      discover: async () => live(),
      request: async (_client, path, init) => {
        requests.push({ path, body: init?.body?.toString() });
        return path === "/v1/operations/bulk-start" ? { operation: { ...operation("start"), kind: "bulk-start", targetServiceIds: targets(catalog(), "infra"), status: "succeeded" } } : { operation: operation("start") };
      },
    };
    expect(await main(options, ["start", "infra", "--wait"], runtime)).toBe(0);
    expect(requests.map(({ path }) => path)).toEqual(["/v1/operations/bulk-start", "/v1/operations/operation-1"]);
    expect(JSON.parse(requests[0]!.body!)).toEqual({ requestId: expect.any(String), targets: targets(catalog(), "infra") });
  });

  test("reports queued-start services distinctly in status output", async () => {
    const output: string[] = [];
    const runtime: LocalctlRuntime = {
      discover: async () => live(),
      request: async (_client, path) => (path === "/v1/services" ? { services: [{ serviceId: "metadata", actualState: "queued-start" }] } : {}),
      output: (line) => output.push(line),
    };
    expect(await main(options, ["status", "metadata"], runtime)).toBe(0);
    expect(output).toEqual(["queued-start metadata"]);
  });

  // A failed service used to print as "stopped" — indistinguishable from one nobody started, while
  // the TUI, the macOS app and the MCP tools all reported it failed.
  test.each([
    ["failed", "failed"],
    ["orphaned", "orphaned"],
    ["externally-owned", "externally-owned"],
    ["stopping", "stopping"],
    ["running-unready", "running"],
    ["stopped", "stopped"],
  ])("status prints %s as %s, never hiding a failure as stopped", async (actualState, printed) => {
    const output: string[] = [];
    const runtime: LocalctlRuntime = {
      discover: async () => live(),
      request: async (_client, path) => (path === "/v1/services" ? { services: [{ serviceId: "metadata", actualState }] } : {}),
      output: (line) => output.push(line),
    };
    expect(await main(options, ["status", "metadata"], runtime)).toBe(0);
    expect(output).toEqual([`${printed} metadata`]);
  });

  test("continues polling a running operation beyond the former two-minute deadline", async () => {
    let now = 0;
    let polls = 0;
    await expect(
      waitOperation(client, "operation-1", {
        now: () => now,
        sleep: async () => {
          now += 100;
        },
        request: async () => ({ operation: { ...operation(), status: ++polls > 1_200 ? "succeeded" : "running" } }),
      }),
    ).resolves.toMatchObject({ status: "succeeded" });
    expect(now).toBe(120_000);
  });

  test("allows a healthy manager response beyond one second during bulk load", async () => {
    const originalFetch = globalThis.fetch;
    const responseDelayMs = 1_340;
    try {
      globalThis.fetch = ((_input, init) => {
        const { promise: aborted, reject } = Promise.withResolvers<Response>();
        init?.signal?.addEventListener("abort", () => reject(init.signal?.reason), { once: true });
        // This integration-style delay verifies the platform abort signal does not fire at the former one-second cap.
        const response = Bun.sleep(responseDelayMs).then(() => Response.json({ manager: { protocolVersion: managerProtocolVersion } }));
        return Promise.race([response, aborted]);
      }) as typeof fetch;
      expect(responseDelayMs).toBeGreaterThan(1_000);
      expect(responseDelayMs).toBeLessThan(managerRequestTimeoutMs);
      await expect(request(client, "/v1/manager")).resolves.toEqual({ manager: { protocolVersion: managerProtocolVersion } });
    } finally {
      globalThis.fetch = originalFetch;
    }
  });

  test("bounds a manager transport timeout with a diagnostic", async () => {
    const originalFetch = globalThis.fetch;
    const originalTimeout = AbortSignal.timeout;
    const controller = new AbortController();
    try {
      Object.defineProperty(AbortSignal, "timeout", {
        configurable: true,
        value: (milliseconds: number) => {
          expect(milliseconds).toBe(managerRequestTimeoutMs);
          return controller.signal;
        },
      });
      globalThis.fetch = ((_input, init) => {
        const { promise, reject } = Promise.withResolvers<Response>();
        init?.signal?.addEventListener("abort", () => reject(init.signal?.reason), { once: true });
        return promise;
      }) as typeof fetch;
      const pending = request(client, "/v1/manager");
      controller.abort(new DOMException("The operation timed out.", "TimeoutError"));
      await expect(pending).rejects.toThrow(`manager request timed out after ${managerRequestTimeoutMs}ms`);
    } finally {
      Object.defineProperty(AbortSignal, "timeout", { configurable: true, value: originalTimeout });
      globalThis.fetch = originalFetch;
    }
  });

  test("keeps only run-only catalog targets", () => {
    expect(targets(catalog(), "core")).toContain("metadata");
    expect(targets(catalog(), "infra")).toEqual(["mongo", "redis", "kafka", "nats"]);
    expect(targets(catalog(), undefined)).not.toContain("frontend");
    const errors: string[] = [];
    expect(main(options, ["start", "core"], { error: (message) => errors.push(message) })).resolves.toBe(localctlExit.usage);
  });
});

describe("daemon-unreachable exit code", () => {
  // A daemon that is merely down used to escape `main` as an unhandled rejection — a stack trace
  // and exit 1 — because `request` threw a plain Error and `main` rethrew anything that wasn't a
  // LocalctlError. The Rust CLI exits 3 for the same situation.
  test("a failing manager request exits unavailable rather than throwing", async () => {
    const errors: string[] = [];
    const runtime: LocalctlRuntime = {
      discover: async () => live(),
      request: async () => {
        throw new ManagerRequestError("manager unavailable");
      },
      error: (message) => errors.push(message),
    };
    // `manager stop` reaches `request` with the discovered client directly (the same path the
    // "incompatible manager" test above exercises), so the thrown ManagerRequestError is what
    // reaches `main` — previously rethrown as an unhandled rejection.
    expect(await main(options, ["manager", "stop"], runtime)).toBe(localctlExit.unavailable);
    expect(errors.join("\n")).toContain("manager unavailable");
  });
});
