import { mkdir, mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterEach, describe, expect, test } from "bun:test";

import { findConfigFile, loadCatalog, loadCatalogFromFile } from "../../src/core/config-file";

const scratchDirs: string[] = [];
afterEach(async () => {
  await Promise.all(scratchDirs.splice(0).map((dir) => rm(dir, { recursive: true, force: true })));
});
async function scratchRoot(): Promise<string> {
  const dir = await mkdtemp(join(tmpdir(), "local-services-config-file-"));
  scratchDirs.push(dir);
  return dir;
}

describe("findConfigFile", () => {
  test("prefers yaml, then yml, then json, then the .ts escape hatch", async () => {
    const root = await scratchRoot();
    await writeFile(join(root, "local-services.config.ts"), "export const catalog = {};");
    expect(await findConfigFile(root)).toBe(join(root, "local-services.config.ts"));
    await writeFile(join(root, "local-services.json"), "{}");
    expect(await findConfigFile(root)).toBe(join(root, "local-services.json"));
    await writeFile(join(root, "local-services.yml"), "version: 1");
    expect(await findConfigFile(root)).toBe(join(root, "local-services.yml"));
    await writeFile(join(root, "local-services.yaml"), "version: 1");
    expect(await findConfigFile(root)).toBe(join(root, "local-services.yaml"));
  });

  test("returns undefined when no candidate exists", async () => {
    expect(await findConfigFile(await scratchRoot())).toBeUndefined();
  });
});

