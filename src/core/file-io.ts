// Two interchangeable strategies for every lock/state/log file operation the manager performs,
// selected by `ServiceCatalog.privateFileGuard` (default on):
//
//  - `guarded`: viclass's O_NOFOLLOW + dev/ino identity-tracking layer, defending against a symlink
//    swapped in between a check and a use on a multi-user machine.
//  - `plain`: infra's straightforward `node:fs/promises` calls — infra's own docs call the guard
//    unnecessary on a single-user dev machine, and this keeps that lighter path available.
//
// Both implementations still write atomically (temp file + rename) and quarantine (rename aside,
// never delete) anything that fails validation, so state is never silently lost either way.

import { constants } from "node:fs";
import { lstat, mkdir, open, readFile, rename, rm, unlink, writeFile } from "node:fs/promises";
import { dirname, join } from "node:path";
import { randomUUID } from "node:crypto";

export class UnsafeFileError extends Error {
  constructor(path: string) {
    super(`Unsafe private file: ${path}`);
    this.name = "UnsafeFileError";
  }
}

export interface FileIo {
  readonly guarded: boolean;
  ensureDirectory(path: string): Promise<void>;
  isPrivateDirectory(path: string): Promise<boolean>;
  readFile(path: string): Promise<string | undefined>;
  /** Atomic write via temp file + rename. */
  writeFile(path: string, content: string): Promise<void>;
  /** Create-if-absent (O_CREAT|O_EXCL semantics); returns true if this call created it. */
  createExclusive(path: string, content: string): Promise<boolean>;
  removeFile(path: string): Promise<void>;
  quarantine(path: string, suffix: string): Promise<void>;
  ageMs(path: string): Promise<number>;
}

const isRecord = (value: unknown): value is Record<string, unknown> => typeof value === "object" && value !== null;

// ---------------------------------------------------------------------------------------------
// Plain strategy
// ---------------------------------------------------------------------------------------------

async function plainEnsureDirectory(path: string): Promise<void> {
  await mkdir(path, { recursive: true, mode: 0o700 });
}
async function plainReadFile(path: string): Promise<string | undefined> {
  try {
    return await readFile(path, "utf8");
  } catch (error) {
    if ((error as NodeJS.ErrnoException).code === "ENOENT") return undefined;
    throw error;
  }
}
async function plainWriteFile(path: string, content: string): Promise<void> {
  await plainEnsureDirectory(dirname(path));
  const temporary = `${path}.tmp-${process.pid}-${randomUUID()}`;
  await writeFile(temporary, content, { encoding: "utf8", mode: 0o600 });
  await rename(temporary, path);
}
async function plainRemoveFile(path: string): Promise<void> {
  await unlink(path).catch((error) => {
    if ((error as NodeJS.ErrnoException).code !== "ENOENT") throw error;
  });
}

const plainFileIo: FileIo = {
  guarded: false,
  ensureDirectory: plainEnsureDirectory,
  isPrivateDirectory: async (path) =>
    (await lstat(path).catch(() => undefined))?.isDirectory() ?? false,
  readFile: plainReadFile,
  writeFile: plainWriteFile,
  createExclusive: async (path, content) => {
    await plainEnsureDirectory(dirname(path));
    try {
      const handle = await open(path, constants.O_CREAT | constants.O_EXCL | constants.O_WRONLY, 0o600);
      try {
        await handle.writeFile(content, "utf8");
      } finally {
        await handle.close();
      }
      return true;
    } catch (error) {
      if ((error as NodeJS.ErrnoException).code === "EEXIST") return false;
      throw error;
    }
  },
  removeFile: plainRemoveFile,
  quarantine: async (path, suffix) => {
    await rename(path, `${path}.${suffix}-${Date.now()}-${randomUUID()}`).catch(() => undefined);
  },
  ageMs: async (path) => (await lstat(path).then((entry) => Date.now() - entry.mtimeMs).catch(() => 0)),
};

// ---------------------------------------------------------------------------------------------
// Guarded strategy — O_NOFOLLOW + dev/ino identity tracking
// ---------------------------------------------------------------------------------------------

type Identity = { dev: number; ino: number };
type Handle = Awaited<ReturnType<typeof open>>;

const isPrivateMode = (mode: number): boolean => (mode & 0o077) === 0;
const noFollow = (flags: number): number => {
  if (typeof constants.O_NOFOLLOW !== "number") throw new Error("O_NOFOLLOW is required for private file access");
  return flags | constants.O_NOFOLLOW;
};
const isPrivateRegular = (entry: { isFile(): boolean; isSymbolicLink(): boolean; mode: number; nlink: number }): boolean =>
  entry.isFile() && !entry.isSymbolicLink() && isPrivateMode(entry.mode) && entry.nlink === 1;
