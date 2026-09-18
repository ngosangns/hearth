import { describe, expect, test } from "bun:test";

import { ProcessSupervisor, type ManagedProcess, type ObservedProcess, type SpawnInput, type SupervisorOptions } from "../../src/core/supervisor";
import type { ServiceCatalog, ServiceId } from "../../src/core/catalog";
import type { ServiceLifecycleState } from "../../src/core/state";

describe("one-profile manager integration", () => {
  test("explicit restart invokes the build-and-serve command again", async () => {
    const inputs: SpawnInput[] = [];
    const state = new Map<ServiceId, ServiceLifecycleState>();
    let pid = 1;
    const alive = new Set<number>();
    const catalog: ServiceCatalog = {
      startFailurePolicy: "stop-on-first-failure-keep-started",
      services: [{ id: "metadata", profiles: { run: { commandStatus: "verified", command: { command: { shell: "build && serve" }, cwd: "." }, readiness: { kind: "process" } } } }],
      groups: { one: ["metadata"] },
    };
    const options: SupervisorOptions = {
      process: {
        spawn: async (input): Promise<ManagedProcess> => {
          inputs.push(input);
          const exited = Promise.withResolvers<number>();
          const current = pid++;
          alive.add(current);
          return { pid: current, pgid: current, startIdentity: String(current), commandFingerprint: input.commandFingerprint, exited: exited.promise };
        },
        inspect: async (identity): Promise<ObservedProcess | undefined> =>
          "containerId" in identity
            ? { containerName: identity.containerName, containerId: identity.containerId, containerStartedAt: identity.containerStartedAt, commandFingerprint: identity.commandFingerprint, alive: true }
            : alive.has(identity.pid)
              ? { pid: identity.pid, pgid: identity.pgid, startIdentity: identity.startIdentity, commandFingerprint: identity.commandFingerprint, alive: true }
              : undefined,
        signalGroup: async (pgid) => {
          alive.delete(pgid);
        },
      },
      runBuild: async () => undefined,
      probes: { tcp: async () => true, http: async () => true, container: async () => true, tailnet: async () => true },
    };
    const supervisor = new ProcessSupervisor(
      { instanceId: "test", catalog, serviceStates: () => [...state.values()], setServiceState: async (next) => { state.set(next.serviceId, next); }, appendLog: async () => undefined, publish: () => undefined },
      options,
    );
    await supervisor.start("metadata");
    await supervisor.restart("metadata");
    expect(inputs.map((input) => ("shell" in input.command.command ? input.command.command.shell : input.command.command.argv.join(" ")))).toEqual(["build && serve", "build && serve"]);
    expect(state.get("metadata")?.generation).toBe(2);
  });
});
