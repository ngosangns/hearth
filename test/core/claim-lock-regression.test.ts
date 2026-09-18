import { afterEach, expect, test } from "bun:test";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { createFileIo } from "../../src/core/file-io";
import { createLockOwnershipProof, LocalServicesManager } from "../../src/core/manager";
import { lockOwnershipProofName, ownershipKeyName } from "../../src/core/paths";
import { isPidAlive } from "../../src/core/platform";
import type { ManagerMetadata } from "../../src/core/state";
import type { ServiceCatalog } from "../../src/core/catalog";

// Regression test for infra's fix (ported from manager.claimLock.test.ts): a lock-claim loop must
// never treat a failed/timed-out health check as proof the owning manager is dead. A production
// incident (263 concurrent daemons, load 425) happened because the old code did exactly that.

const scratchDirs: string[] = [];
afterEach(async () => {
  await Promise.all(scratchDirs.splice(0).map((dir) => rm(dir, { recursive: true, force: true })));
});
async function scratchDir(): Promise<string> {
  const dir = await mkdtemp(join(tmpdir(), "local-services-claimlock-"));
  scratchDirs.push(dir);
  return dir;
}

const emptyCatalog: ServiceCatalog = { startFailurePolicy: "stop-on-first-failure-keep-started", groups: {}, services: [] };

async function seedExistingLock(runtimeDirectory: string, metadata: ManagerMetadata, token: string): Promise<void> {
  const io = createFileIo(true);
  const lockPath = join(runtimeDirectory, "manager.lock");
  const key = "test-ownership-key";
  await io.writeFile(join(runtimeDirectory, ownershipKeyName), key);
  await io.writeFile(join(lockPath, "metadata.json"), JSON.stringify(metadata));
  await io.writeFile(join(lockPath, "token"), token);
  await io.writeFile(join(lockPath, lockOwnershipProofName), JSON.stringify(createLockOwnershipProof(key, metadata, token)));
}

test("isPidAlive reflects real process liveness", async () => {
  expect(isPidAlive(process.pid)).toBe(true);
  const child = Bun.spawn(["true"]);
  await child.exited;
  expect(isPidAlive(child.pid)).toBe(false);
});

test("claimLock (via LocalServicesManager.bootstrap) waits instead of stealing the lock from a live-but-unresponsive owner, then claims it once the owner truly exits", async () => {
  const runtimeDirectory = await scratchDir();
  const owner = Bun.spawn(["sleep", "30"]);
  const ownerMetadata: ManagerMetadata = {
    version: 1,
    protocolVersion: 1,
    instanceId: "owner-instance",
    pid: owner.pid,
    port: 1, // nothing listens on port 1 — the HTTP health check is guaranteed to fail fast
    startedAt: new Date(0).toISOString(), // long past the startup grace period
  };
  await seedExistingLock(runtimeDirectory, ownerMetadata, "owner-token");

  const attempt = LocalServicesManager.bootstrap({ runtimeDirectory, catalog: emptyCatalog });

  let settled = false;
  void attempt.then(
    () => (settled = true),
    () => (settled = true),
  );
  await Bun.sleep(500);
  // A failed health check alone must never be treated as proof of death while the owner PID is alive —
  // this is the exact split-brain that produced a daemon-storm (hundreds of daemons fighting over one root).
  expect(settled).toBe(false);

  owner.kill();
  await owner.exited;

  const manager = await attempt;
  try {
    expect(manager.instanceId).not.toBe(ownerMetadata.instanceId);
  } finally {
    await manager.shutdown("stop-services");
  }
}, 10_000);
