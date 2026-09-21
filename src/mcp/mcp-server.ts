import { randomUUID } from "node:crypto";

import { Server } from "@modelcontextprotocol/sdk/server/index.js";
import { CallToolRequestSchema, ListToolsRequestSchema } from "@modelcontextprotocol/sdk/types.js";

import { requireClient, request, runnableTargets, targets, type LocalctlOptions } from "../cli/localctl";
import { managerProtocolVersion } from "../core/manager";
import type { ServiceId } from "../core/catalog";
import type { Operation } from "../core/state";

const SECRET_KEY_PATTERN = /authorization|token|ownership(?:key|proof)|secret|password|api[_-]?key/i;

type ManageAction = "start" | "stop" | "restart";
type JsonObject = Record<string, unknown>;

export type StatusArguments = { service?: string };
export type LogsArguments = { service: string; cursor?: number; generation?: number; limit?: number };
export type TraceArguments = { operationId: string };
export type EventsArguments = { after?: number; epoch?: string };
export type ManageArguments = { service: string; action: ManageAction };

/** The dependency-injected MCP client shape a reusable package's MCP entrypoint needs: transport-
 * agnostic and unit-testable with a fake, rather than `fetch`-ing the daemon inline in a switch
 * statement. */
export interface LocalServicesMcpClient {
  status(arguments_: StatusArguments): Promise<unknown>;
  logs(arguments_: LogsArguments): Promise<unknown>;
  trace(arguments_: TraceArguments): Promise<unknown>;
  events(arguments_: EventsArguments): Promise<unknown>;
  manage(arguments_: ManageArguments): Promise<unknown>;
}

const isObject = (value: unknown): value is JsonObject => typeof value === "object" && value !== null && !Array.isArray(value);
function requireObject(value: unknown): JsonObject {
  if (!isObject(value)) throw new Error("arguments must be an object");
  return value;
}
function requireOnlyKeys<T extends JsonObject>(value: T, allowed: readonly (keyof T & string)[]): T {
  const unexpected = Object.keys(value).filter((key) => !allowed.includes(key as keyof T & string));
  if (unexpected.length) throw new Error(`unexpected argument${unexpected.length > 1 ? "s" : ""}: ${unexpected.join(", ")}`);
  return value;
}
function optionalString(value: unknown, name: string): string | undefined {
  if (value === undefined) return undefined;
  if (typeof value !== "string" || value.length === 0) throw new Error(`${name} must be a non-empty string`);
  return value;
}
function requiredString(value: unknown, name: string): string {
  const result = optionalString(value, name);
  if (result === undefined) throw new Error(`${name} is required`);
  return result;
}
function optionalInteger(value: unknown, name: string, minimum: number, maximum?: number): number | undefined {
  if (value === undefined) return undefined;
  if (!Number.isSafeInteger(value) || (value as number) < minimum || (maximum !== undefined && (value as number) > maximum)) {
    const range = maximum === undefined ? `at least ${minimum}` : `between ${minimum} and ${maximum}`;
    throw new Error(`${name} must be an integer ${range}`);
  }
  return value as number;
}
function requireEnum<T extends string>(value: unknown, name: string, allowed: readonly T[]): T {
  if (typeof value !== "string" || !allowed.includes(value as T)) throw new Error(`${name} must be one of: ${allowed.join(", ")}`);
  return value as T;
}

function redact(value: unknown, depth = 0): unknown {
  if (depth > 12) return "[truncated]";
  if (Array.isArray(value)) return value.map((entry) => redact(entry, depth + 1));
  if (!isObject(value)) return value;
  return Object.fromEntries(Object.entries(value).filter(([key]) => !SECRET_KEY_PATTERN.test(key)).map(([key, entry]) => [key, redact(entry, depth + 1)]));
}
function safeErrorMessage(error: unknown): string {
  if (!(error instanceof Error)) return "local services request failed";
  return error.message.replace(/Bearer\s+\S+/gi, "Bearer [redacted]");
}
function result(value: unknown) {
  return { content: [{ type: "text" as const, text: JSON.stringify(redact(value), null, 2) }] };
}
function errorResult(error: unknown) {
  return { isError: true, content: [{ type: "text" as const, text: safeErrorMessage(error) }] };
}

