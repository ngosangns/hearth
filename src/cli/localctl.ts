import { randomUUID } from "node:crypto";
import { readdir, rm } from "node:fs/promises";
import { join } from "node:path";

import { validateCatalog, type ServiceCatalog, type ServiceId } from "../core/catalog";
import { createFileIo, type FileIo } from "../core/file-io";
import {
  isStaleLockMarker,
  managerProtocolVersion,
  readLockOwnershipKey,
  readOwnedLockArtifacts,
  verifyLockOwnershipProof,
  type OwnedLockArtifacts,
} from "../core/manager";
import { resolveRuntimeDirectory } from "../core/paths";
import { staleLockMarkerName, type ManagerMetadata, type Operation, type ServiceLifecycleState } from "../core/state";
import type { DoctorChecks, DoctorReport } from "../core/doctor";
import { runDoctor } from "../core/doctor";

export const localctlExit = { usage: 2, unavailable: 3, protocol: 4, failed: 5, timeout: 6, unauthorized: 7 } as const;
export type Client = { root: string; runtimeDirectory: string; metadata: ManagerMetadata; token: string };
export type Discovery = { kind: "absent" | "malformed" | "incompatible" | "stale" | "live"; client?: Client };
export type Flags = { positionals: string[]; json: boolean; wait: boolean; follow: boolean; tail?: number };
// Bulk operations may occupy the manager's single request path briefly; this bounds only transport,
// while waitOperation polls until terminal state.
export const managerRequestTimeoutMs = 10_000;

export type LocalctlOptions = {
  catalog: ServiceCatalog;
  /** Spawns the daemon process, detached, given the repository root. The consumer supplies this
   * because only it knows where its own daemon entrypoint (importing `runDaemon` from
   * `@gnasdev/local-services/core`) lives — see the `/node-bridge` subpath and README for the
   * expected shape. */
  spawnDaemon: (root: string) => void;
  doctorChecks?: DoctorChecks;
};

export type LocalctlRuntime = {
  request?(client: Client, path: string, init?: RequestInit): Promise<unknown>;
  discover?(root: string): Promise<Discovery>;
  spawnDaemon?(root: string): void;
  sleep?(milliseconds: number): Promise<void>;
  now?(): number;
  readDirectory?(path: string): Promise<string[]>;
  readLockArtifacts?(path: string): Promise<OwnedLockArtifacts | undefined>;
  readOwnershipKey?(runtimeDirectory: string): Promise<string | undefined>;
  directory?(path: string): Promise<boolean>;
  remove?(path: string): Promise<void>;
  output?(data: string): void;
  error?(data: string): void;
  doctor?(root: string): Promise<DoctorReport>;
  tui?(root: string): Promise<number>;
};

export class LocalctlError extends Error {
  constructor(
    readonly exitCode: number,
    message: string,
  ) {
    super(message);
    this.name = "LocalctlError";
  }
}

function fail(exitCode: number, message: string): never {
  throw new LocalctlError(exitCode, message);
}
const usage = (message: string): never => fail(localctlExit.usage, message);
const runtimeValue = <T>(value: T | undefined, fallback: T): T => value ?? fallback;
const output = (runtime: LocalctlRuntime, value: string): void => runtimeValue(runtime.output, console.log)(value);
const error = (runtime: LocalctlRuntime, value: string): void => runtimeValue(runtime.error, console.error)(value);

export function parseCommandFlags(arguments_: string[], allowed: readonly ("json" | "wait" | "follow" | "tail")[]): Flags {
  const result: Flags = { positionals: [], json: false, wait: false, follow: false };
  const seen = new Set<string>();
  const requireFlag = (name: string): void => {
    if (!allowed.includes(name as never)) usage(`unsupported flag: --${name}`);
    if (seen.has(name)) usage(`duplicate flag: --${name}`);
    seen.add(name);
  };
  for (let index = 0; index < arguments_.length; index++) {
    const argument = arguments_[index]!;
    if (!argument.startsWith("--")) {
      result.positionals.push(argument);
      continue;
    }
    const name = argument.slice(2);
    if (name === "json") {
      requireFlag(name);
      result.json = true;
    } else if (name === "wait") {
      requireFlag(name);
      result.wait = true;
    } else if (name === "follow") {
      requireFlag(name);
      result.follow = true;
    } else if (name === "tail") {
      requireFlag(name);
      const value = Number(arguments_[++index]);
      if (!Number.isSafeInteger(value) || value < 1) usage("--tail must be a positive integer");
      result.tail = value;
    } else usage(`unknown flag: ${argument}`);
  }
  return result;
}

