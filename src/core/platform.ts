export type LocalServicesPlatform = "darwin" | "linux" | (string & {});

export const isSupportedLocalServicesPlatform = (platform: LocalServicesPlatform = process.platform): boolean => platform === "darwin" || platform === "linux";

export const unsupportedPlatformMessage = (platform: LocalServicesPlatform): string => `Local services manager supports macOS and Linux only; ${platform} is unsupported`;

export const requireSupportedLocalServicesPlatform = (platform: LocalServicesPlatform = process.platform): void => {
  if (!isSupportedLocalServicesPlatform(platform)) throw new Error(unsupportedPlatformMessage(platform));
};

/** True when `pid` is alive (including "alive but owned by someone else", `EPERM`) — never confuse a
 * denied signal with a dead process. Used by the lock-claim protocol to avoid stealing a lock from a
 * live-but-slow-to-answer-healthchecks manager (see `claimLock` in `manager.ts`): a production incident
 * (263 concurrent daemons under load 425) was caused by treating a health-check timeout as proof of
 * death instead of checking the PID directly. */
export const isPidAlive = (pid: number): boolean => {
  if (!Number.isInteger(pid) || pid <= 0) return false;
  try {
    process.kill(pid, 0);
    return true;
  } catch (error) {
    return (error as NodeJS.ErrnoException).code === "EPERM";
  }
};
