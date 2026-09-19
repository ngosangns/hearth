import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterEach, describe, expect, test } from "bun:test";

import { clearLoginShellEnvCacheForTests, loadEnvFile, resolveBaseEnvironment, resolveLoginShellEnv } from "../../src/core/env";

const scratchDirs: string[] = [];
afterEach(async () => {
  clearLoginShellEnvCacheForTests();
  await Promise.all(scratchDirs.splice(0).map((dir) => rm(dir, { recursive: true, force: true })));
});
async function scratchRoot(): Promise<string> {
  const dir = await mkdtemp(join(tmpdir(), "local-services-env-"));
  scratchDirs.push(dir);
  return dir;
}

describe("resolveLoginShellEnv", () => {
  test("captures the real environment of a real shell end-to-end", async () => {
    // /bin/sh always exists; this exercises the actual spawn + marker + parse path rather than
    // mocking it away, at the cost of depending on the host having a working /bin/sh -ilc.
    const env = await resolveLoginShellEnv("/bin/sh");
    expect(env.PATH).toBeTruthy();
  });

  test("never throws for a shell that does not exist, and resolves to {}", async () => {
    const env = await resolveLoginShellEnv("/no/such/shell-binary");
    expect(env).toEqual({});
  });

  test("caches per shell path", async () => {
    const first = resolveLoginShellEnv("/bin/sh");
    const second = resolveLoginShellEnv("/bin/sh");
    expect(first).toBe(second);
    await first;
  });
});

describe("loadEnvFile", () => {
  test("parses KEY=VALUE lines, skipping comments and blanks", async () => {
    const root = await scratchRoot();
    const path = join(root, ".env.local");
    await writeFile(path, `# a comment\n\nFOO=bar\nexport BAZ=qux\nQUOTED="has spaces"\nSINGLE='also quoted'\n`);
    expect(await loadEnvFile(path)).toEqual({ FOO: "bar", BAZ: "qux", QUOTED: "has spaces", SINGLE: "also quoted" });
  });

  test("returns {} for a missing file", async () => {
    expect(await loadEnvFile(join(await scratchRoot(), "missing.env"))).toEqual({});
  });
});

describe("resolveBaseEnvironment", () => {
  test("layers process.env, envFile, and extra overrides in priority order", async () => {
    const root = await scratchRoot();
    await writeFile(join(root, ".env"), "SHARED=from-file\nFILE_ONLY=file\n");
    const env = await resolveBaseEnvironment({ shell: false, root, envFile: ".env", extra: { SHARED: "from-extra", EXTRA_ONLY: "extra" } });
    expect(env.SHARED).toBe("from-extra"); // extra wins over envFile
    expect(env.FILE_ONLY).toBe("file");
    expect(env.EXTRA_ONLY).toBe("extra");
    expect(env.PATH).toBe(process.env.PATH); // process.env is still the floor
  });

  test("shell: false skips shell resolution entirely", async () => {
    const env = await resolveBaseEnvironment({ shell: false });
    expect(env).toEqual(process.env as Record<string, string>);
  });
});
