import { appendFileSync, mkdirSync, readFileSync, renameSync, rmSync, statSync } from "node:fs";
import { join } from "node:path";

import { LocalServicesManager, ManagerAlreadyRunningError, type LocalServicesManagerOptions } from "./manager";
import { metadataPath, resolveRuntimeDirectory } from "./paths";
import { requireSupportedLocalServicesPlatform, type LocalServicesPlatform } from "./platform";

type ShutdownManager = Pick<LocalServicesManager, "shutdown" | "shutdownCompletion">;

const daemonLogMaxBytes = 512 * 1024;
const daemonLogName = "daemon.log";
const lockWatchIntervalMs = 2_000;

export type DaemonLog = (message: string) => void;

const describe = (value: unknown): string => (value instanceof Error ? (value.stack ?? value.message) : String(value));

/** Daemon-level diagnostics sink. Whoever spawns a daemon usually detaches it with stdio ignored, so
 * without this file a crash (or a refused duplicate) leaves no trace at all. Keeps one rotated copy. */
export function createDaemonLog(runtimeDirectory: string): DaemonLog {
  const path = join(runtimeDirectory, daemonLogName);
  const rotated = `${path}.1`;
  return (message) => {
    try {
      mkdirSync(runtimeDirectory, { recursive: true, mode: 0o700 });
      if ((statSync(path, { throwIfNoEntry: false })?.size ?? 0) > daemonLogMaxBytes) {
        rmSync(rotated, { force: true });
        renameSync(path, rotated);
      }
      appendFileSync(path, `${new Date().toISOString()} ${message}\n`, { mode: 0o600 });
    } catch {}
  };
}

/** The lock's owner instance id, `""` when the lock file is gone (quarantined or released) and
 * `undefined` when it exists but could not be read — a transient read failure must not be mistaken
 * for losing the lock. */
export function readLockInstanceId(runtimeDirectory: string): string | undefined {
  try {
    const parsed = JSON.parse(readFileSync(metadataPath(runtimeDirectory), "utf8")) as { instanceId?: unknown };
    return typeof parsed.instanceId === "string" ? parsed.instanceId : "";
  } catch (error) {
    return (error as NodeJS.ErrnoException).code === "ENOENT" ? "" : undefined;
  }
}

/** Keeps the "one daemon per runtime directory" invariant enforced from the losing side. A daemon
 * whose lock was taken over must stop managing services: two live daemons would otherwise fight over
 * one state file, each terminating services the other still believes it owns (and a service's
 * identity carries the instance id of the daemon that spawned it, so ownership checks flap). It never
 * touches the winner's lock — it just leaves the field. */
export class LockOwnershipWatch {
  private timer: NodeJS.Timeout | undefined;
  private stopped = false;
  constructor(
    private readonly runtimeDirectory: string,
    private readonly instanceId: string,
    private readonly onLockLost: () => void,
    private readonly intervalMs = lockWatchIntervalMs,
  ) {}
  start(): void {
    if (this.stopped) return;
    this.timer ??= setInterval(() => this.check(), this.intervalMs);
  }
  stop(): void {
    this.stopped = true;
    clearInterval(this.timer);
    this.timer = undefined;
  }
  check(): void {
    if (this.stopped) return;
    const owner = readLockInstanceId(this.runtimeDirectory);
    // `undefined` means the lock file exists but could not be read — unknown, not lost.
    if (owner === undefined || owner === this.instanceId) return;
    this.stop();
    this.onLockLost();
  }
}

export class DaemonLifecycle {
  private shutdownPromise: Promise<void> | undefined;

  constructor(
    private readonly manager: ShutdownManager,
    private readonly signalMode: "refuse-if-active" | "stop-services" = "refuse-if-active",
  ) {}

  /** Triggered by SIGINT/SIGTERM. Default mode leaves already-running services alone — they're
   * detached processes that outlive this daemon and get reconciled/re-adopted by the next one — so
   * interrupting the daemon never kills a developer's in-flight work. */
  shutdown(): Promise<void> {
    this.shutdownPromise ??= this.manager.shutdown(this.signalMode);
    return this.shutdownPromise;
  }
  waitForManagerShutdown(): Promise<void> {
    return this.manager.shutdownCompletion;
  }
}

export async function terminateAfterManagerShutdown(lifecycle: DaemonLifecycle, terminate: (exitCode: number) => void): Promise<void> {
  try {
    await lifecycle.waitForManagerShutdown();
    terminate(0);
  } catch {
    terminate(1);
  }
}

/** Boots a `LocalServicesManager`, wires SIGINT/SIGTERM to a graceful `DaemonLifecycle` shutdown, and
 * resolves once the manager has fully closed. A raced `ManagerAlreadyRunningError` (another daemon won
 * the lock claim first) is treated as a clean, silent exit rather than an unhandled rejection — this
 * is the daemon-entrypoint robustness gap infra's copy of this tool had (no try/catch around
 * `bootstrap()`, so this exact error class crashed the process instead of exiting quietly).
 *
 * Stray errors are logged, never fatal: Bun terminates a process on an unhandled rejection, and a
 * daemon that dies takes the ownership of every service it manages with it (they survive as orphans
 * tied to its instance id, so the next daemon can only adopt them, not restart them). */
export async function runDaemon(options: LocalServicesManagerOptions, platform: LocalServicesPlatform = process.platform, signalMode: "refuse-if-active" | "stop-services" = "refuse-if-active"): Promise<void> {
  requireSupportedLocalServicesPlatform(platform);
  const runtimeDirectory = options.runtimeDirectory ?? resolveRuntimeDirectory(options.root ?? process.cwd(), options.catalog.runtimeDirectory);
  const log = createDaemonLog(runtimeDirectory);
  process.on("unhandledRejection", (reason) => log(`unhandled rejection: ${describe(reason)}`));
  process.on("uncaughtException", (error) => log(`uncaught exception: ${describe(error)}`));

  let manager: LocalServicesManager;
  try {
    manager = await LocalServicesManager.bootstrap(options);
  } catch (error) {
    if (error instanceof ManagerAlreadyRunningError) {
      log(`bootstrap raced another daemon: ${describe(error)}`);
      return;
    }
    log(`bootstrap failed: ${describe(error)}`);
    throw error;
  }
  log(`listening on 127.0.0.1:${manager.info.port}, root=${options.root ?? process.cwd()}`);

  const lifecycle = new DaemonLifecycle(manager, signalMode);
  let lockWatch: LockOwnershipWatch | undefined;
  if (readLockInstanceId(runtimeDirectory) === manager.instanceId) {
    lockWatch = new LockOwnershipWatch(runtimeDirectory, manager.instanceId, () => {
      log("shutdown: manager lock is owned by another daemon");
      void lifecycle.shutdown();
    });
    lockWatch.start();
  }
  const shutdown = (): void => {
    lockWatch?.stop();
    log("shutdown: signal");
    void lifecycle.shutdown();
  };
  process.once("SIGINT", shutdown);
  process.once("SIGTERM", shutdown);
  // Bun.spawn(detached:true) already runs this daemon in its own session with no controlling
  // terminal, so a terminal hangup should never reach it — but an explicit ignore means a stray
  // SIGHUP can never fall back to Node/Bun's default terminate-the-process behavior either.
  process.on("SIGHUP", () => {});
  await terminateAfterManagerShutdown(lifecycle, (exitCode) => process.exit(exitCode));
}