describe("loadCatalog (yaml)", () => {
  test("maps a full declarative config into a ServiceCatalog", async () => {
    const root = await scratchRoot();
    await mkdir(join(root, "apps/api"), { recursive: true });
    await writeFile(
      join(root, "local-services.yaml"),
      `
version: 1
env:
  NODE_ENV: development
groups:
  all: [redis, api]
services:
  redis:
    kind: infrastructure
    ownership: external
    container: myapp-redis
    run: { argv: [docker, compose, up, -d, redis] }
    readiness: { kind: container }
  api:
    cwd: apps/api
    env:
      PORT: "8080"
    build: { argv: [go, build, ./...], timeoutMs: 120000, serializationKey: go }
    run: { shell: "air -c .air.toml", exec: true }
    readiness: { kind: tcp, port: 8080 }
    ports:
      - { port: 6060, label: pprof }
`,
    );
    const result = await loadCatalog(root);
    expect(result.ok).toBe(true);
    if (!result.ok) return;
    expect(result.catalog.groups).toEqual({ all: ["redis", "api"] });
    const redis = result.catalog.services.find((s) => s.id === "redis")!;
    expect(redis.ownership).toBe("external");
    expect(redis.kind).toBe("infrastructure");
    expect(redis.profiles.run.commandStatus).toBe("verified");
    const api = result.catalog.services.find((s) => s.id === "api")!;
    expect(api.profiles.build).toEqual({ command: { command: { argv: ["go", "build", "./..."] }, cwd: "apps/api" }, timeoutMs: 120000, serializationKey: "go" });
    expect(api.ports).toEqual([{ port: 6060, label: "pprof", requiresRunning: undefined }]);
    expect(api.profiles.run.commandStatus).toBe("verified");
    if (api.profiles.run.commandStatus === "verified") {
      expect(api.profiles.run.command.command).toEqual({ shell: "air -c .air.toml", exec: true });
      expect(api.profiles.run.command.cwd).toBe("apps/api");
      expect(api.profiles.run.command.environment).toEqual({ NODE_ENV: "development", PORT: "8080" });
    }
  });

  test("a service with no run command becomes unresolved", async () => {
    const root = await scratchRoot();
    await writeFile(join(root, "local-services.yaml"), `version: 1\nservices:\n  todo:\n    readiness: { kind: process }\n`);
    const result = await loadCatalog(root);
    expect(result.ok).toBe(true);
    if (result.ok) expect(result.catalog.services[0]!.profiles.run.commandStatus).toBe("unresolved");
  });

  test("also reads a plain-JSON config file (JSON is valid YAML)", async () => {
    const root = await scratchRoot();
    await writeFile(join(root, "local-services.json"), JSON.stringify({ version: 1, services: { web: { run: { argv: ["bun", "run", "dev"] }, readiness: { kind: "process" } } } }));
    const result = await loadCatalog(root);
    expect(result.ok).toBe(true);
    if (result.ok) expect(result.catalog.services.map((s) => s.id)).toEqual(["web"]);
  });

  test("rejects a cwd that escapes the project root", async () => {
    const root = await scratchRoot();
    await writeFile(join(root, "local-services.yaml"), `version: 1\nservices:\n  api:\n    cwd: ../../etc\n    run: { argv: [x] }\n    readiness: { kind: process }\n`);
    const result = await loadCatalog(root);
    expect(result.ok).toBe(false);
    if (!result.ok) expect(result.errors.some((e) => e.includes("escapes the project root"))).toBe(true);
  });

  test("coerces a bare numeric/boolean argv element to a string (YAML `[sleep, 30]` parses 30 as a number)", async () => {
    const root = await scratchRoot();
    await writeFile(join(root, "local-services.yaml"), `version: 1\nservices:\n  sleeper:\n    run: { argv: [sleep, 30] }\n    readiness: { kind: process }\n`);
    const result = await loadCatalog(root);
    expect(result.ok).toBe(true);
    if (result.ok && result.catalog.services[0]!.profiles.run.commandStatus === "verified") {
      expect(result.catalog.services[0]!.profiles.run.command.command).toEqual({ argv: ["sleep", "30"] });
    }
  });

  test("rejects a command with both argv and shell, or neither", async () => {
    const root = await scratchRoot();
    await writeFile(join(root, "local-services.yaml"), `version: 1\nservices:\n  api:\n    run: { argv: [x], shell: "y" }\n    readiness: { kind: process }\n`);
    const result = await loadCatalog(root);
    expect(result.ok).toBe(false);
    if (!result.ok) expect(result.errors.some((e) => e.includes("exactly one of"))).toBe(true);
  });

  test("rejects an unknown readiness kind with a path-qualified error", async () => {
    const root = await scratchRoot();
    await writeFile(join(root, "local-services.yaml"), `version: 1\nservices:\n  api:\n    run: { argv: [x] }\n    readiness: { kind: magic }\n`);
    const result = await loadCatalog(root);
    expect(result.ok).toBe(false);
    if (!result.ok) expect(result.errors).toContain('services.api.readiness.kind must be one of process, tcp, http, container, tailnet, command, got "magic"');
  });

  test("propagates cross-service validation errors (group referencing an unknown service) from validateCatalog", async () => {
    const root = await scratchRoot();
    await writeFile(
      join(root, "local-services.yaml"),
      `version: 1\ngroups:\n  all: [ghost]\nservices:\n  a:\n    run: { argv: [a] }\n    readiness: { kind: process }\n`,
    );
    const result = await loadCatalog(root);
    expect(result.ok).toBe(false);
    if (!result.ok) expect(result.errors.some((e) => e.includes("references unknown service"))).toBe(true);
  });

  test("maps command readiness with an explicit cwd", async () => {
    const root = await scratchRoot();
    await writeFile(
      join(root, "local-services.yaml"),
      `version: 1\nservices:\n  migrations:\n    run: { argv: [task, migrate] }\n    readiness: { kind: command, command: { argv: [task, "db:check"] }, cwd: infra }\n`,
    );
    const result = await loadCatalog(root);
    expect(result.ok).toBe(true);
    if (result.ok) expect(result.catalog.services[0]!.profiles.run.readiness).toEqual({ kind: "command", command: { argv: ["task", "db:check"] }, cwd: "infra" });
  });

  test("maps a declarative preparationCommand alongside readiness", async () => {
    const root = await scratchRoot();
    await writeFile(
      join(root, "local-services.yaml"),
      `version: 1\nservices:\n  sync:\n    run: { argv: [task, sync] }\n    readiness: { kind: process }\n    preparationCommand: { command: { argv: [task, "sync:prepare"] }, cwd: infra }\n`,
    );
    const result = await loadCatalog(root);
    expect(result.ok).toBe(true);
    if (result.ok) expect(result.catalog.services[0]!.profiles.run).toMatchObject({ preparationCommand: { command: { argv: ["task", "sync:prepare"] }, cwd: "infra" } });
  });

  test("maps a preparationCommand's serializationKey", async () => {
    const root = await scratchRoot();
    await writeFile(
      join(root, "local-services.yaml"),
      `version: 1\nservices:\n  sync:\n    run: { argv: [task, sync] }\n    readiness: { kind: process }\n    preparationCommand: { command: { argv: [task, "sync:prepare"] }, serializationKey: shared }\n`,
    );
    const result = await loadCatalog(root);
    expect(result.ok).toBe(true);
    if (result.ok) expect(result.catalog.services[0]!.profiles.run).toMatchObject({ preparationCommand: { serializationKey: "shared" } });
  });

  test("rejects a malformed preparationCommand", async () => {
    const root = await scratchRoot();
    await writeFile(join(root, "local-services.yaml"), `version: 1\nservices:\n  sync:\n    run: { argv: [task, sync] }\n    readiness: { kind: process }\n    preparationCommand: "not-an-object"\n`);
    const result = await loadCatalog(root);
    expect(result.ok).toBe(false);
    if (!result.ok) expect(result.errors.some((e) => e.includes("preparationCommand"))).toBe(true);
  });

  test("reports a missing config file", async () => {
    const result = await loadCatalog(await scratchRoot());
    expect(result.ok).toBe(false);
    if (!result.ok) expect(result.errors[0]).toContain("no config file found");
  });

  test("reports a YAML parse error", async () => {
    const root = await scratchRoot();
    await writeFile(join(root, "local-services.yaml"), "services:\n  api: [this is not\n");
    const result = await loadCatalogFromFile(join(root, "local-services.yaml"), root);
    expect(result.ok).toBe(false);
  });
});

