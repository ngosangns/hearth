import { createHash, createHmac, randomBytes, randomUUID, timingSafeEqual } from "node:crypto";
import { open, readdir, rename, stat } from "node:fs/promises";
import { join } from "node:path";

import { dependencyLevels, validateCatalog, type ServiceCatalog, type ServiceId } from "./catalog";
import { createFileIo, isRecord, removeDirectory, type FileIo } from "./file-io";
import { resolveRuntimeDirectory } from "./paths";
import { isPidAlive, requireSupportedLocalServicesPlatform, type LocalServicesPlatform } from "./platform";
import { activeStates, defaultSupervisorOptions, ProcessSupervisor, type SupervisorOptions } from "./supervisor";
import {
  lockOwnershipProofName,
  lockReleaseMarkerName,
  ownershipKeyName,
  PROTOCOL_VERSION,
  STATE_VERSION,
  staleLockMarkerName,
  type ActualServiceState,
  type LogSlice,
  type ManagerEvent,
  type ManagerInfo,
  type ManagerMetadata,
  type Operation,
  type OperationStatus,
  type OperationTraceEntry,
  type PersistedManagerState,
  type ProcessIdentity,
  type ServiceLifecycleState,
  type ServiceOperationKind,
} from "./state";

export const managerProtocolVersion = PROTOCOL_VERSION;
const managerMetadataVersion = 1;
const logStreamStateVersion = 1;
const defaultEventCapacity = 256;
const defaultLogTailBytes = 16 * 1024;
const defaultLogMaxBytes = 256 * 1024;
const defaultLogRotationCount = 2;
const managerStartupGraceMs = 5_000;
const maxSseQueueFrames = 64;
const maxSseQueueBytes = 64 * 1024;
const liveManagerHealthcheckTimeoutMs = 1_500;

export type LockOwnershipProof = { version: 1; metadata: ManagerMetadata; tokenDigest: string; signature: string };
export type StaleLockMarker = { version: 1; action: "stale-lock"; original: ManagerMetadata; proof: LockOwnershipProof };
type LockReleaseMarker = { version: 1; action: "release-lock"; instanceId: string; tokenDigest: string };
type LockHandle = { path: string; metadataPath: string; tokenPath: string; proofPath: string; ownershipKeyPath: string; instanceId: string };
type LogStreamState = { version: number; generations: Record<string, number> };
type LogRotationJournal = { version: number; serviceId: ServiceId; fromGeneration: number; toGeneration: number; phase: "pending" | "committed" };
export type OwnedLockArtifacts = { metadata: ManagerMetadata; token: string; proof: LockOwnershipProof };
export type CursorLogStoreHooks = { onRotationStep?: (step: "pending" | "renamed" | "committed" | "published") => Promise<void>; onRecoveryStep?: (step: "before-publish" | "published") => Promise<void> };

export class ManagerAlreadyRunningError extends Error {
  constructor(readonly metadata: ManagerMetadata) {
    super(`Local services manager is already running on port ${metadata.port}`);
    this.name = "ManagerAlreadyRunningError";
  }
}
export class ManagerHttpError extends Error {
  constructor(
    readonly status: number,
    readonly code: string,
    message: string,
  ) {
    super(message);
    this.name = "ManagerHttpError";
  }
}

class AsyncSerial {
  private tail = Promise.resolve();
  async run<T>(work: () => Promise<T>): Promise<T> {
    const next = this.tail.catch(() => undefined).then(work);
    this.tail = next.then(
      () => undefined,
      () => undefined,
    );
    return next;
  }
}

const now = (): string => new Date().toISOString();
const sameServiceIds = (left: readonly ServiceId[] | undefined, right: readonly ServiceId[] | undefined): boolean =>
  left === right || (left !== undefined && right !== undefined && left.length === right.length && left.every((id, index) => id === right[index]));
const hasExactKeys = (value: Record<string, unknown>, required: readonly string[], optional: readonly string[] = []): boolean =>
  required.every((key) => key in value) && Object.keys(value).every((key) => required.includes(key) || optional.includes(key));
const isFiniteInteger = (value: unknown, minimum = Number.MIN_SAFE_INTEGER): value is number => typeof value === "number" && Number.isSafeInteger(value) && value >= minimum;
const isTimestamp = (value: unknown): value is string => typeof value === "string" && Number.isFinite(Date.parse(value)) && new Date(value).toISOString() === value;

async function readJson(io: FileIo, path: string): Promise<unknown | undefined> {
  const raw = await io.readFile(path);
  return raw === undefined ? undefined : JSON.parse(raw);
}

// -------------------------------------------------------------------------------------------
// Lock ownership proofs
// -------------------------------------------------------------------------------------------

const isManagerMetadata = (value: unknown): value is ManagerMetadata =>
  isRecord(value) &&
  hasExactKeys(value, ["version", "protocolVersion", "instanceId", "pid", "port", "startedAt"]) &&
  value.version === managerMetadataVersion &&
  isFiniteInteger(value.protocolVersion, 1) &&
  typeof value.instanceId === "string" &&
  isFiniteInteger(value.pid, 1) &&
  isFiniteInteger(value.port, 0) &&
  isTimestamp(value.startedAt);

const ownershipPayload = (metadata: ManagerMetadata, token: string): string =>
  JSON.stringify({
    metadata: { instanceId: metadata.instanceId, pid: metadata.pid, port: metadata.port, protocolVersion: metadata.protocolVersion, startedAt: metadata.startedAt, version: metadata.version },
    tokenDigest: createHash("sha256").update(token).digest("hex"),
  });
const ownershipSignature = (key: string, metadata: ManagerMetadata, token: string): string => createHmac("sha256", key).update(ownershipPayload(metadata, token)).digest("hex");
const isOwnershipProof = (value: unknown): value is LockOwnershipProof =>
  isRecord(value) &&
  hasExactKeys(value, ["version", "metadata", "tokenDigest", "signature"]) &&
  value.version === 1 &&
  isManagerMetadata(value.metadata) &&
  typeof value.tokenDigest === "string" &&
  /^[a-f0-9]{64}$/.test(value.tokenDigest) &&
  typeof value.signature === "string" &&
  /^[a-f0-9]{64}$/.test(value.signature);

export const createLockOwnershipProof = (key: string, metadata: ManagerMetadata, token: string): LockOwnershipProof => ({
  version: 1,
  metadata,
  tokenDigest: createHash("sha256").update(token).digest("hex"),
  signature: ownershipSignature(key, metadata, token),
});
export const verifyLockOwnershipProof = (key: string | undefined, metadata: ManagerMetadata, token: string, proof: unknown): proof is LockOwnershipProof => {
  if (!key || !isOwnershipProof(proof) || JSON.stringify(proof.metadata) !== JSON.stringify(metadata)) return false;
  const expectedDigest = createHash("sha256").update(token).digest("hex");
  const expectedSignature = ownershipSignature(key, metadata, token);
  return timingSafeEqual(Buffer.from(proof.tokenDigest), Buffer.from(expectedDigest)) && timingSafeEqual(Buffer.from(proof.signature), Buffer.from(expectedSignature));
};
export const isStaleLockMarker = (value: unknown, key: string | undefined, metadata: ManagerMetadata, token: string, proof: unknown): value is StaleLockMarker => {
  if (!isRecord(value) || !hasExactKeys(value, ["version", "action", "original", "proof"]) || value.version !== 1 || value.action !== "stale-lock" || !isManagerMetadata(value.original) || !isOwnershipProof(value.proof)) return false;
  return JSON.stringify(value.original) === JSON.stringify(metadata) && JSON.stringify(value.proof) === JSON.stringify(proof) && verifyLockOwnershipProof(key, metadata, token, proof);
};

