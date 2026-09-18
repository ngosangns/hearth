import { randomUUID } from "node:crypto";

import { request, requireClient, type Client, type LocalctlOptions, type LocalctlRuntime } from "../cli/localctl";
import { managerProtocolVersion } from "../core/manager";
import type { ManagerEvent, Operation, ServiceLifecycleState } from "../core/state";
import { TuiState, type TuiFence } from "./state";

const reconnectDelayMs = 250;
export const logTailBytes = 16 * 1024;
export const maxSseFrameBytes = 64 * 1024;

type EventReplay = { epoch: string; reset: boolean; latestSequence: number };
type EventStream = (client: Client, after: number | undefined, epoch: string | undefined, onReplay: (replay: EventReplay) => void, onEvent: (event: ManagerEvent) => void, signal: AbortSignal) => Promise<void>;

export type TuiClientRuntime = Pick<LocalctlRuntime, "discover" | "request" | "sleep" | "now" | "readLockArtifacts" | "readOwnershipKey" | "directory"> & {
  eventStream?: EventStream;
  requestId?: () => string;
};

export type TuiWatchCallbacks<Fence> = {
  beginConnection(): Fence;
  snapshot(fence: Fence, services: ServiceLifecycleState[]): void;
  replay(fence: Fence, replay: EventReplay): void;
  event(fence: Fence, event: ManagerEvent): void;
  unavailable(fence: Fence, message: string): void;
};

/** A daemon HTTP+SSE client — the TUI has no direct access to processes, it's just another client
 * of the manager, same as the CLI (which this module reuses `request`/`requireClient` from). */
export class ManagerTuiClient {
  constructor(
    private readonly root: string,
    private readonly options: LocalctlOptions,
    private readonly runtime: TuiClientRuntime = {},
  ) {}

  async snapshot(): Promise<ServiceLifecycleState[]> {
    const client = await this.client();
    const response = await request(client, "/v1/services", {}, this.runtime);
    if (!isServices(response)) throw new Error("manager returned malformed service state");
    return response.services;
  }

  async log(serviceId: string, cursor?: number, generation?: number): Promise<{ data: string; nextCursor: number; generation: number; reset: boolean }> {
    const client = await this.client();
    const query = new URLSearchParams({ limit: String(logTailBytes), ...(cursor === undefined ? {} : { cursor: String(cursor) }), ...(generation === undefined ? {} : { generation: String(generation) }) });
    const response = await request(client, `/v1/logs/${encodeURIComponent(serviceId)}?${query}`, {}, this.runtime);
    if (!isLogSlice(response)) throw new Error("manager returned malformed log tail");
    return response;
  }

  async operation(operationId: string): Promise<Operation> {
    const response = await request(await this.client(), `/v1/operations/${encodeURIComponent(operationId)}`, {}, this.runtime);
    if (!isOperationResponse(response)) throw new Error("manager returned malformed operation");
    return response.operation;
  }

  async action(serviceId: string, action: "start" | "stop" | "restart"): Promise<Operation> {
    const response = await request(
      await this.client(),
      "/v1/operations",
      { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify({ requestId: (this.runtime.requestId ?? randomUUID)(), serviceId, action }) },
      this.runtime,
    );
    if (!isOperationResponse(response)) throw new Error("manager returned malformed operation");
    return response.operation;
  }

  async bulkStart(targets: readonly string[]): Promise<Operation> {
    const response = await request(
      await this.client(),
      "/v1/operations/bulk-start",
      { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify({ requestId: (this.runtime.requestId ?? randomUUID)(), targets }) },
      this.runtime,
    );
    if (!isOperationResponse(response)) throw new Error("manager returned malformed operation");
    return response.operation;
  }

  async waitOperation(id: string): Promise<Operation> {
    const sleep = this.runtime.sleep ?? Bun.sleep;
    for (;;) {
      const operation = await this.operation(id);
      if (operation.status === "succeeded" || operation.status === "failed") return operation;
      await sleep(100);
    }
  }

  async watch<Fence>(callbacks: TuiWatchCallbacks<Fence>, signal: AbortSignal): Promise<void> {
    let after: number | undefined;
    let epoch: string | undefined;
    while (!signal.aborted) {
      const fence = callbacks.beginConnection();
      try {
        const client = await this.client();
        const snapshot = await request(client, "/v1/services", {}, this.runtime);
        if (!isServices(snapshot)) throw new Error("manager returned malformed service state");
        callbacks.snapshot(fence, snapshot.services);
        await (this.runtime.eventStream ?? streamEvents)(
          client,
          after,
          epoch,
          (replay) => {
            epoch = replay.epoch;
            after = replay.reset ? replay.latestSequence : after;
            callbacks.replay(fence, replay);
          },
          (event) => {
            after = event.sequence;
            callbacks.event(fence, event);
          },
          signal,
        );
      } catch (error) {
        if (signal.aborted) return;
        callbacks.unavailable(fence, safeMessage(error));
      }
      if (!signal.aborted) await (this.runtime.sleep ?? Bun.sleep)(reconnectDelayMs);
    }
  }

