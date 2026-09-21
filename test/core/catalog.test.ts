import { describe, expect, test } from "bun:test";

import { validateCatalog, type ServiceCatalog, type ServiceDefinition } from "../../src/core/catalog";

const argvService = (id: string, overrides: Partial<ServiceDefinition> = {}): ServiceDefinition => ({
  id,
  profiles: { run: { commandStatus: "verified", command: { command: { argv: [id] }, cwd: "." }, readiness: { kind: "process" } } },
  ...overrides,
});

describe("validateCatalog", () => {
  test("accepts a well-formed catalog with services and groups", () => {
    const catalog: ServiceCatalog = {
      startFailurePolicy: "stop-on-first-failure-keep-started",
      groups: { all: ["nginx", "api"] },
      services: [argvService("nginx"), argvService("api")],
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
