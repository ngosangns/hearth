import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterEach, describe, expect, test } from "bun:test";

import { resolveServiceUrls, serviceUrlPlaceholders, type ServiceCatalog } from "../../src/core/catalog";
import { LocalServicesManager, managerProtocolVersion } from "../../src/core/manager";
import { parseTailnetHost } from "../../src/core/service-urls";

const scratchDirs: string[] = [];
afterEach(async () => {
  await Promise.all(scratchDirs.splice(0).map((dir) => rm(dir, { recursive: true, force: true })));
});

const catalog = (urls: NonNullable<ServiceCatalog["services"][number]["urls"]>): ServiceCatalog => ({
  startFailurePolicy: "stop-on-first-failure-keep-started",
  groups: {},
  services: [{ id: "api", profiles: { run: { commandStatus: "verified", command: { command: { argv: ["api"] }, cwd: "." }, readiness: { kind: "tcp", port: 18080 } } }, urls }],
});

describe("resolveServiceUrls", () => {
  const templated = catalog([
    { url: "http://127.0.0.1:8080" },
    { url: "https://{tailnetHost}:8443", label: "admin", requiresRunning: false },
  ]);

  test("substitutes placeholders and defaults requiresRunning to true", () => {
    const { urls, unresolved } = resolveServiceUrls(templated, (name) => (name === "tailnetHost" ? "box.tail.ts.net" : undefined));
    expect(unresolved).toEqual([]);
    expect(urls).toEqual([
      { serviceId: "api", url: "http://127.0.0.1:8080", requiresRunning: true },
      { serviceId: "api", label: "admin", url: "https://box.tail.ts.net:8443", requiresRunning: false },
    ]);
  });

  // No Tailscale on this machine: the templated URL is reported, not silently dropped.
  test("reports a URL whose placeholder has no value", () => {
    const { urls, unresolved } = resolveServiceUrls(templated, () => undefined);
    expect(urls).toHaveLength(1);
    expect(unresolved).toEqual([{ serviceId: "api", url: "https://{tailnetHost}:8443", placeholder: "tailnetHost" }]);
  });

  test("extracts placeholder names in order", () => {
    expect(serviceUrlPlaceholders("https://{tailnetHost}:{port}/x")).toEqual(["tailnetHost", "port"]);
    expect(serviceUrlPlaceholders("http://127.0.0.1:1")).toEqual([]);
  });
});

describe("parseTailnetHost", () => {
  test("returns the DNS name without its trailing dot", () => {
    expect(parseTailnetHost('{"Self":{"DNSName":"macbook.tail2b20d9.ts.net."}}')).toBe("macbook.tail2b20d9.ts.net");
  });
  test("has no host for a logged-out or malformed status", () => {
    expect(parseTailnetHost('{"Self":{"DNSName":""}}')).toBeUndefined();
    expect(parseTailnetHost('{"BackendState":"NeedsLogin"}')).toBeUndefined();
    expect(parseTailnetHost("not json")).toBeUndefined();
  });
});

describe("GET /v1/urls", () => {
  test("serves resolved URLs behind auth, in the same shape as the Rust daemon", async () => {
    const runtimeDirectory = await mkdtemp(join(tmpdir(), "local-services-urls-"));
    scratchDirs.push(runtimeDirectory);
    const manager = await LocalServicesManager.bootstrap({ runtimeDirectory, catalog: catalog([{ url: "http://127.0.0.1:18080/app", label: "app", requiresRunning: false }]) });
    try {
      const response = await fetch(`${manager.baseUrl}/v1/urls`, { headers: { authorization: `Bearer ${manager.bearerToken}`, "x-local-services-protocol": String(managerProtocolVersion) } });
      expect(await response.json()).toEqual({ urls: [{ serviceId: "api", label: "app", url: "http://127.0.0.1:18080/app", requiresRunning: false }], unresolved: [] });
      expect((await fetch(`${manager.baseUrl}/v1/urls`)).status).toBe(401);
    } finally {
      await manager.shutdown("stop-services");
    }
  });
});