async function ownershipKey(io: FileIo, runtimeDirectory: string): Promise<string> {
  await io.ensureDirectory(runtimeDirectory);
  const path = join(runtimeDirectory, ownershipKeyName);
  const existing = await io.readFile(path);
  if (existing !== undefined) {
    const trimmed = existing.trim();
    if (!trimmed) throw new Error("Refusing empty ownership key");
    return trimmed;
  }
  const generated = randomBytes(32).toString("base64url");
  if (await io.createExclusive(path, generated)) return generated;
  const concurrent = (await io.readFile(path))?.trim();
  if (!concurrent) throw new Error("Refusing unsafe ownership key");
  return concurrent;
}
export async function readLockOwnershipKey(io: FileIo, runtimeDirectory: string): Promise<string | undefined> {
  const key = await io.readFile(join(runtimeDirectory, ownershipKeyName));
  return key?.trim() || undefined;
}
export async function readOwnedLockArtifacts(io: FileIo, path: string): Promise<OwnedLockArtifacts | undefined> {
  try {
    const [rawMetadata, rawToken, rawProof] = await Promise.all([io.readFile(join(path, "metadata.json")), io.readFile(join(path, "token")), io.readFile(join(path, lockOwnershipProofName))]);
    if (rawMetadata === undefined || rawToken === undefined || rawProof === undefined) return undefined;
    const metadata = JSON.parse(rawMetadata);
    const proof = JSON.parse(rawProof);
    return isManagerMetadata(metadata) && rawToken.trim() && isOwnershipProof(proof) ? { metadata, token: rawToken.trim(), proof } : undefined;
  } catch {
    return undefined;
  }
}
const liveManagerHealthcheckAttempts = 2;
const liveManagerHealthcheckRetryDelayMs = 150;
/** Liveness is the one question this protocol must never answer with a false negative: a live daemon
 * that merely failed to answer in time gets its lock (and therefore its whole service pool) taken
 * over, which is what produced a daemon storm. The PID check in `claimLock` is the primary guard;
 * the retry here keeps a single slow/blocked response from being read as "no manager". */
async function isLiveManager(metadata: ManagerMetadata, token: string): Promise<boolean> {
  if (!token || metadata.port === 0) return false;
  for (let attempt = 0; attempt < liveManagerHealthcheckAttempts; attempt++) {
    if (await managerAnswersHealthcheck(metadata, token)) return true;
    if (attempt + 1 < liveManagerHealthcheckAttempts) await Bun.sleep(liveManagerHealthcheckRetryDelayMs);
  }
  return false;
}
async function managerAnswersHealthcheck(metadata: ManagerMetadata, token: string): Promise<boolean> {
  try {
    const response = await fetch(`http://127.0.0.1:${metadata.port}/healthz`, { headers: { authorization: `Bearer ${token}` }, signal: AbortSignal.timeout(liveManagerHealthcheckTimeoutMs) });
    const body = (await response.json()) as Record<string, unknown>;
    return response.ok && body.instanceId === metadata.instanceId && body.protocolVersion === metadata.protocolVersion;
  } catch {
    return false;
  }
}
const releaseMarker = (instanceId: string, token: string): LockReleaseMarker => ({ version: 1, action: "release-lock", instanceId, tokenDigest: createHash("sha256").update(token).digest("hex") });
const isReleaseMarker = (raw: string | undefined, instanceId: string, token: string): boolean => {
  if (raw === undefined) return false;
  try {
    const value = JSON.parse(raw) as unknown;
    return isRecord(value) && hasExactKeys(value, ["version", "action", "instanceId", "tokenDigest"]) && value.version === 1 && value.action === "release-lock" && value.instanceId === instanceId && value.tokenDigest === createHash("sha256").update(token).digest("hex");
  } catch {
    return false;
  }
};
async function prepareOwnedLockRelease(io: FileIo, lock: LockHandle, token: string): Promise<boolean> {
  const artifacts = await readOwnedLockArtifacts(io, lock.path);
  if (!artifacts || artifacts.metadata.instanceId !== lock.instanceId || artifacts.token !== token) return false;
  await io.writeFile(join(lock.path, lockReleaseMarkerName), JSON.stringify(releaseMarker(lock.instanceId, token)));
  return true;
}
async function quarantineStaleLock(io: FileIo, path: string, runtimeDirectory: string): Promise<void> {
  const artifacts = await readOwnedLockArtifacts(io, path);
  const key = await readLockOwnershipKey(io, runtimeDirectory);
  if (!artifacts || !verifyLockOwnershipProof(key, artifacts.metadata, artifacts.token, artifacts.proof)) throw new Error("Refusing unsafe or unowned manager lock");
  const quarantined = `${path}.stale-${Date.now()}-${randomUUID()}`;
  await rename(path, quarantined);
  await io.writeFile(join(quarantined, staleLockMarkerName), JSON.stringify({ version: 1, action: "stale-lock", original: artifacts.metadata, proof: artifacts.proof } satisfies StaleLockMarker));
}
async function releaseOwnedLock(io: FileIo, lock: LockHandle, token: string): Promise<void> {
  const artifacts = await readOwnedLockArtifacts(io, lock.path);
  const marker = await io.readFile(join(lock.path, lockReleaseMarkerName));
  if (!artifacts || artifacts.metadata.instanceId !== lock.instanceId || artifacts.token !== token || !isReleaseMarker(marker, lock.instanceId, token)) return;
  await removeDirectory(lock.path);
}

/** The lock-claim protocol. Never steals a lock just because a health check timed out or errored —
 * only a confirmed-dead PID (or an explicit release marker) makes a lock stale. A production incident
 * (263 concurrent daemons, load 425) was caused by treating a health-check timeout as proof of death. */
async function claimLock(io: FileIo, runtimeDirectory: string, bootstrapMetadata: ManagerMetadata, token: string): Promise<LockHandle> {
  const path = join(runtimeDirectory, "manager.lock");
  const managerMetadataPath = join(path, "metadata.json");
  const managerTokenPath = join(path, "token");
  const managerProofPath = join(path, lockOwnershipProofName);
  const managerOwnershipKeyPath = join(runtimeDirectory, ownershipKeyName);
  await io.ensureDirectory(runtimeDirectory);
  for (;;) {
    if (await io.createExclusive(managerMetadataPath, JSON.stringify(bootstrapMetadata))) {
      const key = await ownershipKey(io, runtimeDirectory);
      await io.writeFile(managerTokenPath, token);
      await io.writeFile(managerProofPath, JSON.stringify(createLockOwnershipProof(key, bootstrapMetadata, token)));
      return { path, metadataPath: managerMetadataPath, tokenPath: managerTokenPath, proofPath: managerProofPath, ownershipKeyPath: managerOwnershipKeyPath, instanceId: bootstrapMetadata.instanceId };
    }
    const artifacts = await readOwnedLockArtifacts(io, path);
    const age = await io.ageMs(path);
    if (!artifacts) {
      if ((await io.isPrivateDirectory(path)) && age < managerStartupGraceMs) {
        await Bun.sleep(25);
        continue;
      }
      throw new Error("Refusing unsafe or malformed manager lock");
    }
    if (artifacts.metadata.port === 0 && (Date.now() - Date.parse(artifacts.metadata.startedAt) < managerStartupGraceMs || isPidAlive(artifacts.metadata.pid))) {
      await Bun.sleep(25);
      continue;
    }
    if (await isLiveManager(artifacts.metadata, artifacts.token)) throw new ManagerAlreadyRunningError(artifacts.metadata);
    if (isPidAlive(artifacts.metadata.pid)) {
      await Bun.sleep(100);
      continue;
    }
    if (isReleaseMarker(await io.readFile(join(path, lockReleaseMarkerName)), artifacts.metadata.instanceId, artifacts.token) && age < managerStartupGraceMs) {
      await Bun.sleep(25);
      continue;
    }
    await quarantineStaleLock(io, path, runtimeDirectory);
  }
}

// -------------------------------------------------------------------------------------------
// State / event / operation / log stores
// -------------------------------------------------------------------------------------------