const validMetadata = (value: unknown): value is ManagerMetadata =>
  typeof value === "object" &&
  value !== null &&
  (value as ManagerMetadata).version === 1 &&
  typeof (value as ManagerMetadata).instanceId === "string" &&
  Number.isSafeInteger((value as ManagerMetadata).port) &&
  (value as ManagerMetadata).port > 0 &&
  Number.isSafeInteger((value as ManagerMetadata).pid) &&
  (value as ManagerMetadata).pid > 0 &&
  typeof (value as ManagerMetadata).startedAt === "string" &&
  Number.isSafeInteger((value as ManagerMetadata).protocolVersion);

export async function request(client: Client, path: string, init: RequestInit = {}, runtime: LocalctlRuntime = {}, protocolVersion = managerProtocolVersion): Promise<unknown> {
  const requestInit = { ...init, headers: { authorization: `Bearer ${client.token}`, "x-local-services-protocol": String(protocolVersion), ...(init.headers ?? {}) } };
  if (runtime.request) return runtime.request(client, path, requestInit);
  let response: Response;
  try {
    response = await fetch(`http://127.0.0.1:${client.metadata.port}${path}`, { ...requestInit, signal: AbortSignal.timeout(managerRequestTimeoutMs) });
  } catch (cause) {
    if (cause instanceof Error && (cause.name === "AbortError" || cause.name === "TimeoutError")) throw new Error(`manager request timed out after ${managerRequestTimeoutMs}ms`);
    throw new Error("manager unavailable");
  }
  const body = (await response.json().catch(() => ({}))) as { error?: { code?: string; message?: string } };
  if (!response.ok) throw new Error(`${body.error?.code ?? "request_failed"}:${body.error?.message ?? response.status}`);
  return body;
}

export async function discover(root: string, options: LocalctlOptions, runtime: LocalctlRuntime = {}): Promise<Discovery> {
  if (runtime.discover) return runtime.discover(root);
  const io = createFileIo(options.catalog.privateFileGuard !== false);
  const runtimeDirectory = resolveRuntimeDirectory(root, options.catalog.runtimeDirectory);
  const lockDirectory = join(runtimeDirectory, "manager.lock");
  let metadata: ManagerMetadata;
  let token: string;
  let artifacts: OwnedLockArtifacts | undefined;
  try {
    if (runtime.readLockArtifacts) {
      artifacts = await runtime.readLockArtifacts(lockDirectory);
      if (!artifacts) return { kind: "absent" };
      metadata = artifacts.metadata;
      token = artifacts.token;
    } else {
      if (!(await io.isPrivateDirectory(runtimeDirectory)) || !(await io.isPrivateDirectory(lockDirectory))) return { kind: "absent" };
      const [rawMetadata, rawToken] = await Promise.all([io.readFile(join(lockDirectory, "metadata.json")), io.readFile(join(lockDirectory, "token"))]);
      if (rawMetadata === undefined || rawToken === undefined) return { kind: "absent" };
      metadata = JSON.parse(rawMetadata) as ManagerMetadata;
      token = rawToken.trim();
    }
  } catch {
    return { kind: "malformed" };
  }
  if (!validMetadata(metadata) || !token) return { kind: "malformed" };
  if (!artifacts) artifacts = await readOwnedLockArtifacts(io, lockDirectory);
  const ownershipKey = await runtimeValue(runtime.readOwnershipKey, (dir: string) => readLockOwnershipKey(io, dir))(runtimeDirectory).catch(() => undefined);
  if (!artifacts || !ownershipKey || !verifyLockOwnershipProof(ownershipKey, artifacts.metadata, artifacts.token, artifacts.proof)) return { kind: "malformed" };
  const client: Client = { root, runtimeDirectory, metadata, token };
  if (metadata.protocolVersion !== managerProtocolVersion) return { kind: "incompatible", client };
  try {
    await request(client, "/v1/manager", {}, runtime);
    return { kind: "live", client };
  } catch (cause) {
    return String(cause).includes("unauthorized") ? { kind: "live", client } : { kind: "stale", client };
  }
}

const pendingEnsures = new Map<string, Promise<Client>>();

