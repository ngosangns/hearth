import { describe, expect, test } from "bun:test";
import { Client } from "@modelcontextprotocol/sdk/client/index.js";
import { InMemoryTransport } from "@modelcontextprotocol/sdk/inMemory.js";

import { createLocalServicesMcpServer, type CreateLocalServicesMcpServerOptions, type LocalServicesMcpClient, type ManageArguments } from "../../src/mcp/mcp-server";

class FakeClient implements LocalServicesMcpClient {
  managed: ManageArguments | undefined;
  status(): Promise<unknown> {
    return Promise.resolve({});
  }
  logs(): Promise<unknown> {
    return Promise.resolve({});
  }
  trace(): Promise<unknown> {
    return Promise.resolve({});
  }
  events(): Promise<unknown> {
    return Promise.resolve({});
  }
  manage(arguments_: ManageArguments): Promise<unknown> {
    this.managed = arguments_;
    return Promise.resolve({ operation: "accepted" });
  }
}

const baseOptions: CreateLocalServicesMcpServerOptions = { name: "test-local-services", toolPrefix: "local_services_", knownServiceIds: ["metadata", "mongo"] };

async function connect(implementation: LocalServicesMcpClient, options: CreateLocalServicesMcpServerOptions = baseOptions) {
  const server = createLocalServicesMcpServer(implementation, options);
  const client = new Client({ name: "test", version: "1.0.0" });
  const [left, right] = InMemoryTransport.createLinkedPair();
  await Promise.all([server.connect(right), client.connect(left)]);
  return { server, client };
}

describe("local services MCP", () => {
  test("routes focused application management through the ordinary manager client", async () => {
    const implementation = new FakeClient();
    const { server, client } = await connect(implementation);
    const response = await client.callTool({ name: "local_services_manage", arguments: { service: "metadata", action: "restart", confirm: true } });
    expect(response.isError).not.toBe(true);
    expect(implementation.managed).toEqual({ service: "metadata", action: "restart" });
    await Promise.all([client.close(), server.close()]);
  });

  test("routes focused infrastructure management through the ordinary manager client", async () => {
    const implementation = new FakeClient();
    const { server, client } = await connect(implementation);
    const response = await client.callTool({ name: "local_services_manage", arguments: { service: "mongo", action: "restart", confirm: true } });
    expect(response.isError).not.toBe(true);
    expect(implementation.managed).toEqual({ service: "mongo", action: "restart" });
    await Promise.all([client.close(), server.close()]);
  });

  test("rejects unexpected extra arguments (generalizes viclass's removed-profile rejection)", async () => {
    const { server, client } = await connect(new FakeClient());
    const response = await client.callTool({ name: "local_services_manage", arguments: { service: "metadata", action: "restart", confirm: true, profile: "dev" } });
    expect(response.isError).toBe(true);
    await Promise.all([client.close(), server.close()]);
  });

  test("rejects an unknown service id", async () => {
    const { server, client } = await connect(new FakeClient());
    const response = await client.callTool({ name: "local_services_manage", arguments: { service: "not-a-real-service", action: "restart", confirm: true } });
    expect(response.isError).toBe(true);
    await Promise.all([client.close(), server.close()]);
  });

  test("manage requires confirm=true by default — the schema-enforced default recommended for the package (infra's fix)", async () => {
    const implementation = new FakeClient();
    const { server, client } = await connect(implementation);
    const response = await client.callTool({ name: "local_services_manage", arguments: { service: "metadata", action: "restart" } });
    expect(response.isError).toBe(true);
    expect(implementation.managed).toBeUndefined();
    await Promise.all([client.close(), server.close()]);
  });

  test("manage skips the confirm requirement when requireConfirm is explicitly disabled", async () => {
    const implementation = new FakeClient();
    const { server, client } = await connect(implementation, { ...baseOptions, requireConfirm: false });
    const response = await client.callTool({ name: "local_services_manage", arguments: { service: "metadata", action: "restart" } });
    expect(response.isError).not.toBe(true);
    expect(implementation.managed).toEqual({ service: "metadata", action: "restart" });
    await Promise.all([client.close(), server.close()]);
  });

  test("read-only tools (status/logs/trace/events) need no confirm and are registered under the configured prefix", async () => {
    const { server, client } = await connect(new FakeClient());
    const tools = await client.listTools();
    const names = tools.tools.map((tool) => tool.name);
    expect(names).toEqual(["local_services_status", "local_services_logs", "local_services_trace", "local_services_events", "local_services_manage"]);
    const status = await client.callTool({ name: "local_services_status", arguments: {} });
    expect(status.isError).not.toBe(true);
    await Promise.all([client.close(), server.close()]);
  });
});
