import { LocalServicesManager, ManagerAlreadyRunningError, type LocalServicesManagerOptions } from "./manager";
import { requireSupportedLocalServicesPlatform, type LocalServicesPlatform } from "./platform";

type ShutdownManager = Pick<LocalServicesManager, "shutdown" | "shutdownCompletion">;

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
 * `bootstrap()`, so this exact error class crashed the process instead of exiting quietly). */
export async function runDaemon(options: LocalServicesManagerOptions, platform: LocalServicesPlatform = process.platform, signalMode: "refuse-if-active" | "stop-services" = "refuse-if-active"): Promise<void> {
  requireSupportedLocalServicesPlatform(platform);
  let manager: LocalServicesManager;
  try {
    manager = await LocalServicesManager.bootstrap(options);
  } catch (error) {
    if (error instanceof ManagerAlreadyRunningError) return;
    throw error;
  }
  const lifecycle = new DaemonLifecycle(manager, signalMode);
  const shutdown = (): void => {
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
