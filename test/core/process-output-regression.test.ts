import { afterEach, expect, test } from "bun:test";
import { closeSync, mkdirSync, openSync, readFileSync, writeSync } from "node:fs";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { logsDir, rawLogPath, resolveRuntimeDirectory } from "../../src/core/paths";
import { isPidAlive } from "../../src/core/platform";
import { defaultSupervisorOptions, tailFile } from "../../src/core/supervisor";

// Regression test for infra's fix (ported from supervisor.processOutput.test.ts): a managed dev
// process's stdout/stderr must be captured via a plain file, never a pipe the daemon reads directly —
// once the daemon exits, a piped child's next write earns it a SIGPIPE, which kills a real (non-Node)
// process almost instantly by default.

const scratchDirs: string[] = [];
afterEach(async () => {
  await Promise.all(scratchDirs.splice(0).map((dir) => rm(dir, { recursive: true, force: true })));
});
async function scratchRoot(): Promise<string> {
  const dir = await mkdtemp(join(tmpdir(), "local-services-spawn-"));
  scratchDirs.push(dir);
  return dir;
}

const testServiceId = "sample-service";

test("a process-service survives even when nothing ever reads its output", async () => {
  const root = await scratchRoot();
  const runtimeDirectory = resolveRuntimeDirectory(root);
  const adapter = defaultSupervisorOptions(root, runtimeDirectory).process;
  const app = await adapter.spawn({
    command: { command: { argv: ["yes"] }, cwd: "." },
    commandFingerprint: "test-fingerprint",
    serviceId: testServiceId,
  });
  if (!("pgid" in app)) throw new Error("expected a posix process record");
  try {
    // `yes` writes continuously and unbuffered; piped into a reader nobody drains it dies from
    // SIGPIPE almost instantly (this is the actual bug: real dev servers behave the same way once
    // the daemon that used to read their stdout pipe is gone). The file-backed redirect must not
    // have this failure mode at all.
    await Bun.sleep(600);
    expect(isPidAlive(app.pid)).toBe(true);
    const raw = readFileSync(rawLogPath(runtimeDirectory, testServiceId), "utf8");
    expect(raw.length).toBeGreaterThan(0);
  } finally {
    if ("pgid" in app && app.pgid) {
      try {
        process.kill(-app.pgid, "SIGKILL");
      } catch {}
    }
  }
}, 10_000);

test("attachOutput tails and truncates the raw capture file without disturbing the writer", async () => {
  const root = await scratchRoot();
  const runtimeDirectory = resolveRuntimeDirectory(root);
  const adapter = defaultSupervisorOptions(root, runtimeDirectory).process;
  const app = await adapter.spawn({
    command: { command: { shell: "for i in 1 2 3 4 5; do echo line-$i; sleep 0.1; done" }, cwd: "." },
    commandFingerprint: "test-fingerprint",
    serviceId: testServiceId,
  });
  try {
    const collected: string[] = [];
    const stopTail = adapter.attachOutput!(testServiceId, (data) => collected.push(data));
    await Bun.sleep(900);
    stopTail();
    const combined = collected.join("");
    for (let i = 1; i <= 5; i++) expect(combined).toContain(`line-${i}`);
  } finally {
    if ("pgid" in app && app.pgid) {
      try {
        process.kill(-app.pgid, "SIGKILL");
      } catch {}
    }
  }
}, 10_000);

test("tailFile stop() flushes trailing output written just before the writer exits", async () => {
  const root = await scratchRoot();
  const runtimeDirectory = resolveRuntimeDirectory(root);
  const path = rawLogPath(runtimeDirectory, testServiceId);
  mkdirSync(logsDir(runtimeDirectory), { recursive: true });
  closeSync(openSync(path, "w"));
  const fd = openSync(path, "a");
  writeSync(fd, "final-line\n");
  closeSync(fd);
  const collected: string[] = [];
  const tail = tailFile(path, (data) => collected.push(data));
  tail.stop();
  expect(collected.join("")).toContain("final-line");
});