export type CreateLocalServicesMcpServerOptions = {
  /** MCP server name registered with the SDK, e.g. `"local-services"`. */
  name: string;
  version?: string;
  /** Every tool is registered as `${toolPrefix}status`, `${toolPrefix}logs`, etc. Default `""`. */
  toolPrefix?: string;
  /** Require a schema-enforced `confirm: true` argument on the manage tool (start/stop/restart) —
   * the safer default: an MCP host that ignores prose advice still can't invoke it accidentally,
   * because the tool's own JSON schema demands the field. Default `true`. */
  requireConfirm?: boolean;
  knownServiceIds: readonly ServiceId[];
};

function parseArguments(options: CreateLocalServicesMcpServerOptions) {
  const serviceIds = new Set(options.knownServiceIds);
  const requireService = (value: unknown): string => {
    const service = requiredString(value, "service");
    if (!serviceIds.has(service)) throw new Error(`unknown service: ${service}. Known services: ${[...serviceIds].join(", ")}`);
    return service;
  };
  return {
    status: (value: unknown): StatusArguments => {
      const arguments_ = requireOnlyKeys(requireObject(value ?? {}), ["service"]);
      return { service: arguments_.service === undefined ? undefined : requireService(arguments_.service) };
    },
    logs: (value: unknown): LogsArguments => {
      const arguments_ = requireOnlyKeys(requireObject(value ?? {}), ["service", "cursor", "generation", "limit"]);
      return { service: requireService(arguments_.service), cursor: optionalInteger(arguments_.cursor, "cursor", 0), generation: optionalInteger(arguments_.generation, "generation", 1), limit: optionalInteger(arguments_.limit, "limit", 1, 64 * 1024) };
    },
    trace: (value: unknown): TraceArguments => ({ operationId: requiredString(requireOnlyKeys(requireObject(value ?? {}), ["operationId"]).operationId, "operationId") }),
    events: (value: unknown): EventsArguments => {
      const arguments_ = requireOnlyKeys(requireObject(value ?? {}), ["after", "epoch"]);
      return { after: optionalInteger(arguments_.after, "after", 0), epoch: optionalString(arguments_.epoch, "epoch") };
    },
    manage: (value: unknown): ManageArguments => {
      const arguments_ = requireOnlyKeys(requireObject(value ?? {}), options.requireConfirm !== false ? ["service", "action", "confirm"] : ["service", "action"]);
      if (options.requireConfirm !== false && arguments_.confirm !== true) throw new Error("manage requires confirm=true (explicit user approval) — never call this speculatively");
      return { service: requireService(arguments_.service), action: requireEnum(arguments_.action, "action", ["start", "stop", "restart"] as const) };
    },
  };
}

export function createLocalServicesMcpServer(client: LocalServicesMcpClient, options: CreateLocalServicesMcpServerOptions): Server {
  const prefix = options.toolPrefix ?? "";
  const requireConfirm = options.requireConfirm !== false;
  const parse = parseArguments(options);
  const server = new Server({ name: options.name, version: options.version ?? "1.0.0" }, { capabilities: { tools: {} } });

  server.setRequestHandler(ListToolsRequestSchema, async () => ({
    tools: [
      { name: `${prefix}status`, description: "Read-only status of local dev services. No approval needed.", inputSchema: { type: "object", properties: { service: { type: "string", enum: options.knownServiceIds, description: "Service id. Omit to list all." } }, additionalProperties: false } },
      {
        name: `${prefix}logs`,
        description: "Read-only bounded log chunk for one service. No approval needed.",
        inputSchema: { type: "object", properties: { service: { type: "string", enum: options.knownServiceIds }, cursor: { type: "integer", minimum: 0 }, generation: { type: "integer", minimum: 1 }, limit: { type: "integer", minimum: 1, maximum: 64 * 1024 } }, required: ["service"], additionalProperties: false },
      },
      { name: `${prefix}trace`, description: "Read-only trace/status of a start/stop/restart operation by id. No approval needed.", inputSchema: { type: "object", properties: { operationId: { type: "string" } }, required: ["operationId"], additionalProperties: false } },
      { name: `${prefix}events`, description: "Read-only recent manager events since a sequence number. Not a follow/SSE stream. No approval needed.", inputSchema: { type: "object", properties: { after: { type: "integer", minimum: 0 }, epoch: { type: "string" } }, additionalProperties: false } },
      {
        name: `${prefix}manage`,
        description: `Start/stop/restart a local dev service.${requireConfirm ? " Requires confirm=true (explicit user approval) — never call this speculatively." : " MCP hosts should require approval for this tool."}`,
        inputSchema: {
          type: "object",
          properties: { service: { type: "string", enum: options.knownServiceIds }, action: { type: "string", enum: ["start", "stop", "restart"] }, ...(requireConfirm ? { confirm: { type: "boolean", description: "Must be true; the user must have explicitly asked for this action" } } : {}) },
          required: requireConfirm ? ["service", "action", "confirm"] : ["service", "action"],
          additionalProperties: false,
        },
      },
    ],
  }));

  server.setRequestHandler(CallToolRequestSchema, async (request_) => {
    try {
      const name = request_.params.name;
      const arguments_ = request_.params.arguments;
      if (name === `${prefix}status`) return result(await client.status(parse.status(arguments_)));
      if (name === `${prefix}logs`) return result(await client.logs(parse.logs(arguments_)));
      if (name === `${prefix}trace`) return result(await client.trace(parse.trace(arguments_)));
      if (name === `${prefix}events`) return result(await client.events(parse.events(arguments_)));
      if (name === `${prefix}manage`) return result(await client.manage(parse.manage(arguments_)));
      return errorResult(new Error(`unknown tool: ${name}`));
    } catch (error) {
      return errorResult(error);
    }
  });

  return server;
}

