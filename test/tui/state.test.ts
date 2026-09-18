import { describe, expect, test } from "bun:test";

import { TuiState } from "../../src/tui/state";
import type { Operation, ServiceLifecycleState } from "../../src/core/state";

const service = (): ServiceLifecycleState => ({ serviceId: "metadata", desiredState: "running", actualState: "ready", readiness: "ready", generation: 1, createdAt: "2026-09-08T00:00:00.000Z", updatedAt: "2026-09-08T00:00:00.000Z" });
const operation = (): Operation => ({ id: "start-1", requestId: "request-1", kind: "service", serviceId: "metadata", action: "start", status: "queued", createdAt: "2026-09-08T00:00:00.000Z", updatedAt: "2026-09-08T00:00:00.000Z", trace: [] });

describe("TUI state", () => {
  test("shows all profile-free services and lifecycle state", () => {
    const state = new TuiState();
    const fence = state.beginConnection();
    expect(state.applySnapshot(fence, [service()])).toBe(true);
    expect(state.selection.selectedName).toBe("metadata");
    expect(state.detail()).toContain("generation=1");
    expect(state.detail()).not.toContain("reload");
  });

  test("keeps queued-start lifecycle snapshots distinct from stopped services", () => {
    const state = new TuiState();
    const fence = state.beginConnection();

    expect(state.applySnapshot(fence, [{ ...service(), actualState: "queued-start", readiness: "unknown", generation: 0 }])).toBe(true);
    expect(state.selection.selected()).toMatchObject({ state: "queued-start", generation: 0 });
    expect(state.detail()).toContain("state=queued-start");
  });

  test("clears retained logs when the selected service starts", () => {
    const state = new TuiState();
    const fence = state.beginConnection();
    state.applySnapshot(fence, [service()]);
    state.applyLog(fence, "metadata", { data: "old output\n", generation: 1, nextCursor: 11, reset: false });
    const action = state.beginAction("metadata", "start");
    expect(state.applyAction(action, operation())).toBe(true);
    expect(state.log).toBe("No log yet.");
    expect(state.logCursor("metadata")).toEqual({ cursor: 11, generation: 1 });
  });

  test("selects only a known service", () => {
    const state = new TuiState();
    const fence = state.beginConnection();
    state.applySnapshot(fence, [service(), { ...service(), serviceId: "question" }]);

    expect(state.selection.select("question")).toBe(true);
    expect(state.selection.selectedName).toBe("question");
    expect(state.selection.select("missing")).toBe(false);
    expect(state.selection.selectedName).toBe("question");
  });

  test("looks up service kind through the injected lookup", () => {
    const state = new TuiState((id) => (id === "mongo" ? "infrastructure" : "application"));
    const fence = state.beginConnection();
    state.applySnapshot(fence, [{ ...service(), serviceId: "mongo" }]);
    expect(state.selection.selected()?.kind).toBe("infrastructure");
  });
});
