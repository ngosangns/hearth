import { ProcessTerminal, routeSgrMouseInput, TUI, type TerminalFrameProvider } from "@oh-my-pi/pi-tui";

import { ensure, type LocalctlOptions } from "../cli/localctl";
import type { ResolvedServiceUrl, ServiceCatalog } from "../core/catalog";
import { ServiceScreen } from "./screen";
import { TuiState, type ServiceKindLookup, type TuiFence } from "./state";
import { keyboardAction } from "./tui-actions";
import { ManagerTuiClient, refreshSelectedLog } from "./tui-client";

const mouseTrackingOn = "\x1b[?1000h\x1b[?1006h";
const mouseTrackingOff = "\x1b[?1006l\x1b[?1000l";

export type RunTuiOptions = {
  root: string;
  catalog: ServiceCatalog;
  /** Same shape as `LocalctlOptions.spawnDaemon` — spawns the project's own daemon entry, detached. */
  spawnDaemon: (root: string) => void;
  /** Background reconciliation poll interval; the SSE event stream is the primary update path. */
  refreshIntervalMs?: number;
  /** Looks up a service's `kind` for display. Defaults to a catalog lookup. */
  serviceKind?: ServiceKindLookup;
};

function message(error: unknown): string {
  return error instanceof Error ? error.message.replace(/[\r\n\x00-\x1f\x7f]/g, " ") : "manager unavailable";
}

/** Boots a `pi-tui` terminal app that is just another HTTP+SSE client of the daemon — it has no
 * direct access to processes. Resolves with an exit code once the user quits (`q`/Ctrl-C). Wire
 * this in as a consumer's `LocalctlRuntime.tui` handler (`(root) => runTui({ root, catalog, spawnDaemon })`)
 * so `local-services tui` (the `/cli` subpath's own `tui` command) can launch it, or call it directly. */