/** `/v1/urls`' body narrowed to one service's entries when `status` was asked about one service. */
function filterServiceUrls(value: unknown, service?: string): unknown {
  if (service === undefined || !isObject(value)) return value;
  const keep = (entries: unknown) => (Array.isArray(entries) ? entries.filter((entry) => isObject(entry) && entry.serviceId === service) : entries);
  return { ...value, urls: keep(value.urls), unresolved: keep(value.unresolved) };
}
function filterServiceStates(value: unknown, service?: string): unknown {
  if (service === undefined) return value;
  if (Array.isArray(value)) return value.filter((entry) => isObject(entry) && entry.serviceId === service);
  if (isObject(value) && Array.isArray(value.services)) return { ...value, services: filterServiceStates(value.services, service) };
  return value;
}
function queryString(values: Record<string, string | number | undefined>): string {
  const search = new URLSearchParams();
  for (const [key, value] of Object.entries(values)) if (value !== undefined) search.set(key, String(value));
  const query = search.toString();
  return query.length === 0 ? "" : `?${query}`;
}

/** Default `LocalServicesMcpClient` backed by the daemon's HTTP API via `@gnasdev/local-services/cli`. */
export class ManagerApiClient implements LocalServicesMcpClient {
  constructor(
    private readonly root: string,
    private readonly options: LocalctlOptions,
  ) {}
  private async call(path: string, init?: RequestInit): Promise<unknown> {
    const client = await requireClient(this.root, this.options);
    return request(client, path, init);
  }
  async status(arguments_: StatusArguments): Promise<unknown> {
    const client = await requireClient(this.root, this.options);
    const [manager, services] = await Promise.all([request(client, "/v1/manager"), request(client, "/v1/services")]);
    // Additive, and best-effort: a daemon from before `/v1/urls` existed still answers `status`,
    // just without `urls`, rather than failing the whole call.
    const urls = await request(client, "/v1/urls").then((body) => filterServiceUrls(body, arguments_.service), () => undefined);
    return { manager, services: filterServiceStates(services, arguments_.service), ...(urls !== undefined ? { urls } : {}) };
  }
  logs(arguments_: LogsArguments): Promise<unknown> {
    return this.call(`/v1/logs/${encodeURIComponent(arguments_.service)}${queryString({ cursor: arguments_.cursor, generation: arguments_.generation, limit: arguments_.limit })}`);
  }
  trace(arguments_: TraceArguments): Promise<unknown> {
    return this.call(`/v1/operations/${encodeURIComponent(arguments_.operationId)}`);
  }
  events(arguments_: EventsArguments): Promise<unknown> {
    return this.call(`/v1/events${queryString(arguments_)}`);
  }
  async manage(arguments_: ManageArguments): Promise<unknown> {
    runnableTargets(this.options.catalog, arguments_.service);
    const requestId = randomUUID();
    let operation = ((await this.call("/v1/operations", { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify({ requestId, serviceId: arguments_.service, action: arguments_.action }) })) as { operation: Operation }).operation;
    while (operation.status === "queued" || operation.status === "running") {
      await Bun.sleep(300);
      operation = ((await this.call(`/v1/operations/${operation.id}`)) as { operation: Operation }).operation;
    }
    if (operation.status === "failed") throw new Error(operation.error?.message ?? "operation failed");
    return this.status({ service: arguments_.service });
  }
}

export const mcpProtocolVersion = managerProtocolVersion;