const sameIdentity = (left: Identity, right: Identity): boolean => left.dev === right.dev && left.ino === right.ino;

async function identity(path: string): Promise<Identity | undefined> {
  try {
    const entry = await lstat(path);
    if (!isPrivateRegular(entry)) throw new UnsafeFileError(path);
    return { dev: entry.dev, ino: entry.ino };
  } catch (error) {
    if ((error as NodeJS.ErrnoException).code === "ENOENT") return undefined;
    throw error;
  }
}
async function validateHandle(path: string, handle: Handle, expected?: Identity): Promise<Identity> {
  const entry = await handle.stat();
  const found = { dev: entry.dev, ino: entry.ino };
  if (!isPrivateRegular(entry) || (expected && !sameIdentity(expected, found))) throw new UnsafeFileError(path);
  return found;
}
async function openExisting(path: string, flags: number): Promise<Handle> {
  const before = await identity(path);
  if (!before) throw new UnsafeFileError(path);
  const existingFlags = flags & ~(constants.O_CREAT | constants.O_EXCL);
  let handle: Handle | undefined;
  try {
    handle = await open(path, noFollow(existingFlags));
    await validateHandle(path, handle, before);
    return handle;
  } catch (error) {
    await handle?.close();
    throw error;
  }
}
async function createExclusiveHandle(path: string, flags: number): Promise<Handle | undefined> {
  let handle: Handle | undefined;
  try {
    handle = await open(path, noFollow(flags | constants.O_CREAT | constants.O_EXCL), 0o600);
    await validateHandle(path, handle);
    return handle;
  } catch (error) {
    await handle?.close();
    if ((error as NodeJS.ErrnoException).code === "EEXIST") return undefined;
    throw error;
  }
}
async function openRegular(path: string, flags: number, create: boolean): Promise<Handle> {
  if (await identity(path)) return openExisting(path, flags);
  if (!create) throw new UnsafeFileError(path);
  return (await createExclusiveHandle(path, flags)) ?? openExisting(path, flags);
}
async function isPrivateDirectoryGuarded(path: string): Promise<boolean> {
  try {
    const entry = await lstat(path);
    return entry.isDirectory() && !entry.isSymbolicLink() && isPrivateMode(entry.mode);
  } catch {
    return false;
  }
}
async function guardedEnsureDirectory(path: string): Promise<void> {
  if (await isPrivateDirectoryGuarded(path)) return;
  try {
    await mkdir(path, { recursive: true, mode: 0o700 });
  } catch (error) {
    if ((error as NodeJS.ErrnoException).code !== "EEXIST") throw error;
  }
  if (!(await isPrivateDirectoryGuarded(path))) throw new Error(`Refusing unsafe private directory: ${path}`);
}
async function guardedReadFile(path: string): Promise<string | undefined> {
  if (!(await identity(path))) return undefined;
  const handle = await openRegular(path, constants.O_RDONLY, false);
  try {
    return await handle.readFile("utf8");
  } finally {
    await handle.close();
  }
}
async function guardedWriteFile(path: string, content: string): Promise<void> {
  await guardedEnsureDirectory(dirname(path));
  const temporary = `${path}.tmp-${process.pid}-${randomUUID()}`;
  const file = await openRegular(temporary, constants.O_WRONLY, true);
  try {
    await file.writeFile(content, "utf8");
  } finally {
    await file.close();
  }
  try {
    await rename(temporary, path);
    if (!(await identity(path))) throw new UnsafeFileError(path);
  } catch (error) {
    await unlink(temporary).catch(() => undefined);
    throw error;
  }
}

const guardedFileIo: FileIo = {
  guarded: true,
  ensureDirectory: guardedEnsureDirectory,
  isPrivateDirectory: isPrivateDirectoryGuarded,
  readFile: guardedReadFile,
  writeFile: guardedWriteFile,
  createExclusive: async (path, content) => {
    await guardedEnsureDirectory(dirname(path));
    const handle = await createExclusiveHandle(path, constants.O_WRONLY);
    if (!handle) return false;
    try {
      await handle.writeFile(content, "utf8");
    } finally {
      await handle.close();
    }
    return true;
  },
  removeFile: async (path) => {
    if (!(await identity(path))) return;
    await unlink(path);
  },
  quarantine: async (path, suffix) => {
    if (!(await identity(path))) return;
    await rename(path, `${path}.${suffix}-${Date.now()}-${randomUUID()}`);
  },
  ageMs: async (path) => (await lstat(path).then((entry) => Date.now() - entry.mtimeMs).catch(() => 0)),
};

export function createFileIo(guarded: boolean): FileIo {
  return guarded ? guardedFileIo : plainFileIo;
}

export async function removeDirectory(path: string): Promise<void> {
  await rm(path, { recursive: true, force: true });
}

export { isRecord };