export async function runTui(options: RunTuiOptions): Promise<number> {
  const localctlOptions: LocalctlOptions = { catalog: options.catalog, spawnDaemon: options.spawnDaemon };
  await ensure(options.root, localctlOptions);
  const serviceKindLookup: ServiceKindLookup = options.serviceKind ?? ((id) => options.catalog.services.find((service) => service.id === id)?.kind);
  const refreshIntervalMs = options.refreshIntervalMs ?? 10_000;
  const allTargets = options.catalog.groups.all ?? options.catalog.services.map((service) => service.id);

  const state = new TuiState(serviceKindLookup);
  const terminal = new ProcessTerminal();
  const tui = new TUI(terminal);
  const screen = new ServiceScreen(terminal);
  const client = new ManagerTuiClient(options.root, localctlOptions);
  const streamAbort = new AbortController();
  let busy = false;
  let disposed = false;
  let urls: ResolvedServiceUrl[] = [];

  /** The focused service's URLs as screen rows, flagging the ones that need the service running
   * while it is not — the same rule as `lsd urls`. */
  const focusedUrls = (): string => {
    const selected = state.selection.selectedName;
    const current = state.selection.services.find((service) => service.name === selected);
    const running = ["ready", "running", "degraded", "starting", "preparing"].includes(current?.state ?? "");
    return urls
      .filter((entry) => entry.serviceId === selected)
      .map((entry) => `${entry.label ?? "-"}  ${entry.url}${entry.requiresRunning && !running ? "  (not running)" : ""}`)
      .join("\n");
  };

  return new Promise<number>((resolveExit) => {
    function render(): void {
      screen.update({
        services: state.selection.services,
        selectedName: state.selection.selectedName,
        logService: state.selection.selectedName,
        log: `${state.detail()}\n${state.log}`,
        notice: state.notice,
        urls: focusedUrls(),
      });
      tui.requestRender();
    }

    tui.setFrameProvider({
      renderFrame: ({ columns, rows }) => ({ viewport: screen.render(columns, rows) }),
      acknowledgeHistory: () => {},
    } satisfies TerminalFrameProvider);
    tui.addInputListener(handleInput);
    tui.start();
    // Request SGR button reports solely to receive wheel input. Clicks and drags are deliberately
    // never handled, so Shift-drag remains available for native terminal selection.
    terminal.write(mouseTrackingOn);

    void client.watch(
      {
        beginConnection: () => state.beginConnection(),
        snapshot: (fence, services) => {
          if (!state.applySnapshot(fence, services)) return;
          state.notice = "";
          render();
          void refreshSelected(state.beginRequest());
        },
        replay: (fence, replay) => {
          if (!state.connected(fence)) return;
          if (replay.reset) state.notice = "Manager restarted; state resynchronized.";
          render();
        },
        event: (fence, event) => {
          if (!state.applyEvent(fence, event.type, event.data)) return;
          render();
          if (event.type === "service.log" && event.data.serviceId === state.selection.selectedName) void refreshSelected(state.beginRequest());
          if (event.type === "operation.updated" && event.data.serviceId === state.selection.selectedName) void refreshOperation(state.beginRequest());
        },
        unavailable: (fence, msg) => {
          if (!state.connected(fence)) return;
          state.notice = `Manager unavailable: ${msg}`;
          render();
        },
      },
      streamAbort.signal,
    );
    const timer = setInterval(() => void reconcile(), refreshIntervalMs);

    function selectService(name: string | undefined): void {
      if (!name || !state.selection.select(name)) return;
      const fence = state.beginSelection();
      render();
      void refreshSelected(fence);
    }

    function handleInput(data: string): { consume: true } | undefined {
      if (
        routeSgrMouseInput(data, (event) => {
          if (event.wheel !== null) {
            const service = screen.handleWheel(event.row, event.wheel, terminal.rows);
            if (!service) return false;
            selectService(service);
            tui.requestRender();
            return true;
          }
          if (event.leftClick) {
            const service = screen.serviceAt(event.row, terminal.rows);
            if (!service) return false;
            selectService(service);
            return true;
          }
          return false;
        })
      )
        return { consume: true };
      const action = keyboardAction(data, state.selection.selected());
      if (!action) return undefined;
      if (action === "quit") shutdown();
      else if (action === "up" || action === "down") {
        if (state.selection.move(action === "up" ? -1 : 1)) {
          const fence = state.beginSelection();
          render();
          void refreshSelected(fence);
        }
      } else if (action === "start-all" || action === "stop-all") void runAll(action.slice(0, -4) as "start" | "stop");
      else if (action === "start" || action === "stop" || action === "restart") void runAction(action, state.selection.selectedName);
      return { consume: true };
    }

    async function reconcile(): Promise<void> {
      if (disposed || busy) return;
      const fence = state.beginRequest();
      try {
        const services = await client.snapshot();
        if (!state.applySnapshot(fence, services)) return;
        // Best-effort: a daemon from before `/v1/urls` existed simply has no URL rows.
        urls = await client.urls().catch(() => urls);
        state.notice = "";
        render();
        await refreshSelected(state.beginRequest());
      } catch (error) {
        if (!state.current(fence)) return;
        state.notice = `Manager unavailable: ${message(error)}`;
        render();
      }
    }

    async function refreshSelected(fence: TuiFence): Promise<void> {
      const service = state.selection.selectedName;
      if (!service) {
        if (state.current(fence)) {
          state.log = "No services.";
          render();
        }
        return;
      }
      try {
        if (await refreshSelectedLog(client, state, fence)) render();
        await refreshOperation(fence);
      } catch (error) {
        if (state.current(fence)) {
          state.notice = `Log unavailable: ${message(error)}`;
          render();
        }
      }
    }

    async function refreshOperation(fence: TuiFence): Promise<void> {
      const operationId = state.selection.selected()?.currentOperationId;
      if (!operationId) return;
      try {
        const operation = await client.operation(operationId);
        if (state.applyOperation(fence, operation)) render();
      } catch (error) {
        if (state.current(fence)) {
          state.notice = `Operation unavailable: ${message(error)}`;
          render();
        }
      }
    }

    async function runAll(action: "start" | "stop" | "restart"): Promise<void> {
      if (action === "start") {
        if (busy) return;
        busy = true;
        state.notice = "start all…";
        render();
        try {
          const operation = await client.bulkStart(allTargets);
          const completed = await client.waitOperation(operation.id);
          if (completed.status === "failed") throw new Error(completed.error?.message ?? "start all failed");
          state.notice = "start all: succeeded";
          render();
          await reconcile();
        } catch (error) {
          state.notice = `start all failed: ${message(error)}`;
          render();
        } finally {
          busy = false;
        }
        return;
      }
      for (const service of state.selection.services) {
        if (disposed) return;
        await runAction(action, service.name);
      }
    }

    async function runAction(action: "start" | "stop" | "restart", service: string): Promise<void> {
      if (busy || !service) return;
      const fence = state.beginAction(service, action);
      busy = true;
      state.notice = `${action} ${service}…`;
      render();
      try {
        const operation = await client.action(service, action);
        if (!state.applyAction(fence, operation)) return;
        render();
        void reconcile();
      } catch (error) {
        if (state.applyActionFailure(fence, message(error))) render();
      } finally {
        busy = false;
      }
    }

    function shutdown(): void {
      if (disposed) return;
      disposed = true;
      state.beginConnection();
      clearInterval(timer);
      streamAbort.abort();
      terminal.write(mouseTrackingOff);
      tui.stop();
      resolveExit(0);
    }
  });
}

export { ServiceScreen } from "./screen";
export { sanitizeTerminalText } from "./text.utils";
export { boundedTail, serviceFromLifecycle, ServiceSelection, TuiState, type ActionFence, type Service, type ServiceKindLookup, type TuiFence } from "./state";
export { keyboardAction, type TuiAction } from "./tui-actions";
export { appendSseChunk, logTailBytes, ManagerTuiClient, maxSseFrameBytes, parseSse, refreshSelectedLog, type TuiClientRuntime, type TuiWatchCallbacks } from "./tui-client";