export async function ensure(root: string, options: LocalctlOptions, runtime: LocalctlRuntime = {}): Promise<Client> {
  const existing = pendingEnsures.get(root);
  if (existing) return existing;
  const work = (async (): Promise<Client> => {
    const current = await discover(root, options, runtime);
    if (current.kind === "live" && current.client) return current.client;
    if (current.kind === "incompatible") fail(localctlExit.protocol, "local services manager protocol is incompatible");
    const spawnDaemon = runtimeValue(runtime.spawnDaemon, options.spawnDaemon);
    const sleep = runtimeValue(runtime.sleep, Bun.sleep);
    spawnDaemon(root);
    for (let attempt = 0; attempt < 100; attempt++) {
      await sleep(50);
      const discovered = await discover(root, options, runtime);
      if (discovered.kind === "live" && discovered.client) return discovered.client;
      if (discovered.kind === "incompatible") fail(localctlExit.protocol, "local services manager protocol is incompatible");
    }
    return fail(localctlExit.unavailable, "local services manager is unavailable");
  })();
  pendingEnsures.set(root, work);
  try {
    return await work;
  } finally {
    pendingEnsures.delete(root);
  }
}

export async function requireClient(root: string, options: LocalctlOptions, runtime: LocalctlRuntime = {}): Promise<Client> {
  const discovered = await discover(root, options, runtime);
  if (discovered.kind === "incompatible") fail(localctlExit.protocol, "local services manager protocol is incompatible");
  const client = discovered.kind === "live" ? discovered.client : undefined;
  if (!client) fail(localctlExit.unavailable, "local services manager is unavailable");
  try {
    await request(client, "/v1/manager", {}, runtime);
  } catch (cause) {
    if (String(cause).includes("unauthorized")) fail(localctlExit.unauthorized, "local services manager authentication failed");
    fail(localctlExit.unavailable, "local services manager is unavailable");
  }
  return client;
}

export const targets = (catalog: ServiceCatalog, target: string | undefined): ServiceId[] => {
  if (!target) return catalog.services.map((service) => service.id);
  if (catalog.services.some((service) => service.id === target)) return [target];
  const group = catalog.groups[target];
  if (!group) return usage(`unknown service or group: ${target}`);
  return [...group];
};

export const runnableTargets = (catalog: ServiceCatalog, target: string | undefined): ServiceId[] => {
  const selected = targets(catalog, target);
  for (const serviceId of selected) {
    const service = catalog.services.find((candidate) => candidate.id === serviceId);
    if (!service || service.profiles.run.commandStatus !== "verified") fail(localctlExit.failed, `unsupported service: ${serviceId}`);
  }
  return selected;
};

export const operationId = (value: string): string => (/^[A-Za-z0-9._~-]{1,128}$/.test(value) ? encodeURIComponent(value) : usage("operationId is invalid"));

export async function waitOperation(client: Client, id: string, runtime: LocalctlRuntime = {}): Promise<Operation> {
  const sleep = runtimeValue(runtime.sleep, Bun.sleep);
  for (;;) {
    const operation = ((await request(client, `/v1/operations/${operationId(id)}`, {}, runtime)) as { operation: Operation }).operation;
    if (operation.status === "succeeded" || operation.status === "failed") return operation;
    await sleep(100);
  }
}

const print = (value: unknown, json: boolean, runtime: LocalctlRuntime): void =>
  output(runtime, json ? JSON.stringify(value) : typeof value === "string" ? value : JSON.stringify(value, null, 2));
const serviceRows = async (client: Client, runtime: LocalctlRuntime): Promise<ServiceLifecycleState[]> => ((await request(client, "/v1/services", {}, runtime)) as { services: ServiceLifecycleState[] }).services;
const textState = (state: ServiceLifecycleState | undefined): string =>
  state?.actualState === "ready" ? "ready" : state?.actualState === "queued-start" ? "queued-start" : state && ["running", "running-unready", "starting", "preparing"].includes(state.actualState) ? "running" : "stopped";

export async function cleanup(root: string, options: LocalctlOptions, runtime: LocalctlRuntime = {}): Promise<void> {
  const io = createFileIo(options.catalog.privateFileGuard !== false);
  const runtimeDirectory = resolveRuntimeDirectory(root, options.catalog.runtimeDirectory);
  if ((await discover(root, options, runtime)).kind === "live" || !(await io.isPrivateDirectory(runtimeDirectory))) return;
  const readDirectory = runtimeValue(runtime.readDirectory, (path: string) => readdir(path));
  const remove = runtimeValue(runtime.remove, (path: string) => rm(path, { recursive: true, force: true }));
  const ownershipKey = await readLockOwnershipKey(io, runtimeDirectory);
  if (!ownershipKey) return;
  const candidates = ["manager.lock", ...(await readDirectory(runtimeDirectory).catch(() => [] as string[])).filter((entry) => entry.startsWith("manager.lock.stale-"))];
  for (const entry of candidates) {
    const path = join(runtimeDirectory, entry);
    try {
      const artifacts = await readOwnedLockArtifacts(io, path);
      if (!artifacts || !verifyLockOwnershipProof(ownershipKey, artifacts.metadata, artifacts.token, artifacts.proof)) continue;
      if (entry === "manager.lock") {
        await remove(path);
        continue;
      }
      const marker = await io.readFile(join(path, staleLockMarkerName));
      if (marker !== undefined && isStaleLockMarker(JSON.parse(marker), ownershipKey, artifacts.metadata, artifacts.token, artifacts.proof)) await remove(path);
    } catch {
      /* Retain malformed or unowned lookalikes. */
    }
  }
}