const isLifecycleState = (value: unknown, serviceKey: string): value is ServiceLifecycleState => {
  if (
    !isRecord(value) ||
    !hasExactKeys(value, ["serviceId", "desiredState", "actualState", "readiness", "generation", "createdAt", "updatedAt"], ["identity", "readinessKind", "readinessDetail", "exitedAt", "exitCode", "error", "currentOperationId"])
  )
    return false;
  if (
    value.serviceId !== serviceKey ||
    (value.desiredState !== "stopped" && value.desiredState !== "running") ||
    !["stopped", "queued-start", "preparing", "starting", "running", "running-unready", "ready", "stopping", "failed", "orphaned", "externally-owned"].includes(value.actualState as string) ||
    !["unknown", "not-ready", "ready", "failed"].includes(value.readiness as string) ||
    !isFiniteInteger(value.generation, 0) ||
    !isTimestamp(value.createdAt) ||
    !isTimestamp(value.updatedAt)
  )
    return false;
  if (value.exitedAt !== undefined && !isTimestamp(value.exitedAt)) return false;
  if (value.exitCode !== undefined && !isFiniteInteger(value.exitCode)) return false;
  if (value.error !== undefined && typeof value.error !== "string") return false;
  if (value.currentOperationId !== undefined && typeof value.currentOperationId !== "string") return false;
  if (value.readinessDetail !== undefined && typeof value.readinessDetail !== "string") return false;
  return true;
};
const isPersistedManagerState = (value: unknown): value is PersistedManagerState =>
  isRecord(value) && hasExactKeys(value, ["version", "services"]) && value.version === STATE_VERSION && isRecord(value.services) && Object.entries(value.services).every(([key, state]) => isLifecycleState(state, key));

const isProcessIdentityShape = (value: unknown): value is ProcessIdentity => {
  if (
    !isRecord(value) ||
    typeof value.managerInstanceId !== "string" ||
    typeof value.serviceId !== "string" ||
    typeof value.startedAt !== "string" ||
    typeof value.commandFingerprint !== "string" ||
    !isFiniteInteger(value.generation, 0)
  )
    return false;
  if ("containerId" in value) return typeof value.containerId === "string" && typeof value.containerName === "string" && typeof value.containerStartedAt === "string";
  return isFiniteInteger(value.pid, 0) && isFiniteInteger(value.pgid, 0) && typeof value.startIdentity === "string";
};

const migrateLegacyIdentity = (identity: Record<string, unknown>, serviceId: string): ProcessIdentity | undefined => {
  const { unitId: _legacyUnitId, ...rest } = identity;
  const candidate = { ...rest, serviceId };
  return isProcessIdentityShape(candidate) ? candidate : undefined;
};

/** A state file written by either predecessor copy of this tool (infra's `scripts/local-services-tui`,
 * viclass's `tools/local-services-tui`) keys its services under `units` and names them `unitId`.
 * Reading it matters at the moment a consumer switches onto this package: without the rewrite the file
 * fails validation, gets quarantined, and every service that is *still running* reads as stopped — and
 * a later start then refuses the port those orphaned processes hold. `serviceId` has to be rewritten
 * into the identity too, or ownership checks fail and the process is written off as a stranger. */
const migrateLegacyPersistedState = (value: unknown): PersistedManagerState | undefined => {
  if (!isRecord(value) || !hasExactKeys(value, ["version", "units"]) || !isRecord(value.units)) return undefined;
  const services: Record<string, ServiceLifecycleState> = {};
  for (const [serviceId, unit] of Object.entries(value.units)) {
    if (!isRecord(unit)) return undefined;
    const { unitId, identity, ...rest } = unit as { unitId?: unknown; identity?: unknown };
    const migratedIdentity = isRecord(identity) && typeof identity.unitId === "string" ? migrateLegacyIdentity(identity, identity.unitId) : undefined;
    const candidate = { ...rest, serviceId, ...(migratedIdentity !== undefined ? { identity: migratedIdentity } : {}) };
    if (typeof unitId !== "string" || !isLifecycleState(candidate, serviceId)) return undefined;
    services[serviceId] = candidate;
  }
  return { version: STATE_VERSION, services };
};

export class AtomicStateStore {
  readonly path: string;
  constructor(
    private readonly io: FileIo,
    runtimeDirectory: string,
  ) {
    this.path = join(runtimeDirectory, "state.json");
  }
  async load(): Promise<PersistedManagerState> {
    try {
      const loaded = await readJson(this.io, this.path);
      if (loaded === undefined) return { version: STATE_VERSION, services: {} };
      if (isPersistedManagerState(loaded)) return loaded;
      const migrated = migrateLegacyPersistedState(loaded);
      if (migrated !== undefined) {
        await this.save(migrated);
        return migrated;
      }
    } catch {}
    await this.io.quarantine(this.path, "corrupt");
    return { version: STATE_VERSION, services: {} };
  }
  async save(state: PersistedManagerState): Promise<void> {
    const next = { ...state, version: STATE_VERSION };
    if (!isPersistedManagerState(next)) throw new Error("Refusing to persist invalid manager state");
    await this.io.writeFile(this.path, JSON.stringify(next));
  }
}

export class ManagerEventStore {
  private nextSequence = 1;
  private readonly events: ManagerEvent[] = [];
  private readonly listeners = new Set<(event: ManagerEvent) => void>();
  constructor(
    private readonly capacity = defaultEventCapacity,
    readonly epoch: string = randomUUID(),
  ) {}
  publish(type: string, data: Record<string, unknown>): ManagerEvent {
    const event = { sequence: this.nextSequence++, at: now(), type, data };
    this.events.push(event);
    if (this.events.length > this.capacity) this.events.splice(0, this.events.length - this.capacity);
    for (const listener of this.listeners) listener(event);
    return event;
  }
  subscribe(listener: (event: ManagerEvent) => void): () => void {
    this.listeners.add(listener);
    return () => this.listeners.delete(listener);
  }
  get subscriberCount(): number {
    return this.listeners.size;
  }
  replay(afterSequence?: number, epoch?: string): { epoch: string; reset: boolean; events: ManagerEvent[]; latestSequence: number } {
    const oldestSequence = this.events[0]?.sequence ?? this.nextSequence;
    const latestSequence = this.nextSequence - 1;
    const reset = (epoch !== undefined && epoch !== this.epoch) || (afterSequence !== undefined && (afterSequence < oldestSequence - 1 || afterSequence > latestSequence));
    return { epoch: this.epoch, reset, events: this.events.filter((event) => reset || afterSequence === undefined || event.sequence > afterSequence), latestSequence };
  }
}

export class OperationScheduler {
  private closing = false;
  private readonly operations = new Map<string, Operation>();
  private readonly requestIds = new Map<string, Operation>();
  private readonly queues = new Map<string, Promise<void>>();
  constructor(private readonly events: ManagerEventStore) {}
  get(id: string): Operation | undefined {
    return this.operations.get(id);
  }
  isQueued(operation: Operation): boolean {
    return this.queues.has(operation.serviceId ?? "__manager__");
  }
  closeMutations(): void {
    this.closing = true;
  }
  async drainServices(): Promise<void> {
    await Promise.all([...this.queues.entries()].filter(([key]) => key !== "__manager__").map(([, queue]) => queue.catch(() => undefined)));
  }
  async wait(operation: Operation): Promise<void> {
    const target = operation.serviceId ?? "__manager__";
    await (this.queues.get(target) ?? Promise.resolve()).catch(() => undefined);
  }
  resolveRequest(input: Omit<Operation, "id" | "status" | "createdAt" | "updatedAt" | "trace" | "error">): Operation | undefined {
    const existing = this.requestIds.get(input.requestId);
    if (existing !== undefined && (existing.kind !== input.kind || existing.serviceId !== input.serviceId || existing.action !== input.action || !sameServiceIds(existing.targetServiceIds, input.targetServiceIds)))
      throw new ManagerHttpError(409, "request_id_conflict", "requestId is already used by a different operation");
    return existing;
  }
  schedule(input: Omit<Operation, "id" | "status" | "createdAt" | "updatedAt" | "trace" | "error">, execute: (operation: Operation) => Promise<void>, rejected?: (operation: Operation) => Promise<void>): Operation {
    const existing = this.resolveRequest(input);
    if (existing) return existing;
    const createdAt = now();
    const operation: Operation = { ...input, id: randomUUID(), status: "queued", createdAt, updatedAt: createdAt, trace: [{ at: createdAt, message: "Operation accepted" }] };
    this.operations.set(operation.id, operation);
    this.requestIds.set(operation.requestId, operation);
    this.events.publish("operation.accepted", { operationId: operation.id, requestId: operation.requestId, serviceId: operation.serviceId ?? null });
    const target = operation.serviceId ?? "__manager__";
    const previous = this.queues.get(target) ?? Promise.resolve();
    const current = previous.catch(() => undefined).then(async () => {
      if (this.closing && (operation.kind === "service" || operation.kind === "bulk-start")) {
        await rejected?.(operation);
        operation.error = { code: "manager_closing", message: "Manager is shutting down" };
        this.transition(operation, "failed", "Operation rejected because manager is shutting down");
        return;
      }
      this.transition(operation, "running", "Operation started");
      try {
        await execute(operation);
        this.transition(operation, "succeeded", "Operation completed");
      } catch (error) {
        operation.error = { code: error instanceof ManagerHttpError ? error.code : "operation_failed", message: error instanceof Error ? error.message : String(error) };
        this.transition(operation, "failed", `Operation failed: ${operation.error.message}`);
      }
    });
    this.queues.set(target, current);
    void current.finally(() => {
      if (this.queues.get(target) === current) this.queues.delete(target);
    });
    return operation;
  }
  trace(operation: Operation, message: string): void {
    operation.updatedAt = now();
    operation.trace.push({ at: operation.updatedAt, message });
    this.events.publish("operation.updated", { operationId: operation.id, status: operation.status, serviceId: operation.serviceId ?? null });
  }
  private transition(operation: Operation, status: OperationStatus, message: string): void {
    operation.status = status;
    operation.updatedAt = now();
    operation.trace.push({ at: operation.updatedAt, message });
    this.events.publish("operation.updated", { operationId: operation.id, status, serviceId: operation.serviceId ?? null });
  }
}