  private async client(): Promise<Client> {
    return requireClient(this.root, this.options, this.runtime);
  }
}

export async function refreshSelectedLog(client: Pick<ManagerTuiClient, "log">, state: TuiState, fence: TuiFence): Promise<boolean> {
  const service = state.selection.selectedName;
  if (!service) return false;
  const cursor = state.logCursor(service);
  const delta = await client.log(service, cursor.cursor, cursor.generation);
  if (!state.applyLog(fence, service, delta)) return false;
  if (cursor.cursor === undefined || Buffer.byteLength(delta.data, "utf8") < logTailBytes) return true;
  const latest = await client.log(service);
  return state.replaceLog(fence, service, latest);
}

async function streamEvents(client: Client, after: number | undefined, epoch: string | undefined, onReplay: (replay: EventReplay) => void, onEvent: (event: ManagerEvent) => void, signal: AbortSignal): Promise<void> {
  const query = new URLSearchParams({ ...(after === undefined ? {} : { after: String(after) }), ...(epoch === undefined ? {} : { epoch }) });
  const response = await fetch(`http://127.0.0.1:${client.metadata.port}/v1/events/stream?${query}`, {
    headers: { authorization: `Bearer ${client.token}`, "x-local-services-protocol": String(managerProtocolVersion), accept: "text/event-stream" },
    signal,
  });
  if (!response.ok || !response.body) throw new Error(`event stream failed: ${response.status}`);
  const reader = response.body.getReader();
  const decoder = new TextDecoder();
  let buffer = "";
  try {
    while (!signal.aborted) {
      const { done, value } = await reader.read();
      if (done) return;
      buffer = appendSseChunk(buffer, decoder.decode(value, { stream: true }));
      let boundary = buffer.indexOf("\n\n");
      while (boundary >= 0) {
        const frame = buffer.slice(0, boundary);
        buffer = buffer.slice(boundary + 2);
        const event = parseSse(frame);
        if (!event) throw new Error("event stream frame is malformed");
        if (event.type === "replay" && isReplay(event.data)) onReplay(event.data);
        else if (isManagerEvent(event.data)) onEvent(event.data);
        else throw new Error("event stream payload is malformed");
        boundary = buffer.indexOf("\n\n");
      }
    }
  } finally {
    await reader.cancel().catch(() => undefined);
    reader.releaseLock();
  }
}

export function appendSseChunk(buffer: string, chunk: string): string {
  const next = `${buffer}${chunk}`;
  if (Buffer.byteLength(next, "utf8") > maxSseFrameBytes) throw new Error("event stream frame exceeds limit");
  return next;
}

export function parseSse(frame: string): { type: string; data: unknown } | undefined {
  if (Buffer.byteLength(frame, "utf8") > maxSseFrameBytes) return undefined;
  const type = frame.split("\n").find((line) => line.startsWith("event:"))?.slice("event:".length).trim() ?? "message";
  const raw = frame.split("\n").filter((line) => line.startsWith("data:")).map((line) => line.slice("data:".length).trim()).join("\n");
  if (!raw) return undefined;
  try {
    return { type, data: JSON.parse(raw) };
  } catch {
    return undefined;
  }
}

function isServices(value: unknown): value is { services: ServiceLifecycleState[] } {
  return (
    typeof value === "object" &&
    value !== null &&
    Array.isArray((value as { services?: unknown }).services) &&
    (value as { services: unknown[] }).services.every((service) => typeof service === "object" && service !== null && typeof (service as { serviceId?: unknown }).serviceId === "string" && typeof (service as { actualState?: unknown }).actualState === "string")
  );
}

function isLogSlice(value: unknown): value is { data: string; nextCursor: number; generation: number; reset: boolean } {
  return (
    typeof value === "object" &&
    value !== null &&
    typeof (value as { data?: unknown }).data === "string" &&
    Number.isInteger((value as { nextCursor?: unknown }).nextCursor) &&
    Number.isInteger((value as { generation?: unknown }).generation) &&
    typeof (value as { reset?: unknown }).reset === "boolean"
  );
}

function isOperationResponse(value: unknown): value is { operation: Operation } {
  return typeof value === "object" && value !== null && typeof (value as { operation?: unknown }).operation === "object" && (value as { operation: { id?: unknown } }).operation !== null && typeof (value as { operation: { id?: unknown } }).operation.id === "string";
}

function isReplay(value: unknown): value is EventReplay {
  return typeof value === "object" && value !== null && typeof (value as { epoch?: unknown }).epoch === "string" && typeof (value as { reset?: unknown }).reset === "boolean" && Number.isInteger((value as { latestSequence?: unknown }).latestSequence);
}

function isManagerEvent(value: unknown): value is ManagerEvent {
  return typeof value === "object" && value !== null && Number.isInteger((value as { sequence?: unknown }).sequence) && typeof (value as { type?: unknown }).type === "string" && typeof (value as { data?: unknown }).data === "object" && (value as { data: unknown }).data !== null;
}

function safeMessage(error: unknown): string {
  return error instanceof Error ? error.message.replace(/[\r\n\x00-\x1f\x7f]/g, " ") : "manager unavailable";
}