export async function logs(client: Client, serviceId: ServiceId, tail: number, follow: boolean, json: boolean, options: LocalctlOptions, runtime: LocalctlRuntime = {}): Promise<void> {
  const sleep = runtimeValue(runtime.sleep, Bun.sleep);
  const write = runtime.output ? runtime.output : (data: string) => process.stdout.write(data);
  let cursor: number | undefined;
  let generation: number | undefined;
  let reconnects = 0;
  for (;;) {
    try {
      const query = new URLSearchParams({ limit: "16384", ...(cursor === undefined ? {} : { cursor: String(cursor) }), ...(generation === undefined ? {} : { generation: String(generation) }) });
      const slice = (await request(client, `/v1/logs/${serviceId}?${query}`, {}, runtime)) as { data: string; nextCursor: number; generation: number; reset: boolean };
      const lines = slice.data.split(/\r?\n/).filter((line) => line.length > 0);
      const response = { ...slice, data: lines.slice(-tail).join("\n") + (lines.length ? "\n" : "") };
      if (json) print(response, true, runtime);
      else {
        if (slice.reset) error(runtime, `log reset ${serviceId} generation ${slice.generation}`);
        if (response.data) write(response.data);
      }
      cursor = slice.nextCursor;
      generation = slice.generation;
      reconnects = 0;
      if (!follow) return;
      await sleep(500);
    } catch {
      if (!follow || ++reconnects > 3) fail(localctlExit.unavailable, "log follow lost manager connection");
      await sleep(200);
      client = await requireClient(client.root, options, runtime);
    }
  }
}