export class CursorLogStore {
  private readonly rotationGenerations = new Map<ServiceId, number>();
  private readonly appendQueues = new Map<ServiceId, AsyncSerial>();
  private readonly metadataSerial = new AsyncSerial();
  private loaded = false;
  private loadPromise: Promise<void> | undefined;
  readonly streamStatePath: string;
  constructor(
    private readonly io: FileIo,
    private readonly directory: string,
    private readonly latestTailBytes = defaultLogTailBytes,
    private readonly maxBytes = defaultLogMaxBytes,
    private readonly rotationCount = defaultLogRotationCount,
    private readonly hooks: CursorLogStoreHooks = {},
  ) {
    this.streamStatePath = join(directory, "streams.json");
  }
  async append(serviceId: ServiceId, data: string): Promise<void> {
    await this.runForService(serviceId, async () => {
      await this.ensureLoaded();
      const encoded = Buffer.from(data, "utf8");
      if (encoded.byteLength > this.maxBytes) throw new RangeError(`Log append exceeds ${this.maxBytes} byte limit`);
      await this.io.ensureDirectory(this.directory);
      const path = this.pathFor(serviceId);
      const currentSize = await this.size(path);
      if (currentSize > 0 && currentSize + encoded.byteLength > this.maxBytes) await this.rotate(serviceId);
      // Appends bypass FileIo (which always rewrites atomically) — an append-in-place is safe here
      // because only this manager instance's serialized queue ever writes this path.
      const handle = await open(path, "a", 0o600);
      try {
        await handle.write(encoded);
      } finally {
        await handle.close();
      }
    });
  }
  async read(serviceId: ServiceId, cursor: number | undefined, limit = this.latestTailBytes, lifecycleGeneration = 0, requestedGeneration?: number): Promise<LogSlice> {
    return this.runForService(serviceId, async () => {
      await this.ensureLoaded();
      const path = this.pathFor(serviceId);
      const size = await this.size(path);
      const safeLimit = Math.max(1, Math.min(limit, this.latestTailBytes));
      const staleGeneration = requestedGeneration !== undefined && requestedGeneration !== lifecycleGeneration;
      const invalidCursor = cursor !== undefined && (cursor < 0 || cursor > size || !(await this.isUtf8Boundary(path, cursor, size)));
      const reset = staleGeneration || invalidCursor;
      const start = reset || cursor === undefined ? await this.tailStart(path, size, safeLimit) : cursor;
      const { data, bytesRead } = start >= size ? { data: "", bytesRead: 0 } : await this.readFramed(path, start, size, safeLimit);
      return { serviceId, generation: lifecycleGeneration, cursor: start, nextCursor: start + bytesRead, data, reset, truncated: start > 0 };
    });
  }
  private runForService<T>(serviceId: ServiceId, work: () => Promise<T>): Promise<T> {
    const serial = this.appendQueues.get(serviceId) ?? new AsyncSerial();
    this.appendQueues.set(serviceId, serial);
    return serial.run(work);
  }
  private async ensureLoaded(): Promise<void> {
    if (this.loaded) return;
    this.loadPromise ??= this.load();
    await this.loadPromise;
  }
  private async load(): Promise<void> {
    try {
      const loaded = await readJson(this.io, this.streamStatePath);
      if (loaded !== undefined && isRecord(loaded) && loaded.version === logStreamStateVersion && isRecord(loaded.generations)) {
        for (const [serviceId, generation] of Object.entries(loaded.generations as Record<string, number>)) this.rotationGenerations.set(serviceId, generation);
      }
    } catch {
      await this.io.quarantine(this.streamStatePath, "corrupt");
    }
    await this.reconcileRotationJournals();
    this.loaded = true;
  }
  private async reconcileRotationJournals(): Promise<void> {
    let changed = false;
    const journalsToDelete: string[] = [];
    let entries: string[];
    try {
      entries = await readdir(this.directory);
    } catch (error) {
      if ((error as NodeJS.ErrnoException).code === "ENOENT") return;
      throw error;
    }
    for (const entry of entries.filter((candidate) => candidate.endsWith(".log.rotation.json"))) {
      const path = join(this.directory, entry);
      try {
        const journal = (await readJson(this.io, path)) as LogRotationJournal | undefined;
        if (!journal || journal.version !== logStreamStateVersion) throw new Error("Invalid log rotation journal");
        const rotatedPath = `${this.pathFor(journal.serviceId)}.${journal.fromGeneration}`;
        const renamed = await this.exists(rotatedPath);
        if (journal.phase === "committed" || renamed) {
          if ((this.rotationGenerations.get(journal.serviceId) ?? 1) < journal.toGeneration) {
            this.rotationGenerations.set(journal.serviceId, journal.toGeneration);
            changed = true;
          }
        }
        journalsToDelete.push(path);
      } catch {
        await this.io.quarantine(path, "corrupt");
      }
    }
    if (changed) {
      await this.hooks.onRecoveryStep?.("before-publish");
      await this.io.writeFile(this.streamStatePath, JSON.stringify({ version: logStreamStateVersion, generations: Object.fromEntries(this.rotationGenerations) } satisfies LogStreamState));
      await this.hooks.onRecoveryStep?.("published");
    }
    for (const path of journalsToDelete) await this.io.removeFile(path);
  }
  private async saveGenerationAfterRotation(serviceId: ServiceId, generation: number): Promise<void> {
    await this.metadataSerial.run(async () => {
      this.rotationGenerations.set(serviceId, generation);
      await this.io.writeFile(this.streamStatePath, JSON.stringify({ version: logStreamStateVersion, generations: Object.fromEntries(this.rotationGenerations) } satisfies LogStreamState));
    });
  }
  private pathFor(serviceId: ServiceId): string {
    return join(this.directory, `${serviceId}.log`);
  }
  private rotationJournalPath(serviceId: ServiceId): string {
    return `${this.pathFor(serviceId)}.rotation.json`;
  }
  private async exists(path: string): Promise<boolean> {
    return (await stat(path).catch(() => undefined)) !== undefined;
  }
  private async size(path: string): Promise<number> {
    return (await stat(path).catch(() => undefined))?.size ?? 0;
  }
  private async readBuffer(path: string, start: number, length: number): Promise<{ buffer: Buffer; bytesRead: number }> {
    const handle = await open(path, "r");
    try {
      const buffer = Buffer.alloc(length);
      const { bytesRead } = await handle.read(buffer, 0, length, start);
      return { buffer, bytesRead };
    } finally {
      await handle.close();
    }
  }
  private async isUtf8Boundary(path: string, cursor: number, size: number): Promise<boolean> {
    if (cursor === 0 || cursor === size) return true;
    const { buffer, bytesRead } = await this.readBuffer(path, cursor, 1);
    return bytesRead === 1 && (buffer[0]! & 0xc0) !== 0x80;
  }
  private async tailStart(path: string, size: number, limit: number): Promise<number> {
    let start = Math.max(0, size - limit);
    while (start < size && !(await this.isUtf8Boundary(path, start, size))) start++;
    return start;
  }
  private async readFramed(path: string, start: number, size: number, limit: number): Promise<{ data: string; bytesRead: number }> {
    const maximum = Math.min(size - start, limit + 3);
    const { buffer, bytesRead } = await this.readBuffer(path, start, maximum);
    let end = Math.min(bytesRead, limit);
    while (end < bytesRead && (buffer[end]! & 0xc0) === 0x80) end++;
    if (end < bytesRead && end > 0 && (buffer[end - 1]! & 0xe0) === 0xc0 && end - 1 + 2 > end) end = Math.min(bytesRead, end + 1);
    if (end < bytesRead && end > 0 && (buffer[end - 1]! & 0xf0) === 0xe0 && end - 1 + 3 > end) end = Math.min(bytesRead, end + 2);
    if (end < bytesRead && end > 0 && (buffer[end - 1]! & 0xf8) === 0xf0 && end - 1 + 4 > end) end = Math.min(bytesRead, end + 3);
    while (end < bytesRead && (buffer[end]! & 0xc0) === 0x80) end++;
    return { data: buffer.subarray(0, end).toString("utf8"), bytesRead: end };
  }
  private async rotate(serviceId: ServiceId): Promise<void> {
    const path = this.pathFor(serviceId);
    const fromGeneration = this.rotationGenerations.get(serviceId) ?? 1;
    const journalPath = this.rotationJournalPath(serviceId);
    const journal: LogRotationJournal = { version: logStreamStateVersion, serviceId, fromGeneration, toGeneration: fromGeneration + 1, phase: "pending" };
    const rotatedPath = `${path}.${fromGeneration}`;
    await this.io.writeFile(journalPath, JSON.stringify(journal));
    await this.hooks.onRotationStep?.("pending");
    // A second writer (another daemon sharing this runtime directory) may have rotated the same file
    // first; the log is simply already gone, so appending recreates it. Throwing here would reject
    // the append that triggered the rotation.
    await rename(path, rotatedPath).catch((error: NodeJS.ErrnoException) => {
      if (error.code !== "ENOENT") throw error;
    });
    await this.hooks.onRotationStep?.("renamed");
    await this.io.writeFile(journalPath, JSON.stringify({ ...journal, phase: "committed" }));
    await this.hooks.onRotationStep?.("committed");
    const rotated = (await readdir(this.directory)).filter((entry) => entry.startsWith(`${serviceId}.log.`) && !entry.endsWith(".rotation.json")).sort();
    await Promise.all(rotated.slice(0, Math.max(0, rotated.length - this.rotationCount)).map((entry) => this.io.removeFile(join(this.directory, entry))));
    await this.saveGenerationAfterRotation(serviceId, journal.toGeneration);
    await this.hooks.onRotationStep?.("published");
    await this.io.removeFile(journalPath);
  }
}

