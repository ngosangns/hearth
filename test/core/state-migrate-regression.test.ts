import { afterEach, describe, expect, test } from "bun:test";
import { mkdirSync, readFileSync, readdirSync, writeFileSync } from "node:fs";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { createFileIo } from "../../src/core/file-io";
import { AtomicStateStore } from "../../src/core/manager";
import { resolveRuntimeDirectory } from "../../src/core/paths";
import { STATE_VERSION } from "../../src/core/state";

// Regression test for the one-time upgrade path a consumer needs when it switches from a hand-forked
// copy of this tool onto this package: the old copies key their services under `units` and name them
// `unitId`. Quarantining such a file would report every still-running service as stopped, and the
// next start would then refuse the port its orphaned process still holds.

const scratchDirs: string[] = [];
afterEach(async () => {
  await Promise.all(scratchDirs.splice(0).map((dir) => rm(dir, { recursive: true, force: true })));
});
async function scratchRoot(): Promise<string> {
  const dir = await mkdtemp(join(tmpdir(), "local-services-state-migrate-"));
  scratchDirs.push(dir);
  return dir;
}

const store = (runtimeDirectory: string): AtomicStateStore => new AtomicStateStore(createFileIo(true), runtimeDirectory);

const legacyState = (): unknown => ({
  version: 1,
  units: {
    mongodb: {
      unitId: "mongodb",
      desiredState: "running",
      actualState: "ready",
      readiness: "ready",
      generation: 16,
      createdAt: "2026-09-18T00:00:00.000Z",
      updatedAt: "2026-09-18T12:00:00.000Z",
      readinessKind: "container",
      readinessDetail: "adopted readiness verified",
      identity: {
        managerInstanceId: "old-daemon",
        unitId: "mongodb",
        generation: 16,
        startedAt: "2026-09-18T00:00:00.000Z",
        containerName: "infra-local-mongodb-1",
        containerId: "abc123",
        containerStartedAt: "2026-09-18T00:00:00.000Z",
        commandFingerprint: "fingerprint",
      },
    },
  },
});

describe("legacy state migration", () => {
  test("upgrades a units/unitId state file in place, keeping the adopted identity usable", async () => {
    const root = await scratchRoot();
    const runtimeDirectory = resolveRuntimeDirectory(root);
    mkdirSync(runtimeDirectory, { recursive: true, mode: 0o700 });
    // The store's file guard only accepts owner-private files, exactly as a real daemon writes them.
    writeFileSync(join(runtimeDirectory, "state.json"), JSON.stringify(legacyState()), { mode: 0o600 });

    const loaded = await store(runtimeDirectory).load();

    expect(loaded.version).toBe(STATE_VERSION);
    expect(Object.keys(loaded.services)).toEqual(["mongodb"]);
    const service = loaded.services["mongodb"]!;
    expect(service.serviceId).toBe("mongodb");
    expect(service.actualState).toBe("ready");
    // The identity must carry serviceId (not unitId) or `identityMatchesState` rejects it and the
    // running container is treated as a stranger.
    expect(service.identity?.serviceId).toBe("mongodb");
    expect(service.identity && "unitId" in service.identity).toBe(false);
    // Persisted, so the upgrade happens once.
    const rewritten = JSON.parse(readFileSync(join(runtimeDirectory, "state.json"), "utf8")) as { version: number; services: Record<string, unknown> };
    expect(rewritten.version).toBe(STATE_VERSION);
    expect(Object.keys(rewritten.services)).toEqual(["mongodb"]);
  });

  test("still quarantines a payload that is neither the current nor the legacy shape", async () => {
    const root = await scratchRoot();
    const runtimeDirectory = resolveRuntimeDirectory(root);
    mkdirSync(runtimeDirectory, { recursive: true, mode: 0o700 });
    writeFileSync(join(runtimeDirectory, "state.json"), JSON.stringify({ version: 1, units: { mongodb: { nonsense: true } } }), { mode: 0o600 });

    const loaded = await store(runtimeDirectory).load();

    expect(loaded.services).toEqual({});
    // The payload is quarantined rather than migrated: nothing in it looked like either shape.
    const quarantined = readdirSync(runtimeDirectory).filter((entry) => entry.startsWith("state.json.corrupt-"));
    expect(quarantined).toHaveLength(1);
  });
});