export async function main(options: LocalctlOptions, argv = process.argv.slice(2), runtime: LocalctlRuntime = {}): Promise<number> {
  try {
    let root = process.cwd();
    if (argv[0] === "--root") {
      root = argv[1] ?? usage("--root requires a path");
      argv = argv.slice(2);
    }
    const [command, ...rest] = argv;
    if (!command) usage("usage: local-services <command>");
    if (command === "doctor") {
      const flags = parseCommandFlags(rest, ["json"]);
      if (flags.positionals.length) usage("doctor takes no positional arguments");
      const report = await runtimeValue(runtime.doctor, (r: string) => runDoctor(options.catalog, options.doctorChecks))(root);
      if (flags.json) print(report, true, runtime);
      else {
        report.checks.forEach((check) => output(runtime, `${check.ok ? "ok" : "missing"} ${check.name} ${check.detail}`));
        report.unresolvedProfiles.forEach((warning) => output(runtime, `unresolved ${warning}`));
      }
      if (!report.ok) fail(localctlExit.failed, "doctor checks failed");
      return 0;
    }
    if (command === "cleanup") {
      const flags = parseCommandFlags(rest, []);
      if (flags.positionals.length) usage("cleanup takes no positional arguments");
      await cleanup(root, options, runtime);
      return 0;
    }
    if (command === "manager") {
      const flags = parseCommandFlags(rest, ["json"]);
      const [subcommand] = flags.positionals;
      if (flags.positionals.length !== 1 || !["ensure", "status", "stop"].includes(subcommand!)) usage("usage: local-services manager ensure|status|stop [--json]");
      if (subcommand === "ensure") {
        const client = await ensure(root, options, runtime);
        print({ instanceId: client.metadata.instanceId, port: client.metadata.port }, flags.json, runtime);
        return 0;
      }
      const discovered = await discover(root, options, runtime);
      if (subcommand === "status") {
        if (discovered.kind === "incompatible") fail(localctlExit.protocol, "local services manager protocol is incompatible");
        const client = await requireClient(root, options, runtime);
        print(await request(client, "/v1/manager", {}, runtime), flags.json, runtime);
        return 0;
      }
      const client = discovered.client ?? (await requireClient(root, options, runtime));
      const operation = ((await request(client, "/v1/manager/shutdown", { method: "POST", body: JSON.stringify({ requestId: randomUUID(), mode: "stop-services" }), headers: { "content-type": "application/json" } }, runtime, client.metadata.protocolVersion)) as {
        operation: Operation;
      }).operation;
      print(operation, flags.json, runtime);
      return 0;
    }
    if (command === "status") {
      const flags = parseCommandFlags(rest, ["json"]);
      if (flags.positionals.length > 1) usage("usage: local-services status [target] [--json]");
      const rows = await serviceRows(await requireClient(root, options, runtime), runtime);
      const result = targets(options.catalog, flags.positionals[0]).map((serviceId) => {
        const identity = rows.find((row) => row.serviceId === serviceId)?.identity;
        return { serviceId, state: textState(rows.find((row) => row.serviceId === serviceId)), pid: identity && "pid" in identity ? identity.pid : undefined };
      });
      if (flags.json) print({ services: result }, true, runtime);
      else result.forEach((row) => output(runtime, `${row.state} ${row.serviceId}${row.pid ? ` pid ${row.pid}` : ""}`));
      return 0;
    }
    if (command === "start" || command === "stop" || command === "restart") {
      const flags = parseCommandFlags(rest, ["wait", "json"]);
      if (flags.positionals.length !== 1) usage(`usage: local-services ${command} <service|group> [--wait] [--json]`);
      const target = flags.positionals[0]!;
      const selected = runnableTargets(options.catalog, target);
      const isSingleService = options.catalog.services.some((service) => service.id === target);
      if (!flags.wait && !isSingleService) usage(`group ${target} requires --wait to preserve stop-on-first-failure ordering`);
      const client = await ensure(root, options, runtime);
      if (command === "start" && !isSingleService) {
        const accepted = ((await request(client, "/v1/operations/bulk-start", { method: "POST", body: JSON.stringify({ requestId: randomUUID(), targets: selected }), headers: { "content-type": "application/json" } }, runtime)) as { operation: Operation }).operation;
        const operation = flags.wait ? await waitOperation(client, accepted.id, runtime) : accepted;
        if (flags.json) print({ operation }, true, runtime);
        else output(runtime, `${operation.status} bulk-start ${operation.id}`);
        if (operation.status === "failed") fail(localctlExit.failed, "service operation failed");
        return 0;
      }
      const operations: Operation[] = [];
      for (const serviceId of selected) {
        const accepted = ((await request(client, "/v1/operations", { method: "POST", body: JSON.stringify({ requestId: randomUUID(), serviceId, action: command }), headers: { "content-type": "application/json" } }, runtime)) as { operation: Operation }).operation;
        const completed = flags.wait ? await waitOperation(client, accepted.id, runtime) : accepted;
        operations.push(completed);
        if (completed.status === "failed") break;
      }
      if (flags.json) print({ operations }, true, runtime);
      else operations.forEach((operation) => output(runtime, `${operation.status} ${operation.serviceId} ${operation.id}`));
      if (operations.some((operation) => operation.status === "failed")) fail(localctlExit.failed, "service operation failed");
      return 0;
    }
    if (command === "operation") {
      const flags = parseCommandFlags(rest, ["json"]);
      if (flags.positionals.length !== 2 || !["get", "watch"].includes(flags.positionals[0]!)) usage("usage: local-services operation get|watch <operationId> [--json]");
      const client = await requireClient(root, options, runtime);
      const operation = flags.positionals[0] === "watch" ? await waitOperation(client, flags.positionals[1]!, runtime) : ((await request(client, `/v1/operations/${operationId(flags.positionals[1]!)}`, {}, runtime)) as { operation: Operation }).operation;
      print(operation, flags.json, runtime);
      return 0;
    }
    if (command === "logs") {
      const flags = parseCommandFlags(rest, ["tail", "follow", "json"]);
      if (flags.positionals.length !== 1 || !options.catalog.services.some((service) => service.id === flags.positionals[0])) usage("usage: local-services logs <service> [--tail N] [--follow] [--json]");
      await logs(await requireClient(root, options, runtime), flags.positionals[0]!, flags.tail ?? 200, flags.follow, flags.json, options, runtime);
      return 0;
    }
    if (command === "tui") {
      const flags = parseCommandFlags(rest, []);
      if (flags.positionals.length) usage("tui takes no positional arguments");
      await ensure(root, options, runtime);
      const tui = runtime.tui;
      if (!tui) return usage("tui is not available: this build did not wire a `tui` runtime handler (see the /tui subpath)");
      return tui(root);
    }
    return usage(`unknown command: ${command}`);
  } catch (cause) {
    if (cause instanceof LocalctlError) {
      error(runtime, cause.message);
      return cause.exitCode;
    }
    throw cause;
  }
}