// -------------------------------------------------------------------------------------------
// LocalServicesManager
// -------------------------------------------------------------------------------------------

export type LocalServicesManagerOptions = {
  runtimeDirectory?: string;
  root?: string;
  catalog: ServiceCatalog;
  eventCapacity?: number;
  logTailBytes?: number;
  logMaxBytes?: number;
  logRotationCount?: number;
  logHooks?: CursorLogStoreHooks;
  supervisor?: SupervisorOptions;
  platform?: LocalServicesPlatform;
};

const definitionOwnership = (catalog: ServiceCatalog, serviceId: ServiceId): "daemon" | "external" => catalog.services.find((service) => service.id === serviceId)?.ownership ?? "daemon";

export class LocalServicesManager {
  readonly instanceId: string;
  readonly events: ManagerEventStore;
  readonly operations: OperationScheduler;
  readonly logs: CursorLogStore;
  readonly stateStore: AtomicStateStore;
  readonly supervisor: ProcessSupervisor;
  /** Not `readonly`: `reloadCatalog` swaps this reference. Every reader (the supervisor via `Host`,
   * every HTTP handler) re-reads `this.catalog`/`this.host.catalog` on each use rather than caching
   * it, so a swap takes effect for the very next operation. */
  catalog: ServiceCatalog;
  readonly runtimeDirectory: string;
  private readonly io: FileIo;
  private readonly lock: LockHandle;
  private readonly token: string;
  private readonly lifecycle = new AsyncSerial();
  private readonly catalogReloadSerial = new AsyncSerial();
  private server: ReturnType<typeof Bun.serve> | undefined;
  private externalSyncTimer: ReturnType<typeof setInterval> | undefined;
  private state: PersistedManagerState = { version: STATE_VERSION, services: {} };
  private metadata: ManagerMetadata | undefined;
  private closed = false;
  private closing = false;
  private shutdownPromise: Promise<void> | undefined;
  private readonly shutdownResult = Promise.withResolvers<void>();

  private constructor(options: LocalServicesManagerOptions, io: FileIo, lock: LockHandle, token: string, instanceId: string) {
    this.instanceId = instanceId;
    this.runtimeDirectory = options.runtimeDirectory ?? resolveRuntimeDirectory(options.root ?? process.cwd(), options.catalog.runtimeDirectory);
    this.catalog = options.catalog;
    this.io = io;
    this.lock = lock;
    this.token = token;
    this.events = new ManagerEventStore(options.eventCapacity, instanceId);
    this.operations = new OperationScheduler(this.events);
    this.logs = new CursorLogStore(io, join(this.runtimeDirectory, "logs"), options.logTailBytes, options.logMaxBytes, options.logRotationCount, options.logHooks);
    this.stateStore = new AtomicStateStore(io, this.runtimeDirectory);
    this.supervisor = new ProcessSupervisor(this, { ...(options.supervisor ?? defaultSupervisorOptions(options.root ?? process.cwd(), this.runtimeDirectory)), isClosing: () => this.closing });
  }

  static async bootstrap(options: LocalServicesManagerOptions): Promise<LocalServicesManager> {
    requireSupportedLocalServicesPlatform(options.platform ?? process.platform);
    const validation = validateCatalog(options.catalog);
    if (validation.errors.length) throw new Error(`Invalid service catalog: ${validation.errors.join("; ")}`);
    const runtimeDirectory = options.runtimeDirectory ?? resolveRuntimeDirectory(options.root ?? process.cwd(), options.catalog.runtimeDirectory);
    const io = createFileIo(options.catalog.privateFileGuard !== false);
    const token = randomBytes(32).toString("base64url");
    const bootstrapMetadata: ManagerMetadata = { version: managerMetadataVersion, protocolVersion: managerProtocolVersion, instanceId: randomUUID(), pid: process.pid, port: 0, startedAt: now() };
    const lock = await claimLock(io, runtimeDirectory, bootstrapMetadata, token);
    const manager = new LocalServicesManager({ ...options, runtimeDirectory }, io, lock, token, bootstrapMetadata.instanceId);
    try {
      manager.state = await manager.stateStore.load();
      await manager.supervisor.reconcile();
      manager.server = Bun.serve({ hostname: "127.0.0.1", port: 0, fetch: (request) => manager.handleRequest(request) });
      const port = manager.server.port;
      if (port === undefined) throw new Error("Manager server did not expose its loopback port");
      manager.metadata = { ...bootstrapMetadata, port };
      const key = await readLockOwnershipKey(io, runtimeDirectory);
      if (!key) throw new Error("Missing ownership key");
      await io.writeFile(lock.metadataPath, JSON.stringify(manager.metadata));
      await io.writeFile(lock.proofPath, JSON.stringify(createLockOwnershipProof(key, manager.metadata, token)));
      manager.events.publish("manager.started", { instanceId: manager.instanceId });
      if (manager.catalog.services.some((service) => (service.ownership ?? "daemon") === "external")) {
        await manager.supervisor.syncExternalServices();
        manager.externalSyncTimer = setInterval(() => {
          manager.supervisor.syncExternalServices().catch((error: unknown) => manager.recordBackgroundError("syncExternalServices", error));
        }, 2000);
      }
      return manager;
    } catch (error) {
      manager.server?.stop(true);
      await prepareOwnedLockRelease(io, lock, token);
      await releaseOwnedLock(io, lock, token);
      throw error;
    }
  }

  get info(): ManagerInfo {
    if (!this.metadata) throw new Error("Manager has not started");
    const { version, ...metadata } = this.metadata;
    return { ...metadata, metadataVersion: version, runtimeDirectory: this.runtimeDirectory };
  }
  get bearerToken(): string {
    return this.token;
  }
  get baseUrl(): string {
    if (!this.metadata) throw new Error("Manager has not started");
    return `http://127.0.0.1:${this.metadata.port}`;
  }