describe("loadCatalog (.config.ts escape hatch)", () => {
  test("imports a hand-authored ServiceCatalog", async () => {
    const root = await scratchRoot();
    await writeFile(
      join(root, "local-services.config.ts"),
      `export const catalog = { startFailurePolicy: "stop-on-first-failure-keep-started", groups: {}, services: [{ id: "x", profiles: { run: { commandStatus: "verified", command: { command: { argv: ["x"] }, cwd: "." }, readiness: { kind: "process" } } } }] };\n`,
    );
    const result = await loadCatalog(root);
    expect(result.ok).toBe(true);
    if (result.ok) expect(result.catalog.services.map((s) => s.id)).toEqual(["x"]);
  });

  test("rejects a .config.ts that doesn't export a catalog", async () => {
    const root = await scratchRoot();
    await writeFile(join(root, "local-services.config.ts"), "export const somethingElse = 1;\n");
    const result = await loadCatalog(root);
    expect(result.ok).toBe(false);
  });
});

describe("service urls", () => {
  test("maps urls in both the string and object forms", async () => {
    const root = await scratchRoot();
    await writeFile(join(root, "local-services.yaml"), 'version: 1\nservices:\n  api:\n    run: { argv: [x] }\n    readiness: { kind: process }\n    urls:\n      - http://127.0.0.1:8080\n      - { url: "https://{tailnetHost}:8443", label: admin, requiresRunning: false }\n');
    const result = await loadCatalog(root);
    if (!result.ok) throw new Error(result.errors.join("; "));
    expect(result.catalog.services[0]?.urls).toEqual([{ url: "http://127.0.0.1:8080" }, { url: "https://{tailnetHost}:8443", label: "admin", requiresRunning: false }]);
  });

  // A typo'd placeholder must fail the load rather than render a dead link — with exactly the
  // messages the Rust loader produces for the same input.
  test("rejects an unknown placeholder and a non-http url", async () => {
    const root = await scratchRoot();
    await writeFile(join(root, "local-services.yaml"), 'version: 1\nservices:\n  api:\n    run: { argv: [x] }\n    readiness: { kind: process }\n    urls: ["https://{tailnethost}:1", "ftp://x"]\n');
    const result = await loadCatalog(root);
    expect(result.ok).toBe(false);
    if (result.ok) return;
    expect(result.errors).toEqual(["api:urls[0] has unknown placeholder {tailnethost} (known: {tailnetHost})", "api:urls[1] must be an http:// or https:// URL"]);
  });
});

describe("unknown keys", () => {
  // `command:` instead of `run:` used to be dropped silently, leaving a service that only failed
  // later with an unrelated "unsupported service" — now it fails the load, same message as Rust.
  test("rejects a typo'd key but allows x- prefixed ones", async () => {
    const root = await scratchRoot();
    await writeFile(join(root, "local-services.yaml"), "version: 1\nx-common: &c { kind: process }\nservices:\n  api:\n    command: [x]\n    readiness: *c\n");
    const result = await loadCatalog(root);
    expect(result.ok).toBe(false);
    if (result.ok) return;
    expect(result.errors).toEqual([
      'services.api has unknown key "command" (known: label, kind, ownership, env, container, cwd, run, stop, build, readiness, preparationCommand, ports, urls)',
    ]);
  });
});
