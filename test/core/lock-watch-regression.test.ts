import { afterEach, describe, expect, test } from "bun:test";
import { mkdirSync, rmSync, writeFileSync } from "node:fs";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { LockOwnershipWatch, readLockInstanceId } from "../../src/core/daemon";
import { lockDir, metadataPath } from "../../src/core/paths";
import type { ManagerMetadata } from "../../src/core/state";

// Regression test for infra's daemon-singleton fix: a daemon whose lock was taken over must stop
// managing services. Two live daemons fighting over one state file is what produced the split-brain
// ("identity no longer matches", a service terminated by a daemon that no longer owned it), and the
// losing daemon must never touch the winner's lock — it only leaves.

const scratchDirs: string[] = [];
afterEach(async () => {
  await Promise.all(scratchDirs.splice(0).map((dir) => rm(dir, { recursive: true, force: true })));
});
async function scratchRuntime(): Promise<string> {
  const dir = await mkdtemp(join(tmpdir(), "local-services-lock-watch-"));
  scratchDirs.push(dir);
  return dir;
}

const writeLock = (runtimeDirectory: string, instanceId: string): void => {
  mkdirSync(lockDir(runtimeDirectory), { recursive: true, mode: 0o700 });
  const metadata: Pick<ManagerMetadata, "instanceId" | "pid" | "port"> = { instanceId, pid: process.pid, port: 1234 };
  writeFileSync(metadataPath(runtimeDirectory), JSON.stringify(metadata));
};

describe("lock ownership watch", () => {
  test("reports a lost lock once and stops, when another daemon takes it over", async () => {
    const runtimeDirectory = await scratchRuntime();
    writeLock(runtimeDirectory, "instance-a");
    expect(readLockInstanceId(runtimeDirectory)).toBe("instance-a");

    const losses: string[] = [];
    const watch = new LockOwnershipWatch(runtimeDirectory, "instance-a", () => losses.push("lost"));
    watch.check();
    expect(losses).toEqual([]);

    writeLock(runtimeDirectory, "instance-b");
    watch.check();
    expect(losses).toEqual(["lost"]);

    // A daemon that already gave up must not report again (and must not keep polling).
    watch.check();
    expect(losses).toEqual(["lost"]);
  });

  test("reports a lost lock when the lock file disappears", async () => {
    const runtimeDirectory = await scratchRuntime();
    writeLock(runtimeDirectory, "instance-a");
    const losses: string[] = [];
    const watch = new LockOwnershipWatch(runtimeDirectory, "instance-a", () => losses.push("lost"));

    rmSync(lockDir(runtimeDirectory), { recursive: true, force: true });
    watch.check();
    expect(losses).toEqual(["lost"]);
  });

  test("an unreadable lock file is not mistaken for a lost lock", async () => {
    const runtimeDirectory = await scratchRuntime();
    mkdirSync(lockDir(runtimeDirectory), { recursive: true, mode: 0o700 });
    writeFileSync(metadataPath(runtimeDirectory), "{ not json");
    const losses: string[] = [];
    new LockOwnershipWatch(runtimeDirectory, "instance-a", () => losses.push("lost")).check();
    expect(losses).toEqual([]);
  });
});