  async setServiceState(next: ServiceLifecycleState): Promise<void> {
    await this.lifecycle.run(async () => {
      if (this.closed) return;
      if (!isLifecycleState(next, next.serviceId)) throw new Error("Refusing invalid service lifecycle state");
      this.state.services[next.serviceId] = next;
      await this.stateStore.save(this.state);
      this.events.publish("service.lifecycle", { serviceId: next.serviceId, actualState: next.actualState, generation: next.generation, operationId: next.currentOperationId });
    });
  }
  async appendLog(serviceId: ServiceId, data: string): Promise<void> {
    if (!this.closed) {
      await this.logs.append(serviceId, data);
      this.events.publish("service.log", { serviceId });
    }
  }
  publish(type: string, data: Record<string, unknown>): void {
    this.events.publish(type, data);
  }
  /** Sink for errors raised by fire-and-forget work (log forwarding, external-state polling) that
   * must never reject into — or terminate — the daemon process. Surfaces them as manager events so a
   * TUI/MCP client can see them without scraping logs. */
  recordBackgroundError(scope: string, error: unknown): void {
    const message = error instanceof Error && error.message ? error.message : String(error);
    this.events.publish("manager.error", { scope, message });
  }
  serviceStates(): ServiceLifecycleState[] {
    const timestamp = now();
    return this.catalog.services.map(
      (service) => this.state.services[service.id] ?? { serviceId: service.id, desiredState: "stopped" as const, actualState: "stopped" as const, readiness: "unknown" as const, generation: 0, createdAt: timestamp, updatedAt: timestamp },
    );
  }

  /** Swaps in a new catalog after validating it. A service removed from the new catalog that is
   * currently active gets stopped first — using the *old* catalog, since `ProcessSupervisor` needs
   * the service's definition to know how to stop it — and only then does the swap happen, so a
   * removed-but-still-stopping service is never briefly invisible from `serviceStates()`/`/v1/services`
   * while its process is still alive. `external`-owned removed services are left alone, same as
   * every other operation in this package. A service that stays present but whose definition changed
   * (command, readiness, ...) is left running as-is — reload never restarts a healthy service out
   * from under a developer — and is reported back in `changed` so a caller can decide whether/when to
   * restart it. Rejects (without swapping or stopping anything) when the new catalog fails
   * `validateCatalog`, or while the manager is shutting down. Serialized against itself (not against
   * `this.lifecycle`, which `supervisor.stop`'s own state writes run through — nesting into that
   * from here would deadlock). */
  async reloadCatalog(nextCatalog: ServiceCatalog): Promise<{ ok: true; stopped: ServiceId[]; changed: ServiceId[] } | { ok: false; errors: string[] }> {
    return this.catalogReloadSerial.run(async () => {
      if (this.closing) return { ok: false, errors: ["manager is shutting down"] };
      const validation = validateCatalog(nextCatalog);
      if (validation.errors.length) return { ok: false, errors: validation.errors };
      const previous = this.catalog;
      const nextById = new Map(nextCatalog.services.map((service) => [service.id, service]));
      const removedIds = previous.services.filter((service) => !nextById.has(service.id)).map((service) => service.id);
      const changed = previous.services.filter((service) => { const next = nextById.get(service.id); return next !== undefined && JSON.stringify(next) !== JSON.stringify(service); }).map((service) => service.id);
      const stopped: ServiceId[] = [];
      for (const serviceId of removedIds) {
        if (definitionOwnership(previous, serviceId) !== "daemon") continue;
        const state = this.state.services[serviceId];
        if (!state || !activeStates.includes(state.actualState)) continue;
        await this.supervisor.stop(serviceId);
        stopped.push(serviceId);
      }
      this.catalog = nextCatalog;
      this.events.publish("manager.catalog-reloaded", { removed: removedIds, changed, stopped });
      return { ok: true, stopped, changed };
    });
  }

  private async queueStoppedServicesForStart(serviceIds: readonly ServiceId[], operationId: string): Promise<void> {
    await this.lifecycle.run(async () => {
      if (this.closed) return;
      const timestamp = now();
      let changed = false;
      for (const serviceId of serviceIds) {
        const previous = this.state.services[serviceId] ?? { serviceId, desiredState: "stopped" as const, actualState: "stopped" as const, readiness: "unknown" as const, generation: 0, createdAt: timestamp, updatedAt: timestamp };
        if (previous.actualState !== "stopped") continue;
        const next: ServiceLifecycleState = { ...previous, desiredState: "running", actualState: "queued-start", readiness: "unknown", updatedAt: timestamp, currentOperationId: operationId };
        this.state.services[serviceId] = next;
        this.events.publish("service.lifecycle", { serviceId, actualState: next.actualState, generation: next.generation, operationId });
        changed = true;
      }
      if (changed) await this.stateStore.save(this.state);
    });
  }
  private async clearQueuedStarts(serviceIds: readonly ServiceId[], operationId: string): Promise<void> {
    await this.lifecycle.run(async () => {
      if (this.closed) return;
      const timestamp = now();
      let changed = false;
      for (const serviceId of serviceIds) {
        const previous = this.state.services[serviceId];
        if (!previous || previous.actualState !== "queued-start" || previous.currentOperationId !== operationId) continue;
        const next: ServiceLifecycleState = { ...previous, desiredState: "stopped", actualState: "stopped", readiness: "unknown", updatedAt: timestamp, exitedAt: timestamp, currentOperationId: undefined };
        this.state.services[serviceId] = next;
        this.events.publish("service.lifecycle", { serviceId, actualState: next.actualState, generation: next.generation });
        changed = true;
      }
      if (changed) await this.stateStore.save(this.state);
    });
  }
  private lifecycleGeneration(serviceId: ServiceId): number {
    return this.state.services[serviceId]?.generation ?? 0;
  }

  /** Alias for `shutdown('refuse-if-active')`, matching viclass's simpler single-mode API surface. */
  async close(): Promise<void> {
    await this.shutdown("refuse-if-active");
  }
  get shutdownCompletion(): Promise<void> {
    return this.shutdownResult.promise;
  }
  shutdown(mode: "refuse-if-active" | "stop-services" = "stop-services"): Promise<void> {
    return this.beginShutdown(mode);
  }
  private beginShutdown(mode: "refuse-if-active" | "stop-services", ownerReady?: Promise<void>): Promise<void> {
    this.closing = true;
    this.operations.closeMutations();
    this.supervisor.beginShutdown();
    this.shutdownPromise ??= (async () => {
      await ownerReady;
      await this.operations.drainServices();
      if (mode === "stop-services") await this.supervisor.shutdown();
      await this.lifecycle.run(async () => this.closeLocked());
    })();
    void this.shutdownPromise.then(this.shutdownResult.resolve, this.shutdownResult.reject);
    return this.shutdownPromise;
  }
  private async closeLocked(): Promise<void> {
    if (this.closed) return;
    if (this.externalSyncTimer) clearInterval(this.externalSyncTimer);
    const releasePrepared = await prepareOwnedLockRelease(this.io, this.lock, this.token);
    this.closed = true;
    this.events.publish("manager.stopped", { instanceId: this.instanceId });
    this.server?.stop(true);
    if (releasePrepared) await releaseOwnedLock(this.io, this.lock, this.token);
  }

