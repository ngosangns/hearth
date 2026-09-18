import { join } from "node:path";

import { lockOwnershipProofName, lockReleaseMarkerName, ownershipKeyName, staleLockMarkerName } from "./state";

export { lockOwnershipProofName, lockReleaseMarkerName, ownershipKeyName, staleLockMarkerName };

/** Default runtime directory, relative to a catalog's root, when `ServiceCatalog.runtimeDirectory`
 * is not set. */
export const defaultRuntimeDirectoryName = ".local-services/runtime-v1";

export function resolveRuntimeDirectory(root: string, catalogRuntimeDirectory?: string): string {
  return join(root, catalogRuntimeDirectory ?? defaultRuntimeDirectoryName);
}

export function lockDir(runtimeDirectory: string): string {
  return join(runtimeDirectory, "manager.lock");
}

export function tokenPath(runtimeDirectory: string): string {
  return join(lockDir(runtimeDirectory), "token");
}

export function metadataPath(runtimeDirectory: string): string {
  return join(lockDir(runtimeDirectory), "metadata.json");
}

export function proofPath(runtimeDirectory: string): string {
  return join(lockDir(runtimeDirectory), lockOwnershipProofName);
}

export function ownershipKeyPath(runtimeDirectory: string): string {
  return join(runtimeDirectory, ownershipKeyName);
}

export function statePath(runtimeDirectory: string): string {
  return join(runtimeDirectory, "state.json");
}

export function logsDir(runtimeDirectory: string): string {
  return join(runtimeDirectory, "logs");
}

export function logPath(runtimeDirectory: string, serviceId: string): string {
  return join(logsDir(runtimeDirectory), `${serviceId}.log`);
}

export function rawLogPath(runtimeDirectory: string, serviceId: string): string {
  return join(logsDir(runtimeDirectory), `${serviceId}.raw`);
}

export function logStreamStatePath(runtimeDirectory: string): string {
  return join(logsDir(runtimeDirectory), "streams.json");
}
