import type { ServiceKind } from "../core/catalog";
import type { Operation, ServiceLifecycleState } from "../core/state";

export type Service = {
  name: string;
  kind?: ServiceKind;
  state: string;
  generation?: number;
  currentOperationId?: string;
};

export type TuiFence = { connection: number; request: number; selection: number };
export type ActionFence = TuiFence & { service: string; action: "start" | "stop" | "restart" };
type LogSlice = { data: string; nextCursor: number; generation: number; reset: boolean };
type LogStream = { data: string; cursor?: number; generation?: number };

/** Looks up a service's `kind` for display — pass the consumer's `ServiceCatalog` (e.g.
 * `(id) => catalog.services.find((s) => s.id === id)?.kind`), or omit for no kind badges. */
export type ServiceKindLookup = (serviceId: string) => ServiceKind | undefined;

export function serviceFromLifecycle(service: ServiceLifecycleState, serviceKind: ServiceKindLookup = () => undefined): Service {
  return {
    name: service.serviceId,
    kind: serviceKind(service.serviceId),
    state: service.actualState === "running-unready" ? "degraded" : service.actualState,
    generation: service.generation,
    currentOperationId: service.currentOperationId,
  };
}

export function boundedTail(current: string, next: string, limit = 16 * 1024): string {
  const combined = `${current}${next}`;
  return combined.length <= limit ? combined : combined.slice(combined.length - limit);
}

export class ServiceSelection {
  services: Service[] = [];
  selectedName = "";

  setServices(services: Service[]): void {
    this.services = services;
    if (!services.some((service) => service.name === this.selectedName)) this.selectedName = services[0]?.name ?? "";
  }

  move(delta: number): boolean {
    const index = this.services.findIndex((service) => service.name === this.selectedName);
    const next = index + delta;
    if (next < 0 || next >= this.services.length) return false;
    this.selectedName = this.services[next]!.name;
    return true;
  }

  select(name: string): boolean {
    if (name === this.selectedName || !this.services.some((service) => service.name === name)) return false;
    this.selectedName = name;
    return true;
  }

  selected(): Service | undefined {
    return this.services.find((service) => service.name === this.selectedName);
  }
}

/** Pure TUI state with connection, response, selection, and action fences. */
export class TuiState {
  readonly selection = new ServiceSelection();
  log = "Loading…";
  operation: Operation | undefined;
  notice = "";
  readonly #logs = new Map<string, LogStream>();
  readonly #serviceKind: ServiceKindLookup;
  #connection = 0;
  #request = 0;
  #selection = 0;

  constructor(serviceKind: ServiceKindLookup = () => undefined) {
    this.#serviceKind = serviceKind;
  }

  beginConnection(): TuiFence {
    this.#connection++;
    this.#request++;
    return { connection: this.#connection, request: this.#request, selection: this.#selection };
  }

  beginRequest(): TuiFence {
    this.#request++;
    return { connection: this.#connection, request: this.#request, selection: this.#selection };
  }

  beginSelection(): TuiFence {
    this.#selection++;
    this.#request++;
    this.syncSelectedLog();
    this.operation = undefined;
    return { connection: this.#connection, request: this.#request, selection: this.#selection };
  }

  beginAction(service: string, action: ActionFence["action"]): ActionFence {
    return { ...this.beginRequest(), service, action };
  }

  current(fence: TuiFence): boolean {
    return this.connected(fence) && fence.request === this.#request && fence.selection === this.#selection;
  }

  connected(fence: TuiFence): boolean {
    return fence.connection === this.#connection;
  }

  currentAction(fence: ActionFence): boolean {
    return this.current(fence) && this.selection.selectedName === fence.service;
  }

  applySnapshot(fence: TuiFence, services: ServiceLifecycleState[]): boolean {
    if (!this.current(fence)) return false;
    const selected = this.selection.selectedName;
    this.selection.setServices(services.map((service) => serviceFromLifecycle(service, this.#serviceKind)));
    if (selected !== this.selection.selectedName) this.syncSelectedLog();
    return true;
  }

  logCursor(service: string): Pick<LogStream, "cursor" | "generation"> {
    const stream = this.#logs.get(service);
    return { cursor: stream?.cursor, generation: stream?.generation };
  }

  applyLog(fence: TuiFence, service: string, slice: LogSlice): boolean {
    if (!this.current(fence) || service !== this.selection.selectedName) return false;
    const current = this.#logs.get(service);
    const replace = slice.reset || current?.generation !== slice.generation || current?.cursor === undefined;
    const stream = { data: replace ? boundedTail("", slice.data) : boundedTail(current!.data, slice.data), cursor: slice.nextCursor, generation: slice.generation };
    this.#logs.set(service, stream);
    this.log = stream.data || "No log yet.";
    return true;
  }

  replaceLog(fence: TuiFence, service: string, slice: LogSlice): boolean {
    if (!this.current(fence) || service !== this.selection.selectedName) return false;
    const stream = { data: boundedTail("", slice.data), cursor: slice.nextCursor, generation: slice.generation };
    this.#logs.set(service, stream);
    this.log = stream.data || "No log yet.";
    return true;
  }

  applyOperation(fence: TuiFence, operation: Operation): boolean {
    if (!this.current(fence) || operation.serviceId !== this.selection.selectedName) return false;
    this.operation = operation;
    return true;
  }

  applyAction(fence: ActionFence, operation: Operation): boolean {
    if (!this.currentAction(fence) || operation.serviceId !== fence.service || operation.action !== fence.action) return false;
    this.operation = operation;
    if (fence.action === "start") this.clearLog(fence.service);
    this.notice = `${fence.action} ${fence.service}: ${operation.id}`;
    return true;
  }

  applyActionFailure(fence: ActionFence, error: string): boolean {
    if (!this.currentAction(fence)) return false;
    this.notice = `${fence.action} ${fence.service} failed: ${error}`;
    return true;
  }

  applyEvent(fence: TuiFence, type: string, data: Record<string, unknown>): boolean {
    if (!this.connected(fence)) return false;
    const serviceId = typeof data.serviceId === "string" ? data.serviceId : undefined;
    const service = serviceId ? this.selection.services.find((candidate) => candidate.name === serviceId) : undefined;
    if (service && type === "service.lifecycle") {
      service.state = typeof data.actualState === "string" ? (data.actualState === "running-unready" ? "degraded" : data.actualState) : service.state;
      service.generation = typeof data.generation === "number" ? data.generation : service.generation;
      service.currentOperationId = typeof data.operationId === "string" ? data.operationId : undefined;
    }
    return true;
  }

  detail(): string {
    const service = this.selection.selected();
    if (!service) return "No services.";
    const parts = [`state=${service.state}`, `generation=${service.generation ?? 0}`];
    if (this.operation?.serviceId === service.name) {
      parts.push(`operation=${this.operation.id} ${this.operation.status}`);
      parts.push(...this.operation.trace.map((entry) => `${entry.at} ${entry.message}`));
      if (this.operation.error) parts.push(`operation error=${this.operation.error.message}`);
    }
    return parts.join("\n");
  }

  private clearLog(service: string): void {
    const previous = this.#logs.get(service);
    this.#logs.set(service, { data: "", cursor: previous?.cursor, generation: previous?.generation });
    if (service === this.selection.selectedName) this.log = "No log yet.";
  }

  private syncSelectedLog(): void {
    this.log = this.#logs.get(this.selection.selectedName)?.data || "Loading…";
  }
}