  private async handleRequest(request: Request): Promise<Response> {
    const url = new URL(request.url);
    try {
      if (url.pathname === "/healthz" && request.method === "GET") return this.json({ status: "ok", protocolVersion: managerProtocolVersion, ...(this.authorized(request) ? { instanceId: this.instanceId } : {}) });
      this.requireAuthorized(request);
      this.requireProtocol(request);
      if (url.pathname === "/v1/manager" && request.method === "GET") return this.json(this.info);
      if (url.pathname === "/v1/catalog" && request.method === "GET") return this.json({ catalog: this.catalog });
      if (url.pathname === "/v1/manager/reload" && request.method === "POST") return await this.reloadRequest(request);
      if (url.pathname === "/v1/services" && request.method === "GET") return this.json({ services: this.serviceStates() });
      if (url.pathname === "/v1/operations" && request.method === "POST") return await this.createServiceOperation(request);
      if (url.pathname === "/v1/operations/bulk-start" && request.method === "POST") return await this.createBulkStartOperation(request);
      if (url.pathname.startsWith("/v1/operations/") && url.pathname.length > "/v1/operations/".length && request.method === "GET") return this.getOperation(url.pathname.slice("/v1/operations/".length));
      if (url.pathname === "/v1/events" && request.method === "GET") return this.replayEvents(url);
      if (url.pathname === "/v1/events/stream" && request.method === "GET") return this.streamEvents(url, request.signal);
      if (url.pathname.startsWith("/v1/logs/") && request.method === "GET") return await this.readLogs(url, url.pathname.slice("/v1/logs/".length));
      if (url.pathname === "/v1/manager/shutdown" && request.method === "POST") return await this.shutdownRequest(request);
      throw new ManagerHttpError(404, "not_found", "Endpoint not found");
    } catch (error) {
      if (error instanceof ManagerHttpError) return this.json({ error: { code: error.code, message: error.message } }, error.status);
      return this.json({ error: { code: "internal_error", message: "Unexpected manager error" } }, 500);
    }
  }
  private authorized(request: Request): boolean {
    return request.headers.get("authorization") === `Bearer ${this.token}`;
  }
  private requireAuthorized(request: Request): void {
    if (!this.authorized(request)) throw new ManagerHttpError(401, "unauthorized", "Bearer authentication is required");
  }
  private requireProtocol(request: Request): void {
    if (Number(request.headers.get("x-local-services-protocol")) !== managerProtocolVersion) throw new ManagerHttpError(426, "incompatible_protocol", `Expected protocol ${managerProtocolVersion}`);
  }
  private isServiceId(value: unknown): value is ServiceId {
    return typeof value === "string" && this.catalog.services.some((service) => service.id === value);
  }

  private async createServiceOperation(request: Request): Promise<Response> {
    if (this.closing) throw new ManagerHttpError(409, "manager_closing", "Manager is shutting down");
    const body = await this.strictBody(request, ["requestId", "serviceId", "action"], ["requestId", "serviceId", "action"]);
    if (typeof body.requestId !== "string" || body.requestId.length === 0 || body.requestId.length > 128) throw new ManagerHttpError(400, "invalid_request", "requestId must be a non-empty string");
    if (!this.isServiceId(body.serviceId)) throw new ManagerHttpError(400, "invalid_service", "serviceId must be a catalog service");
    if (body.action !== "start" && body.action !== "stop" && body.action !== "restart" && body.action !== "status") throw new ManagerHttpError(400, "invalid_action", "action must be start, stop, restart, or status");
    const serviceId = body.serviceId;
    const action = body.action as ServiceOperationKind;
    const startServices = action === "start" ? dependencyLevels(this.catalog, [serviceId]).flat() : [];
    const input = { requestId: body.requestId, kind: "service" as const, serviceId, action };
    const existing = this.operations.resolveRequest(input);
    if (existing) return this.json({ operation: existing }, 202);
    const operation = this.operations.schedule(
      input,
      async (scheduled) => {
        try {
          if (action === "start") await this.startSelectedDag(startServices, scheduled);
          else if (action === "stop") await this.supervisor.stop(serviceId, scheduled.id);
          else if (action === "restart") await this.supervisor.restart(serviceId, scheduled.id);
          else await this.supervisor.status(serviceId);
        } finally {
          if (action === "start") await this.clearQueuedStarts(startServices, scheduled.id);
        }
      },
      async (scheduled) => {
        if (action === "start") await this.clearQueuedStarts(startServices, scheduled.id);
      },
    );
    if (action === "start") await this.queueStoppedServicesForStart(startServices, operation.id);
    return this.json({ operation }, 202);
  }

  private async createBulkStartOperation(request: Request): Promise<Response> {
    if (this.closing) throw new ManagerHttpError(409, "manager_closing", "Manager is shutting down");
    const body = await this.strictBody(request, ["requestId", "targets"], ["requestId", "targets"]);
    if (typeof body.requestId !== "string" || body.requestId.length === 0 || body.requestId.length > 128) throw new ManagerHttpError(400, "invalid_request", "requestId must be a non-empty string");
    if (!Array.isArray(body.targets) || body.targets.length === 0 || body.targets.some((target) => !this.isServiceId(target)) || new Set(body.targets).size !== body.targets.length)
      throw new ManagerHttpError(400, "invalid_targets", "targets must be a non-empty set of catalog services");
    const targets = body.targets as ServiceId[];
    const services = new Map(this.catalog.services.map((service) => [service.id, service]));
    if (targets.some((target) => services.get(target)?.profiles.run.commandStatus !== "verified")) throw new ManagerHttpError(409, "unsupported_service", "The requested catalog service is not executable");
    const selected = dependencyLevels(this.catalog, targets).flat();
    const input = { requestId: body.requestId, kind: "bulk-start" as const, targetServiceIds: targets };
    const existing = this.operations.resolveRequest(input);
    if (existing) return this.json({ operation: existing }, 202);
    const operation = this.operations.schedule(
      input,
      async (scheduled) => {
        try {
          await this.startSelectedDag(selected, scheduled);
        } finally {
          await this.clearQueuedStarts(selected, scheduled.id);
        }
      },
      (scheduled) => this.clearQueuedStarts(selected, scheduled.id),
    );
    await this.queueStoppedServicesForStart(selected, operation.id);
    return this.json({ operation }, 202);
  }

  private async startSelectedDag(selected: readonly ServiceId[], operation: Operation): Promise<void> {
    type StartResult = { serviceId: ServiceId; status: "ready" | "failed" | "blocked"; error?: unknown };
    const selectedSet = new Set(selected);
    const services = new Map(this.catalog.services.map((service) => [service.id, service]));
    const tasks = new Map<ServiceId, Promise<StartResult>>();
    for (const serviceId of selected) {
      const dependencies = (services.get(serviceId)?.dependencies ?? []).filter((dependency) => selectedSet.has(dependency));
      const task = (async (): Promise<StartResult> => {
        const dependencyResults = await Promise.all(dependencies.map((dependency) => tasks.get(dependency)!));
        const unavailable = dependencyResults.filter((result) => result.status !== "ready").map((result) => result.serviceId);
        if (unavailable.length) {
          this.operations.trace(operation, `Blocked: ${serviceId} (dependencies not ready: ${unavailable.join(", ")})`);
          return { serviceId, status: "blocked" };
        }
        const state = this.serviceStates().find((current) => current.serviceId === serviceId);
        if (state?.desiredState !== "running") {
          this.operations.trace(operation, `Skipped: ${serviceId} (start cancelled)`);
          return { serviceId, status: "blocked" };
        }
        if (state.actualState === "ready" && state.readiness === "ready") {
          this.operations.trace(operation, `Ready: ${serviceId} (already ready)`);
          return { serviceId, status: "ready" };
        }
        this.operations.trace(operation, `Starting: ${serviceId}`);
        try {
          await this.supervisor.start(serviceId, operation.id);
          const after = this.serviceStates().find((current) => current.serviceId === serviceId);
          const settled = after?.actualState === "ready" || (after?.actualState === "running-unready" && after.readinessKind === "process");
          if (!settled) throw new Error(`${serviceId} did not become ready`);
          this.operations.trace(operation, `Ready: ${serviceId}`);
          return { serviceId, status: "ready" };
        } catch (error) {
          this.operations.trace(operation, `Failed: ${serviceId}${error instanceof Error && error.message ? ` (${error.message})` : ""}`);
          return { serviceId, status: "failed", error };
        }
      })();
      tasks.set(serviceId, task);
    }
    const results = await Promise.all(selected.map((serviceId) => tasks.get(serviceId)!));
    const failures = results.filter((result) => result.status === "failed");
    if (failures.length) {
      const summary = failures.map((result) => result.serviceId).join(", ");
      const cause = failures[0]!.error;
      throw new Error(`Dependency startup failed: ${summary}${cause instanceof Error && cause.message ? ` (${cause.message})` : ""}`);
    }
  }

