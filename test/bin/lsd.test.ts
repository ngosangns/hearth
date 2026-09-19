// End-to-end coverage for the `lsd` bin entry (src/bin/lsd.ts): spawns the real script as a
// subprocess, exactly as an installed `lsd` binary would run, against a real `local-services.yaml`.
// Everything else in this package's test suite exercises the library surface directly with fakes;
// this is the one place that verifies the wiring `lsd.ts` itself does (config loading, spawnDaemon,
// argv/`--root` handling) actually works end to end.

import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterEach, describe, expect, test } from "bun:test";

const binPath = join(import.meta.dir, "../../src/bin/lsd.ts");
const scratchDirs: string[] = [];

afterEach(async () => {
  await Promise.all(scratchDirs.splice(0).map((dir) => rm(dir, { recursive: true, force: true })));
});

async function scratchRoot(): Promise<string> {
  const dir = await mkdtemp(join(tmpdir(), "local-services-lsd-bin-"));
  scratchDirs.push(dir);
  return dir;
}

async function lsd(args: string[]): Promise<{ code: number; stdout: string; stderr: string }> {
  const proc = Bun.spawn(["bun", "run", binPath, ...args], { stdout: "pipe", stderr: "pipe" });
  const [stdout, stderr, code] = await Promise.all([new Response(proc.stdout).text(), new Response(proc.stderr).text(), proc.exited]);
  return { code, stdout, stderr };
}

describe("lsd bin", () => {
  test("reports a clear error and exits non-zero when no config file exists", async () => {
    const root = await scratchRoot();
    const result = await lsd(["--root", root, "status"]);
    expect(result.code).not.toBe(0);
    expect(result.stderr).toContain("no config file found");
  });

  test("spawns a daemon, starts/stops a service, and stops the daemon — end to end against a real local-services.yaml", async () => {
    const root = await scratchRoot();
    await writeFile(join(root, "local-services.yaml"), `version: 1\nservices:\n  sleeper:\n    run: { argv: [sleep, 30] }\n    readiness: { kind: process }\n`);
    try {
      const start = await lsd(["--root", root, "start", "sleeper", "--wait", "--json"]);
      expect(start.code).toBe(0);
      expect((JSON.parse(start.stdout) as { operations: Array<{ status: string }> }).operations[0]?.status).toBe("succeeded");

      const status = await lsd(["--root", root, "status", "--json"]);
      expect(status.code).toBe(0);
      const services = (JSON.parse(status.stdout) as { services: Array<{ serviceId: string; state: string; pid?: number }> }).services;
      expect(services).toEqual([{ serviceId: "sleeper", state: "running", pid: expect.any(Number) }]);

      const ensure = await lsd(["--root", root, "manager", "ensure", "--json"]);
      expect(ensure.code).toBe(0);
      const connection = JSON.parse(ensure.stdout) as { instanceId: string; port: number; token: string; protocolVersion: number; runtimeDirectory: string; root: string };
      expect(connection.token.length).toBeGreaterThan(0);
      expect(connection.root).toBe(root);

      const stopService = await lsd(["--root", root, "stop", "sleeper", "--wait", "--json"]);
      expect(stopService.code).toBe(0);
    } finally {
      const managerStop = await lsd(["--root", root, "manager", "stop", "--json"]);
      expect(managerStop.code).toBe(0);
    }
  }, 15_000);
});
