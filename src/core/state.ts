import type { ServiceId } from "./catalog";

/** Bumped on any breaking change to the daemon/TUI/MCP wire protocol or persisted-state shape. A
 * daemon and a client built against different `PROTOCOL_VERSION`s refuse to talk to each other
 * (see `LocalServicesManager`'s `x-local-services-protocol` header check) rather than silently
 * misbehaving. The package's own semver major tracks this: a protocol bump is a major release. */
export const PROTOCOL_VERSION = 1;

/** Persisted `state.json` schema version, owned by this package as the single source of truth —
 * closes the class of drift where two hand-forked copies of this tool disagreed on their state
 * schema version without either side noticing. */
export const STATE_VERSION = 1;

export type DesiredServiceState = "stopped" | "running";
export type ActualServiceState = "stopped" | "queued-start" | "preparing" | "starting" | "running" | "running-unready" | "ready" | "stopping" | "failed" | "orphaned" | "externally-owned";
export type ServiceReadiness = "unknown" | "not-ready" | "ready" | "failed";
export type ServiceOperationKind = "start" | "stop" | "restart" | "status";
export type OperationStatus = "queued" | "running" | "succeeded" | "failed";
export type ReadinessKind = "process" | "tcp" | "http" | "container" | "tailnet" | "custom";

export type PosixProcessIdentity = {
  managerInstanceId: string;
  serviceId: ServiceId;
  generation: number;
  pid: number;
  pgid: number;
  startedAt: string;
  startIdentity: string;
  commandFingerprint: string;
};
export type DockerContainerIdentity = {
  managerInstanceId: string;
  serviceId: ServiceId;
  generation: number;
  startedAt: string;
  commandFingerprint: string;
  containerName: string;
  containerId: string;
  containerStartedAt: string;
};
export type ProcessIdentity = PosixProcessIdentity | DockerContainerIdentity;

export type ServiceLifecycleState = {
  serviceId: ServiceId;
  desiredState: DesiredServiceState;
  actualState: ActualServiceState;
  readiness: ServiceReadiness;
  generation: number;
  identity?: ProcessIdentity;
  readinessKind?: ReadinessKind;
  readinessDetail?: string;
  createdAt: string;
  updatedAt: string;
  exitedAt?: string;
  exitCode?: number;
  error?: string;
  currentOperationId?: string;
};

export type OperationTraceEntry = { at: string; message: string };
export type Operation = {
  id: string;
  requestId: string;
  kind: "service" | "bulk-start" | "manager-shutdown";
  serviceId?: ServiceId;
  targetServiceIds?: readonly ServiceId[];
  action?: ServiceOperationKind;
  status: OperationStatus;
  createdAt: string;
  updatedAt: string;
  trace: OperationTraceEntry[];
  error?: { code: string; message: string };
};

export type ManagerEvent = { sequence: number; at: string; type: string; data: Record<string, unknown> };
export type ManagerMetadata = { version: number; protocolVersion: number; instanceId: string; pid: number; port: number; startedAt: string };
export type ManagerInfo = Omit<ManagerMetadata, "version"> & { metadataVersion: number; runtimeDirectory: string };
export type PersistedManagerState = { version: number; services: Record<string, ServiceLifecycleState> };
export type LogSlice = { serviceId: ServiceId; generation: number; cursor: number; nextCursor: number; data: string; reset: boolean; truncated: boolean };

export const staleLockMarkerName = "quarantine.json";
export const lockReleaseMarkerName = "releasing.json";
export const ownershipKeyName = "ownership.key";
export const lockOwnershipProofName = "ownership.json";
export type LockOwnershipProof = { version: 1; metadata: ManagerMetadata; tokenDigest: string; signature: string };
export type StaleLockMarker = { version: 1; action: "stale-lock"; original: ManagerMetadata; proof: LockOwnershipProof };