  private getOperation(id: string): Response {
    const operation = this.operations.get(id);
    if (!operation) throw new ManagerHttpError(404, "operation_not_found", "Operation not found");
    return this.json({ operation });
  }
  private replayEvents(url: URL): Response {
    const afterValue = url.searchParams.get("after");
    const after = afterValue === null ? undefined : Number(afterValue);
    if (after !== undefined && (!Number.isInteger(after) || after < 0)) throw new ManagerHttpError(400, "invalid_cursor", "after must be a non-negative integer");
    return this.json(this.events.replay(after, url.searchParams.get("epoch") ?? undefined));
  }
  private streamEvents(url: URL, signal: AbortSignal): Response {
    const afterValue = url.searchParams.get("after");
    const after = afterValue === null ? undefined : Number(afterValue);
    if (after !== undefined && (!Number.isInteger(after) || after < 0)) throw new ManagerHttpError(400, "invalid_cursor", "after must be a non-negative integer");
    const replay = this.events.replay(after, url.searchParams.get("epoch") ?? undefined);
    const encoder = new TextEncoder();
    const frame = (type: string, data: unknown, sequence?: number): Uint8Array => encoder.encode(`${sequence === undefined ? "" : `id: ${sequence}\n`}event: ${type}\ndata: ${JSON.stringify(data)}\n\n`);
    let close: (() => void) | undefined;
    let drain: (() => void) | undefined;
    const stream = new ReadableStream<Uint8Array>(
      {
        start: (controller) => {
          let closed = false;
          let terminal = false;
          let unsubscribe: (() => void) | undefined;
          const pending: Uint8Array[] = [];
          let pendingBytes = 0;
          const stop = (): void => {
            if (closed) return;
            closed = true;
            unsubscribe?.();
            unsubscribe = undefined;
            signal.removeEventListener("abort", stop);
            pending.length = 0;
            pendingBytes = 0;
            try {
              controller.close();
            } catch {
              return;
            }
          };
          const flush = (): void => {
            while (!closed && (controller.desiredSize ?? 0) > 0 && pending.length > 0) {
              const next = pending.shift()!;
              pendingBytes -= next.byteLength;
              controller.enqueue(next);
            }
            if (terminal && pending.length === 0) stop();
          };
          const enqueue = (next: Uint8Array): void => {
            if (closed) return;
            if (next.byteLength > maxSseQueueBytes || pending.length >= maxSseQueueFrames || pendingBytes + next.byteLength > maxSseQueueBytes) {
              stop();
              return;
            }
            pending.push(next);
            pendingBytes += next.byteLength;
            flush();
          };
          close = stop;
          drain = flush;
          if (signal.aborted) return stop();
          signal.addEventListener("abort", stop, { once: true });
          const replayEvents_ = replay.events.map((event) => frame(event.type, event, event.sequence));
          const marker = frame("replay", { epoch: replay.epoch, reset: false, latestSequence: replay.latestSequence });
          const reset = replay.reset || replayEvents_.length + 1 > maxSseQueueFrames || marker.byteLength + replayEvents_.reduce((bytes, event) => bytes + event.byteLength, 0) > maxSseQueueBytes;
          enqueue(reset ? frame("replay", { epoch: replay.epoch, reset: true, latestSequence: replay.latestSequence }) : marker);
          if (reset) {
            terminal = true;
            flush();
            return;
          }
          unsubscribe = this.events.subscribe((event) => enqueue(frame(event.type, event, event.sequence)));
          for (const event of replayEvents_) enqueue(event);
        },
        pull: () => drain?.(),
        cancel: () => close?.(),
      },
      { highWaterMark: 1 },
    );
    return new Response(stream, { headers: { "cache-control": "no-store", connection: "keep-alive", "content-type": "text/event-stream" } });
  }
  private async readLogs(url: URL, rawServiceId: string): Promise<Response> {
    if (!this.isServiceId(rawServiceId)) throw new ManagerHttpError(404, "service_not_found", "Service is not in the catalog");
    const cursorValue = url.searchParams.get("cursor");
    const cursor = cursorValue === null ? undefined : Number(cursorValue);
    const limitValue = url.searchParams.get("limit");
    const limit = limitValue === null ? undefined : Number(limitValue);
    const generationValue = url.searchParams.get("generation");
    const generation = generationValue === null ? undefined : Number(generationValue);
    if (cursor !== undefined && (!Number.isInteger(cursor) || cursor < 0)) throw new ManagerHttpError(400, "invalid_cursor", "cursor must be a non-negative integer");
    if (limit !== undefined && (!Number.isInteger(limit) || limit < 1)) throw new ManagerHttpError(400, "invalid_limit", "limit must be a positive integer");
    if (generation !== undefined && (!Number.isInteger(generation) || generation < 0)) throw new ManagerHttpError(400, "invalid_generation", "generation must be a non-negative integer");
    return this.json(await this.logs.read(rawServiceId, cursor, limit, this.lifecycleGeneration(rawServiceId), generation));
  }
  private async shutdownRequest(request: Request): Promise<Response> {
    const body = await this.strictBody(request, ["requestId", "mode"], ["requestId"]);
    if (typeof body.requestId !== "string" || body.requestId.length === 0) throw new ManagerHttpError(400, "invalid_request", "requestId must be a non-empty string");
    if (body.mode !== undefined && body.mode !== "refuse-if-active" && body.mode !== "stop-services") throw new ManagerHttpError(400, "invalid_shutdown_mode", "mode must be refuse-if-active or stop-services");
    const mode = (body.mode as "refuse-if-active" | "stop-services" | undefined) ?? "refuse-if-active";
    const scheduleInput = { requestId: body.requestId, kind: "manager-shutdown" as const };
    const existing = this.operations.resolveRequest(scheduleInput);
    if (existing) return this.json({ operation: existing }, 202);
    if (this.closing) throw new ManagerHttpError(409, "manager_closing", "Manager is shutting down");
    // Services with ownership 'external' are adopted, never spawned or stopped by this manager, so
    // they can never block or be interrupted by a shutdown (generalizes infra's docker/tailnet carve-out).
    const active = this.serviceStates().filter((service) => definitionOwnership(this.catalog, service.serviceId) === "daemon" && !["stopped", "queued-start", "failed", "orphaned", "externally-owned"].includes(service.actualState));
    if (active.length > 0 && mode !== "stop-services") throw new ManagerHttpError(409, "active_services", "Manager shutdown is refused while managed services are active");
    let releaseOwner!: () => void;
    const ownerReady = new Promise<void>((resolve) => {
      releaseOwner = resolve;
    });
    this.beginShutdown(mode, ownerReady);
    const operation = this.operations.schedule(scheduleInput, async () => undefined);
    void this.operations.wait(operation).then(releaseOwner, releaseOwner);
    return this.json({ operation }, 202);
  }
  private async reloadRequest(request: Request): Promise<Response> {
    if (this.closing) throw new ManagerHttpError(409, "manager_closing", "Manager is shutting down");
    const body = await this.strictBody(request, ["requestId", "catalog"], ["requestId", "catalog"]);
    if (typeof body.requestId !== "string" || body.requestId.length === 0) throw new ManagerHttpError(400, "invalid_request", "requestId must be a non-empty string");
    const catalog = body.catalog;
    if (!isRecord(catalog) || !Array.isArray(catalog.services) || !isRecord(catalog.groups) || typeof catalog.startFailurePolicy !== "string") {
      throw new ManagerHttpError(400, "invalid_catalog", "catalog must be a ServiceCatalog: { services: [...], groups: {...}, startFailurePolicy }");
    }
    const result = await this.reloadCatalog(catalog as unknown as ServiceCatalog);
    if (!result.ok) throw new ManagerHttpError(422, "invalid_catalog", result.errors.join("; "));
    return this.json({ stopped: result.stopped, changed: result.changed });
  }
  private async strictBody(request: Request, allowed: readonly string[], required: readonly string[]): Promise<Record<string, unknown>> {
    let body: unknown;
    try {
      body = await request.json();
    } catch {
      throw new ManagerHttpError(400, "invalid_json", "Request body must be JSON");
    }
    if (!isRecord(body) || Object.keys(body).some((key) => !allowed.includes(key)) || required.some((key) => !(key in body))) throw new ManagerHttpError(400, "invalid_request", "Request schema is invalid");
    return body;
  }
  private json(body: unknown, status = 200): Response {
    return Response.json(body, { status, headers: { "cache-control": "no-store" } });
  }
}
