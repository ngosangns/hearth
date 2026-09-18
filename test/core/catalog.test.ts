import { describe, expect, test } from "bun:test";

import { dependencyLevels, validateCatalog, type ServiceCatalog, type ServiceDefinition } from "../../src/core/catalog";

const argvService = (id: string, overrides: Partial<ServiceDefinition> = {}): ServiceDefinition => ({
  id,
  profiles: { run: { commandStatus: "verified", command: { command: { argv: [id] }, cwd: "." }, readiness: { kind: "process" } } },
  ...overrides,
});

describe("validateCatalog", () => {
  test("accepts a well-formed catalog with dependencies and groups", () => {
    const catalog: ServiceCatalog = {
      startFailurePolicy: "stop-on-first-failure-keep-started",
      groups: { all: ["nginx", "api"] },
      services: [argvService("nginx"), argvService("api", { dependencies: ["nginx"] })],
    };
    expect(validateCatalog(catalog)).toEqual({ errors: [], warnings: [] });
  });

  test("rejects a duplicate service id", () => {
    const catalog: ServiceCatalog = { startFailurePolicy: "stop-on-first-failure-keep-started", groups: {}, services: [argvService("api"), argvService("api")] };
    expect(validateCatalog(catalog).errors).toContain("duplicate service api");
  });

  test("rejects a group referencing an unknown service", () => {
    const catalog: ServiceCatalog = { startFailurePolicy: "stop-on-first-failure-keep-started", groups: { all: ["ghost"] }, services: [argvService("api")] };
    expect(validateCatalog(catalog).errors).toContain("group all references unknown service ghost");
  });

  test("rejects a dependency on an unknown service", () => {
    const catalog: ServiceCatalog = { startFailurePolicy: "stop-on-first-failure-keep-started", groups: {}, services: [argvService("api", { dependencies: ["ghost"] })] };
    expect(validateCatalog(catalog).errors).toContain("api depends on unknown service ghost");
  });

  test("detects a dependency cycle and reports the cycle path", () => {
    const catalog: ServiceCatalog = {
      startFailurePolicy: "stop-on-first-failure-keep-started",
      groups: {},
      services: [argvService("a", { dependencies: ["b"] }), argvService("b", { dependencies: ["a"] })],
    };
    expect(validateCatalog(catalog).errors).toEqual(["dependency cycle: a -> b -> a"]);
  });

  test("rejects two services sharing the same tcp readiness port", () => {
    const catalog: ServiceCatalog = {
      startFailurePolicy: "stop-on-first-failure-keep-started",
      groups: {},
      services: [
        { id: "a", profiles: { run: { commandStatus: "verified", command: { command: { argv: ["a"] }, cwd: "." }, readiness: { kind: "tcp", port: 8080 } } } },
        { id: "b", profiles: { run: { commandStatus: "verified", command: { command: { argv: ["b"] }, cwd: "." }, readiness: { kind: "tcp", port: 8080 } } } },
      ],
    };
    expect(validateCatalog(catalog).errors).toContain("port 8080 is shared by a and b");
  });

  test("warns (does not error) on an unresolved command status", () => {
    const catalog: ServiceCatalog = {
      startFailurePolicy: "stop-on-first-failure-keep-started",
      groups: {},
      services: [{ id: "wip", profiles: { run: { commandStatus: "unresolved", readiness: { kind: "process" } } } }],
    };
    const validation = validateCatalog(catalog);
    expect(validation.errors).toEqual([]);
    expect(validation.warnings).toEqual(["wip:run command is unresolved"]);
  });

  test("rejects an invalid build timeout", () => {
    const catalog: ServiceCatalog = {
      startFailurePolicy: "stop-on-first-failure-keep-started",
      groups: {},
      services: [argvService("api", { profiles: { run: argvService("api").profiles.run, build: { command: { command: { argv: ["build"] }, cwd: "." }, timeoutMs: -1 } } })],
    };
    expect(validateCatalog(catalog).errors).toContain("api:build has an invalid timeout");
  });
});

describe("dependencyLevels", () => {
  test("orders a diamond dependency graph into levels by depth, deduplicating shared ancestors", () => {
    const catalog: ServiceCatalog = {
      startFailurePolicy: "stop-on-first-failure-keep-started",
      groups: {},
      services: [argvService("nginx"), argvService("mongo", { dependencies: ["nginx"] }), argvService("redis", { dependencies: ["nginx"] }), argvService("api", { dependencies: ["mongo", "redis"] })],
    };
    expect(dependencyLevels(catalog, ["api"])).toEqual([["nginx"], ["mongo", "redis"], ["api"]]);
  });

  test("includes only the transitive dependencies of the requested targets", () => {
    const catalog: ServiceCatalog = {
      startFailurePolicy: "stop-on-first-failure-keep-started",
      groups: {},
      services: [argvService("nginx"), argvService("api", { dependencies: ["nginx"] }), argvService("unrelated")],
    };
    expect(dependencyLevels(catalog, ["api"]).flat()).toEqual(["nginx", "api"]);
  });

  test("throws when the catalog itself is invalid", () => {
    const catalog: ServiceCatalog = { startFailurePolicy: "stop-on-first-failure-keep-started", groups: {}, services: [argvService("a", { dependencies: ["b"] }), argvService("b", { dependencies: ["a"] })] };
    expect(() => dependencyLevels(catalog, ["a"])).toThrow(/Invalid service catalog/);
  });
});
