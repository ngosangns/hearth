import { afterEach, expect, test } from "bun:test";
import { closeSync, mkdirSync, openSync, readFileSync, statSync, writeSync } from "node:fs";
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
    // Wait for the writer to actually produce output rather than betting 600ms is enough for it
    // to start under load; the survival assertion below is what this test is really about, and it
    // only gets stronger the longer the process has been running.
    const deadline = Date.now() + 15_000;
    const rawPath = rawLogPath(runtimeDirectory, testServiceId);
    const bytesWritten = (): number => statSync(rawPath, { throwIfNoEntry: false })?.size ?? 0;
    while (bytesWritten() === 0 && Date.now() < deadline) await Bun.sleep(25);
    expect(isPidAlive(app.pid)).toBe(true);
    const raw = readFileSync(rawPath, "utf8");
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
    // POLLED, not a fixed sleep. The writer needs at least 5 x `sleep 0.1` = 500ms, and a fixed
    // 900ms wait leaves only 400ms of headroom — which a loaded machine (a full suite running
    // alongside this one) eats, failing the test for a reason that has nothing to do with tailing.
    // Waiting for the actual condition keeps the assertion exactly as strong and removes the bet
    // on scheduling.
    const deadline = Date.now() + 15_000;
    const sawAllLines = (): boolean => {
      const combined = collected.join("");
      return [1, 2, 3, 4, 5].every((i) => combined.includes(`line-${i}`));
    };
    while (!sawAllLines() && Date.now() < deadline) await Bun.sleep(25);
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
