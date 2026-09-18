import { afterEach, expect, test } from "bun:test";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";

import type { ServiceCatalog, ServiceId } from "../../src/core/catalog";
import { resolveRuntimeDirectory } from "../../src/core/paths";
import { isPidAlive } from "../../src/core/platform";
import type { ServiceLifecycleState } from "../../src/core/state";
import { defaultSupervisorOptions, normalizeCommandFingerprint, ProcessSupervisor, type SupervisorOptions } from "../../src/core/supervisor";

// Regression test for infra's fix (ported from supervisor.terminate.test.ts): stop must reach a
// descendant that moved itself into its own process group. `air` does exactly that with the built
// server binary, so signalling only the tracked pgid left the real server alive holding its port —
// the next start then failed with "Port N is held by an unowned process" even though nothing foreign
// was listening. The tree is snapshotted before signalling so it survives the wrapper's death.

type SupervisorHost = ConstructorParameters<typeof ProcessSupervisor>[0];

const scratchDirs: string[] = [];
afterEach(async () => {
  await Promise.all(scratchDirs.splice(0).map((dir) => rm(dir, { recursive: true, force: true })));
});
async function scratchRoot(): Promise<string> {
  const dir = await mkdtemp(join(tmpdir(), "local-services-terminate-"));
  scratchDirs.push(dir);
  return dir;
}

const serviceId: ServiceId = "sample-service";
const catalog: ServiceCatalog = {
  startFailurePolicy: "stop-on-first-failure-keep-started",
  services: [{ id: serviceId, profiles: { run: { commandStatus: "verified", command: { command: { argv: ["sh", "-c", "set -m; sleep 60 & wait"] }, cwd: "." }, readiness: { kind: "process" } } } }],
  groups: {},
};

type ProcessRow = { pid: number; ppid: number; pgid: number };

async function processTable(): Promise<ProcessRow[]> {
  const child = Bun.spawn(["ps", "-Ao", "pid=,ppid=,pgid="], { stdout: "pipe", stderr: "ignore" });
  const stdout = await new Response(child.stdout).text();
  await child.exited;
  const rows: ProcessRow[] = [];
  for (const line of stdout.split("\n")) {
    const match = /^(\d+)\s+(\d+)\s+(\d+)/.exec(line.trimStart());
    if (match) rows.push({ pid: Number(match[1]), ppid: Number(match[2]), pgid: Number(match[3]) });
  }
  return rows;
}

async function descendantInOwnGroup(leaderPid: number): Promise<ProcessRow | undefined> {
  // Real wall-clock wait on purpose: we are waiting for the OS to fork a child of a live shell, which
  // fake timers cannot advance. Typically settles on the first pass; bounded at ~3s.
  for (let attempt = 0; attempt < 60; attempt++) {
    const rows = await processTable();
    const leader = rows.find((row) => row.pid === leaderPid);
    if (leader) {
      const seen = new Set<number>([leaderPid]);
      const queue = [leaderPid];
      while (queue.length > 0) {
        const pid = queue.shift()!;
        for (const row of rows) {
          if (row.ppid !== pid || seen.has(row.pid)) continue;
          seen.add(row.pid);
          queue.push(row.pid);
          if (row.pgid !== leader.pgid) return row;
        }
      }
    }
    await Bun.sleep(50);
  }
  return undefined;
}

test("stop reaches a descendant that moved into its own process group, like air does with the server binary", async () => {
  const root = await scratchRoot();
  const runtimeDirectory = resolveRuntimeDirectory(root);
  const adapter = defaultSupervisorOptions(root, runtimeDirectory).process;
  const command = { command: { argv: ["sh", "-c", "set -m; sleep 60 & wait"] }, cwd: "." };
  const app = await adapter.spawn({ command, commandFingerprint: normalizeCommandFingerprint(command), serviceId });
  if (!("pgid" in app)) throw new Error("expected a posix process record");

  const escaped = await descendantInOwnGroup(app.pid);
  const states = new Map<ServiceId, ServiceLifecycleState>();
  const host: SupervisorHost = {
    instanceId: "terminate-test-instance",
    catalog,
    serviceStates: () => [...states.values()],
    setServiceState: async (next) => {
      states.set(next.serviceId, structuredClone(next));
    },
    publish: () => {},
    appendLog: async () => {},
  };
  const timestamp = new Date().toISOString();
  states.set(serviceId, {
    serviceId,
    desiredState: "running",
    actualState: "ready",
    readiness: "ready",
    generation: 1,
    createdAt: timestamp,
    updatedAt: timestamp,
    identity: { managerInstanceId: host.instanceId, serviceId, generation: 1, startedAt: timestamp, pid: app.pid, pgid: app.pgid, startIdentity: app.startIdentity, commandFingerprint: app.commandFingerprint },
  } satisfies ServiceLifecycleState);

  const supervisor = new ProcessSupervisor(host, {
    process: adapter,
    runBuild: async () => undefined,
    probes: { tcp: async () => false, http: async () => false, container: async () => false, tailnet: async () => false },
    readinessTimeoutMs: 200,
    readinessBackoffMs: 25,
    terminationGraceMs: 1_000,
  } satisfies SupervisorOptions);

  try {
    expect(escaped).toBeDefined();
    await supervisor.stop(serviceId);
    expect(isPidAlive(app.pid)).toBe(false);
    expect(isPidAlive(escaped!.pid)).toBe(false);
  } finally {
    for (const pid of [app.pid, escaped?.pid ?? 0]) {
      if (pid > 1 && isPidAlive(pid)) process.kill(pid, "SIGKILL");
    }
  }
}, 15_000);
