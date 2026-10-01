import { For, Show, createEffect, createSignal, onCleanup, onMount } from "solid-js";
import { createStore } from "solid-js/store";
import {
  Boxes,
  Check,
  Clipboard,
  Download,
  Flame,
  FolderOpen,
  FolderPlus,
  Loader2,
  OctagonAlert,
  Play,
  Power,
  RotateCw,
  ShieldCheck,
  Square,
  Terminal,
  Trash2,
  X,
} from "lucide-solid";
import { Button } from "~/components/ui/button";
import { api } from "~/api";
import {
  DAEMON,
  type ServiceDef,
  type ServiceState,
  type Snapshot,
  type Workspace,
  counts,
  displayState,
  dotClass,
  isUp,
  readinessKind,
  sectionsOf,
  serviceOrder,
} from "~/model";

type Ask = { title: string; message: string; confirm: string; danger?: boolean; resolve: (value: boolean) => void };

export function App() {
  const [state, setState] = createStore({
    workspaces: [] as Workspace[],
    suggested: null as string | null,
    selectedId: null as string | null,
    mode: "workspace" as "workspace" | "shared",
    snapshot: null as Snapshot | null,
    shared: null as any,
    recipes: [] as { name: string; versions: string[] }[],
    selection: sessionStorage.getItem("hearth.service") || DAEMON,
    busy: {} as Record<string, boolean>,
    groupBusy: {} as Record<string, boolean>,
    stopped: {} as Record<string, boolean>,
    sharedBusy: {} as Record<string, boolean>,
    banner: "",
    log: "",
    booted: false,
    workspacesReady: false,
    snapshotPending: false,
    refreshing: false,
    logPending: false,
    sharedPending: false,
    sharedReady: false,
    toolbar: "" as string,
  });
  const [copied, setCopied] = createSignal("");
  const [askState, setAsk] = createSignal<Ask | null>(null);
  let configSeen: { id: string; revision: number | null } | null = null;
  let logEl: HTMLPreElement | undefined;

  function banner(message: string) {
    setState("banner", message || "");
  }

  function ask(input: Omit<Ask, "resolve">) {
    return new Promise<boolean>((resolve) => setAsk({ ...input, resolve }));
  }

  function finishAsk(value: boolean) {
    const current = askState();
    setAsk(null);
    current?.resolve(value);
  }

  function remember() {
    if (state.selectedId) sessionStorage.setItem("hearth.workspace", state.selectedId);
    sessionStorage.setItem("hearth.mode", state.mode);
    if (state.selection) sessionStorage.setItem("hearth.service", state.selection);
  }

  async function copyText(text: string, key = text) {
    try {
      await navigator.clipboard.writeText(text);
      setCopied(key);
      setTimeout(() => setCopied((current) => (current === key ? "" : current)), 1200);
    } catch {
      banner("Could not copy.");
    }
  }

  async function refreshWorkspaces() {
    const data = await api("GET", "/api/workspaces");
    const workspaces = (data.workspaces || []) as Workspace[];
    setState("workspaces", workspaces);
    setState("suggested", data.suggestedWorkspaceId || null);
    if (data.loadError) banner(data.loadError);
    if (!state.booted) {
      setState("booted", true);
      const ids = new Set(workspaces.map((workspace) => workspace.id));
      const stored = sessionStorage.getItem("hearth.workspace");
      let selected = state.selectedId;
      if (stored && ids.has(stored)) selected = stored;
      else if (data.suggestedWorkspaceId && ids.has(data.suggestedWorkspaceId)) selected = data.suggestedWorkspaceId;
      else selected = workspaces[0]?.id || null;
      setState("selectedId", selected);
      if (sessionStorage.getItem("hearth.mode") === "shared") setState("mode", "shared");
    }
    if (state.selectedId && !workspaces.some((workspace) => workspace.id === state.selectedId)) {
      setState("selectedId", workspaces[0]?.id || null);
    }
    setState("workspacesReady", true);
  }

  async function refreshSnapshot() {
    if (!state.selectedId || state.mode !== "workspace") return;
    if (state.stopped[state.selectedId]) return;
    const first = !state.snapshot || state.snapshot.workspace?.id !== state.selectedId;
    setState(first ? "snapshotPending" : "refreshing", true);
    let data: Snapshot;
    try {
      data = (await api("GET", `/api/workspaces/${encodeURIComponent(state.selectedId)}/snapshot`)) as Snapshot;
    } finally {
      setState("snapshotPending", false);
      setState("refreshing", false);
    }
    const previous = configSeen;
    setState("snapshot", data);
    configSeen = { id: data.workspace.id, revision: data.configRevision || null };
    const edited =
      previous &&
      previous.id === data.workspace.id &&
      previous.revision &&
      data.configRevision &&
      previous.revision !== data.configRevision;
    if (edited && data.trusted && data.daemon) {
      try {
        await api("POST", `/api/workspaces/${encodeURIComponent(data.workspace.id)}/reload`);
        const fresh = (await api("GET", `/api/workspaces/${encodeURIComponent(data.workspace.id)}/snapshot`)) as Snapshot;
        setState("snapshot", fresh);
        configSeen = { id: fresh.workspace.id, revision: fresh.configRevision || data.configRevision || null };
      } catch (error) {
        banner(`Config reload failed: ${(error as Error).message}`);
      }
    }
  }

  async function refreshLog() {
    if (state.mode !== "workspace" || !state.selectedId || !state.snapshot?.trusted) return;
    const path =
      state.selection === DAEMON
        ? `/api/workspaces/${encodeURIComponent(state.selectedId)}/daemon-log`
        : `/api/workspaces/${encodeURIComponent(state.selectedId)}/logs?service=${encodeURIComponent(state.selection)}`;
    setState("logPending", true);
    try {
      const slice = await api("GET", path);
      const next = slice.data || "";
      if (logEl && logEl.textContent !== next) {
        const stick = logEl.scrollHeight - logEl.scrollTop - logEl.clientHeight < 48;
        setState("log", next);
        queueMicrotask(() => {
          if (stick && logEl) logEl.scrollTop = logEl.scrollHeight;
        });
      } else setState("log", next);
    } catch (error) {
      setState("log", (error as Error).message);
    } finally {
      setState("logPending", false);
    }
  }

  async function refreshShared() {
    setState("sharedPending", true);
    try {
      const [status, catalog] = await Promise.all([api("GET", "/api/shared"), api("GET", "/api/shared/catalog")]);
      setState("shared", status);
      setState("recipes", catalog.services || []);
      setState("sharedReady", true);
      if (status.error || catalog.error) banner(status.error || catalog.error);
    } finally {
      setState("sharedPending", false);
    }
  }

  async function tick() {
    try {
      if (state.mode === "shared") await refreshShared();
      else if (state.selectedId && !state.stopped[state.selectedId]) {
        await refreshSnapshot();
        await refreshLog();
      }
    } catch (error) {
      banner((error as Error).message);
    }
  }

  onMount(() => {
    void (async () => {
      try {
        await refreshWorkspaces();
        await tick();
      } catch (error) {
        banner((error as Error).message);
      }
    })();
    const timer = setInterval(() => {
      if (!document.hidden) void tick();
    }, 2000);
    onCleanup(() => clearInterval(timer));
  });

  createEffect(() => {
    state.log;
    if (logEl) logEl.textContent = state.log;
  });

  async function selectWorkspace(id: string) {
    setState({ mode: "workspace", selectedId: id, snapshot: null });
    remember();
    await tick();
  }

  async function removeWorkspace(workspace: Workspace) {
    const ok = await ask({
      title: `Remove “${workspace.name}”?`,
      message: "This forgets the folder. It does not stop its daemon or services.",
      confirm: "Remove",
      danger: true,
    });
    if (!ok) return;
    await api("DELETE", `/api/workspaces/${encodeURIComponent(workspace.id)}`);
    if (state.selectedId === workspace.id) setState("selectedId", null);
    await refreshWorkspaces();
    await tick();
  }

  async function addWorkspace(event: SubmitEvent) {
    event.preventDefault();
    const input = event.currentTarget as HTMLFormElement;
    const path = new FormData(input).get("path")?.toString().trim() || "";
    try {
      const added = await api("POST", "/api/workspaces", { path });
      input.reset();
      setState({ mode: "workspace", selectedId: added.workspace.id });
      remember();
      await refreshWorkspaces();
      await tick();
    } catch (error) {
      banner((error as Error).message);
    }
  }

  function lifecycle(id: string): ServiceState {
    return state.snapshot?.services?.find((service) => service.serviceId === id) || { serviceId: id, actualState: "stopped" };
  }

  function defOf(id: string): ServiceDef | undefined {
    return state.snapshot?.catalog?.services?.find((service) => service.id === id);
  }

  async function waitOperation(id?: string, shared = false) {
    if (!id) return;
    const deadline = Date.now() + 180000;
    while (Date.now() < deadline) {
      const current = shared
        ? await api("GET", `/api/shared/operations/${encodeURIComponent(id)}`)
        : await api("GET", `/api/workspaces/${encodeURIComponent(state.selectedId || "")}/operations/${encodeURIComponent(id)}`);
      const status = current.operation?.status;
      if (status === "succeeded") return;
      if (status === "failed") throw new Error(current.operation?.error?.message || "The operation failed.");
      await new Promise((resolve) => setTimeout(resolve, 300));
    }
    throw new Error("The operation did not finish.");
  }

  async function operate(id: string, action: string, killUnowned = false) {
    setState("busy", id, true);
    try {
      const accepted = await api("POST", `/api/workspaces/${encodeURIComponent(state.selectedId || "")}/operations`, {
        serviceId: id,
        action,
        killUnowned,
      });
      await waitOperation(accepted.operation?.id);
    } catch (error) {
      banner((error as Error).message);
    } finally {
      setState("busy", id, false);
      await refreshSnapshot();
    }
  }

  async function reclaim(id: string, error?: string) {
    const ok = await ask({
      title: "Kill the process holding this port?",
      message: error || "A process this manager does not own holds the port. It will be terminated, then the service will start.",
      confirm: "Kill & Start",
      danger: true,
    });
    if (ok) await operate(id, "start", true);
  }

  async function runGroup(name: string, ids: string[], action: "start" | "stop" | "restart") {
    setState("groupBusy", name, true);
    try {
      if (action === "start") await startGroup(ids);
      else await Promise.all(ids.map((id) => operate(id, action)));
    } finally {
      setState("groupBusy", name, false);
    }
  }

  async function startGroup(ids: string[]) {
    if (!ids.length) return;
    try {
      const accepted = await api("POST", `/api/workspaces/${encodeURIComponent(state.selectedId || "")}/bulk-start`, { targets: ids });
      await waitOperation(accepted.operation?.id);
    } catch (error) {
      banner((error as Error).message);
    }
    await refreshSnapshot();
  }

  async function stopAll() {
    setState("toolbar", "stop-all");
    try {
      await stopAllNow();
    } finally {
      setState("toolbar", "");
    }
  }

  async function stopAllNow() {
    const data = state.snapshot;
    if (!data) return;
    const defs = new Map((data.catalog?.services || []).map((service) => [service.id, service]));
    const ids = serviceOrder(data.catalog, data.services).filter((id) => {
      if (defs.get(id)?.disabled) return false;
      const shown = displayState(lifecycle(id).actualState);
      return shown !== "stopped" && shown !== "succeeded";
    });
    await Promise.all(ids.map((id) => operate(id, "stop")));
  }

  async function trust(id: string) {
    try {
      await api("POST", `/api/workspaces/${encodeURIComponent(id)}/trust`);
      await refreshWorkspaces();
      await refreshSnapshot();
    } catch (error) {
      banner((error as Error).message);
    }
  }

  async function reloadCatalog(id: string) {
    setState("toolbar", "reload");
    try {
      await api("POST", `/api/workspaces/${encodeURIComponent(id)}/reload`);
      await refreshSnapshot();
    } catch (error) {
      banner((error as Error).message);
    } finally {
      setState("toolbar", "");
    }
  }

  async function reveal(id: string) {
    setState("toolbar", "reveal");
    try {
      await api("POST", `/api/workspaces/${encodeURIComponent(id)}/reveal`);
    } catch (error) {
      banner((error as Error).message);
    } finally {
      setState("toolbar", "");
    }
  }

  async function restartDaemon(id: string) {
    const ok = await ask({ title: "Restart this project's daemon?", message: "Services keep running. The next daemon adopts them.", confirm: "Restart daemon" });
    if (!ok) return;
    setState("toolbar", "restart-daemon");
    try {
      await api("POST", `/api/workspaces/${encodeURIComponent(id)}/daemon/restart`);
      await refreshSnapshot();
    } catch (error) {
      banner((error as Error).message);
    } finally {
      setState("toolbar", "");
    }
  }

  async function startDaemon(id: string) {
    setState("stopped", id, false);
    await refreshSnapshot();
    await refreshLog();
  }

  async function stopDaemon(id: string) {
    const ok = await ask({
      title: "Stop this project's daemon?",
      message: "The daemon and every service it manages will be stopped.",
      confirm: "Stop daemon",
      danger: true,
    });
    if (!ok) return;
    setState("toolbar", "stop-daemon");
    try {
      await api("POST", `/api/workspaces/${encodeURIComponent(id)}/daemon/stop`);
      setState("stopped", id, true);
      setState("snapshot", null);
    } catch (error) {
      banner((error as Error).message);
    } finally {
      setState("toolbar", "");
    }
  }

  function selectService(id: string) {
    if (state.selection !== id) {
      setState("log", "");
      setState("logPending", true);
    }
    setState("selection", id);
    remember();
    void refreshLog();
  }

  const summary = (workspace: Workspace) => {
    if (!workspace.exists) return "Missing";
    if (!workspace.trusted) return "Not trusted";
    if (state.mode === "workspace" && state.snapshot?.workspace?.id === workspace.id) {
      const count = counts(state.snapshot.catalog, state.snapshot.services);
      if (!count.total) return "";
      return `${count.ready}/${count.total}${count.failed ? ` · ${count.failed} failed` : ""}`;
    }
    return "";
  };

  return (
    <div class="grid h-dvh grid-cols-1 overflow-hidden bg-background text-foreground md:grid-cols-[260px_minmax(0,1fr)]">
      <aside class="flex min-h-0 flex-col border-b bg-muted/40 md:border-b-0 md:border-r">
        <div class="flex items-baseline justify-between px-4 pb-2 pt-4">
          <strong class="inline-flex items-center gap-2"><Flame class="size-4" /> Hearth</strong>
          <span class="text-xs text-muted-foreground">Workspaces</span>
        </div>
        <nav class="min-h-0 flex-1 space-y-1 overflow-auto px-2" aria-label="Workspaces" aria-busy={!state.workspacesReady}>
          <Show when={!state.workspacesReady}><SidebarSkeleton /></Show>
          <Show when={state.workspacesReady && state.workspaces.length === 0}><p class="px-2 text-sm text-muted-foreground">No workspaces yet.</p></Show>
          <For each={state.workspaces}>
            {(workspace) => {
              const selected = () => state.mode === "workspace" && state.selectedId === workspace.id;
              const extra = () => summary(workspace);
              return (
                <div class="grid grid-cols-[minmax(0,1fr)_auto] items-center gap-1">
                  <button
                    type="button"
                    class={`min-w-0 rounded-md px-2 py-2 text-left ${selected() ? "bg-accent" : "hover:bg-accent/60"}`}
                    aria-current={selected() ? "true" : undefined}
                    onClick={() => void selectWorkspace(workspace.id)}
                  >
                    <strong class="block truncate text-sm">{workspace.name || workspace.path}</strong>
                    <span class="block truncate text-xs text-muted-foreground">
                      {extra() ? `${workspace.displayPath} · ${extra()}` : workspace.displayPath}
                    </span>
                  </button>
                  <Button variant="ghost" size="icon" aria-label={`Remove ${workspace.name || "workspace"}`} onClick={() => void removeWorkspace(workspace)}>
                    <Trash2 />
                  </Button>
                </div>
              );
            }}
          </For>
        </nav>
        <form class="space-y-2 p-3" onSubmit={addWorkspace}>
          <label class="text-xs font-medium" for="folder">Add folder</label>
          <div class="flex gap-2">
            <input id="folder" name="path" required placeholder="/absolute/path" autocomplete="off" class="h-8 min-w-0 flex-1 rounded-md border bg-background px-2 text-sm" />
            <Button type="submit" size="sm"><FolderPlus /> Add</Button>
          </div>
        </form>
        <div class="space-y-1 p-3 pt-0">
          <Button variant={state.mode === "shared" ? "secondary" : "outline"} size="sm" class="w-full justify-start" onClick={() => { setState("mode", "shared"); remember(); void tick(); }}>
            <Boxes /> Shared services
          </Button>
          <a class="inline-flex items-center gap-2 px-2 text-xs text-muted-foreground" href="https://github.com/ngosangns/hearth/releases" target="_blank" rel="noreferrer">Check for updates</a>
        </div>
      </aside>
      <main class="flex min-h-0 min-w-0 flex-col">
        <Show when={state.banner}>
          <div class="flex items-center justify-between gap-3 bg-destructive px-4 py-2 text-sm text-destructive-foreground" role="status">
            <span>{state.banner}</span>
            <Button variant="secondary" size="sm" onClick={() => banner("")}>Dismiss</Button>
          </div>
        </Show>
        <Show when={state.mode === "shared"} fallback={<Workspace state={state} setState={setState} logEl={(el) => (logEl = el)} lifecycle={lifecycle} defOf={defOf} selectService={selectService} operate={operate} reclaim={reclaim} runGroup={runGroup} stopAll={stopAll} trust={trust} reloadCatalog={reloadCatalog} reveal={reveal} restartDaemon={restartDaemon} stopDaemon={stopDaemon} startDaemon={startDaemon} copyText={copyText} copied={copied()} />}>
          <Shared state={state} setState={setState} copyText={copyText} banner={banner} refresh={refreshShared} ask={ask} />
        </Show>
      </main>
      <Show when={askState()}>
        {(ask) => (
          <div class="fixed inset-0 z-50 grid place-items-center bg-black/45 p-4" role="alertdialog" aria-modal="true">
            <div class="grid w-[min(440px,100%)] gap-3 rounded-lg border bg-background p-5 shadow-lg">
              <h2 class="text-base font-semibold">{ask().title}</h2>
              <p class="text-sm text-muted-foreground">{ask().message}</p>
              <div class="flex justify-end gap-2">
                <Button variant="outline" size="sm" onClick={() => finishAsk(false)}>Cancel</Button>
                <Button variant={ask().danger ? "destructive" : "default"} size="sm" onClick={() => finishAsk(true)}>{ask().confirm}</Button>
              </div>
            </div>
          </div>
        )}
      </Show>
    </div>
  );
}

function Workspace(props: any) {
  const data = () => props.state.snapshot as Snapshot | null;
  const stopped = () => !!props.state.stopped[props.state.selectedId];
  return (
    <Show when={props.state.selectedId} fallback={<Center title="No workspaces" body="Add a folder that has a hearth.yaml to manage its services here." />}>
      <Show when={!stopped()} fallback={<Center title="Daemon stopped" body="Services are down. Start the daemon to bring the workspace back." action={<Button size="sm" onClick={() => props.startDaemon(props.state.selectedId)}><Play /> Start daemon</Button>} />}>
        <Show when={data()} fallback={<DeskSkeleton />}>
          {(snap) => (
            <Show when={!snap().missing} fallback={<Center title="Folder missing" body={snap().workspace.path} />}>
              <Show when={!(snap().error && !snap().daemon)} fallback={<Center title="Daemon failed to start" body={snap().error} action={<Button size="sm" onClick={() => props.startDaemon(snap().workspace.id)}><RotateCw /> Retry</Button>} />}>
                <Show when={snap().trusted} fallback={
                  <Center title="Trust this folder?" body={`${snap().workspace.path}\n\nThis folder's hearth.yaml names commands the daemon will run on your behalf. Only trust folders you wrote or downloaded from somewhere you trust.\n\n${snap().catalogError || (snap().catalog?.services || []).map((service: ServiceDef) => service.label || service.id).join(", ") || "No services in this catalog yet."}`} action={<Button size="sm" onClick={() => props.trust(snap().workspace.id)}><ShieldCheck /> Trust and start</Button>} />
                }>
                  <ServiceDesk {...props} snap={snap()} />
                </Show>
              </Show>
            </Show>
          )}
        </Show>
      </Show>
    </Show>
  );
}

function Center(props: { title: string; body?: string; action?: any }) {
  return (
    <div class="grid h-full place-items-center p-8 text-center">
      <div class="max-w-md space-y-3">
        <h1 class="text-lg font-semibold">{props.title}</h1>
        <Show when={props.body}><p class="whitespace-pre-wrap text-sm text-muted-foreground">{props.body}</p></Show>
        {props.action}
      </div>
    </div>
  );
}

function ServiceDesk(props: any) {
  const snap = () => props.snap as Snapshot;
  const order = () => serviceOrder(snap().catalog, snap().services);
  const sections = () => sectionsOf(snap().catalog, order());
  const count = () => counts(snap().catalog, snap().services);
  const header = () => {
    const parts = [snap().workspace.displayPath];
    const total = count();
    if (total.total) parts.push(`${total.ready}/${total.total} ready`);
    if (total.failed) parts.push(`${total.failed} failed`);
    if (snap().daemon?.pid) parts.push(`daemon ${snap().daemon.pid}`);
    return parts.join(" · ");
  };
  const title = () => props.state.selection === DAEMON ? "daemon log" : (props.defOf(props.state.selection)?.label || props.state.selection || "Log");
  return (
    <div class="grid h-full min-h-0 grid-cols-1 grid-rows-[auto_minmax(180px,46%)_minmax(0,1fr)] xl:grid-cols-[minmax(280px,380px)_minmax(0,1fr)] xl:grid-rows-[auto_minmax(0,1fr)]">
      <header class="flex flex-wrap items-center justify-between gap-3 border-b px-4 py-3 xl:col-span-2">
        <div class="min-w-0">
          <h1 class="truncate text-base font-semibold">{snap().workspace.name}</h1>
          <p class="truncate text-xs text-muted-foreground">
            {header()}
            <Show when={props.state.refreshing}>
              <span class="ml-2 inline-flex items-center gap-1 text-muted-foreground"><Loader2 class="size-3 animate-spin" /> Updating</span>
            </Show>
          </p>
        </div>
        <div class="flex flex-wrap gap-2">
          <Button variant="outline" size="sm" disabled={props.state.toolbar === "stop-all"} onClick={() => props.stopAll()}>{props.state.toolbar === "stop-all" ? <Loader2 class="animate-spin" /> : <Square />} Stop all</Button>
          <Button variant="outline" size="sm" disabled={props.state.toolbar === "reload"} onClick={() => props.reloadCatalog(snap().workspace.id)}>{props.state.toolbar === "reload" ? <Loader2 class="animate-spin" /> : <RotateCw />} Reload</Button>
          <Button variant="outline" size="sm" disabled={props.state.toolbar === "reveal"} onClick={() => props.reveal(snap().workspace.id)}>{props.state.toolbar === "reveal" ? <Loader2 class="animate-spin" /> : <FolderOpen />} Reveal</Button>
          <Button variant="outline" size="sm" disabled={props.state.toolbar === "restart-daemon"} onClick={() => props.restartDaemon(snap().workspace.id)}>{props.state.toolbar === "restart-daemon" ? <Loader2 class="animate-spin" /> : <RotateCw />} Restart daemon</Button>
          <Button variant="destructive" size="sm" disabled={props.state.toolbar === "stop-daemon"} onClick={() => props.stopDaemon(snap().workspace.id)}>{props.state.toolbar === "stop-daemon" ? <Loader2 class="animate-spin" /> : <Power />} Stop daemon</Button>
        </div>
      </header>
      <section class="min-h-0 overflow-auto border-b xl:border-b-0 xl:border-r" aria-label="Services">
        <Show when={snap().error || snap().catalogError}><p class="px-4 py-2 text-sm text-muted-foreground">{snap().error || snap().catalogError}</p></Show>
        <Show when={order().length === 0}><p class="p-4 text-sm text-muted-foreground">No services yet. Add service blocks to hearth.yaml.</p></Show>
        <For each={sections()}>
          {(section) => (
            <>
              <GroupHead section={section} snap={snap()} defOf={props.defOf} lifecycle={props.lifecycle} pending={!!props.state.groupBusy[section.name || ""]} runGroup={props.runGroup} />
              <For each={section.ids}>
                {(id) => <ServiceRow id={id} def={props.defOf(id)} service={props.lifecycle(id)} urls={snap().urls || []} selected={props.state.selection === id} busy={!!props.state.busy[id]} copied={props.copied} onSelect={() => props.selectService(id)} operate={props.operate} reclaim={props.reclaim} copyText={props.copyText} />}
              </For>
            </>
          )}
        </For>
      </section>
      <section class="flex min-h-0 flex-col">
        <div class="flex items-center gap-2 border-b px-3 py-2">
          <strong class="min-w-0 flex-1 truncate text-sm">{title()}</strong>
          <Button variant={props.state.selection === DAEMON ? "default" : "outline"} size="sm" onClick={() => props.selectService(DAEMON)}><Terminal /> Daemon log</Button>
          <IconButton label={props.copied === "log" ? "Copied" : "Copy log"} onClick={() => props.copyText(props.state.log, "log")}>
            {props.copied === "log" ? <Check /> : <Clipboard />}
          </IconButton>
        </div>
        <Show when={props.state.logPending && !props.state.log} fallback={<pre ref={props.logEl} tabindex="0" class="min-h-0 flex-1 overflow-auto whitespace-pre-wrap break-words p-3 font-mono text-xs" />}>
          <LogSkeleton />
        </Show>
      </section>
    </div>
  );
}

function IconButton(props: { label: string; pending?: boolean; variant?: "default" | "outline" | "destructive" | "ghost"; onClick?: () => void; children?: any }) {
  return (
    <Button
      variant={props.pending ? "outline" : props.variant || "outline"}
      size="icon"
      aria-label={props.label}
      title={props.label}
      aria-busy={props.pending || undefined}
      disabled={props.pending}
      onClick={props.onClick}
    >
      {props.pending ? <Loader2 class="animate-spin" /> : props.children}
    </Button>
  );
}

function SidebarSkeleton() {
  return (
    <div class="space-y-2 px-2 py-1" aria-hidden="true">
      <For each={[1, 2, 3]}>{() => <div class="h-10 animate-pulse rounded-md bg-muted" />}</For>
    </div>
  );
}

function DeskSkeleton() {
  return (
    <div class="space-y-3 p-4" aria-busy="true" aria-label="Loading services">
      <div class="h-8 w-48 animate-pulse rounded-md bg-muted" />
      <For each={[1, 2, 3, 4, 5]}>{() => <div class="h-12 animate-pulse rounded-md bg-muted" />}</For>
    </div>
  );
}

function LogSkeleton() {
  return (
    <div class="space-y-2 p-3" aria-busy="true" aria-label="Loading log">
      <For each={[1, 2, 3, 4, 5, 6]}>{() => <div class="h-3 animate-pulse rounded bg-muted" />}</For>
    </div>
  );
}

function GroupHead(props: any) {
  const name = () => props.section.name || ((props.snap.catalog?.groupTree || []).length ? "Other services" : "Services");
  const enabled = () => props.section.ids.filter((id: string) => !props.defOf(id)?.disabled);
  const allStarted = () => {
    const longLived = enabled().filter((id: string) => readinessKind(props.defOf(id)) !== "exit");
    return longLived.length > 0 && longLived.every((id: string) => isUp(displayState(props.lifecycle(id).actualState)));
  };
  return (
    <div class="sticky top-0 z-10 flex items-center justify-between gap-2 border-b bg-muted/70 px-3 py-2 text-sm font-semibold">
      <span>{name()}</span>
      <Show when={props.section.name && enabled().length}>
        <div class="flex gap-1">
          <Show when={allStarted()} fallback={<IconButton label={`Start ${name()}`} variant="default" pending={props.pending} onClick={() => props.runGroup(props.section.name, enabled(), "start")}><Play /></IconButton>}>
            <IconButton label={`Restart ${name()}`} variant="outline" pending={props.pending} onClick={() => props.runGroup(props.section.name, enabled(), "restart")}><RotateCw /></IconButton>
          </Show>
          <IconButton label={`Stop ${name()}`} variant="outline" pending={props.pending} onClick={() => props.runGroup(props.section.name, enabled(), "stop")}><Square /></IconButton>
        </div>
      </Show>
    </div>
  );
}

function ServiceRow(props: any) {
  const shown = () => displayState(props.service.actualState);
  const finite = () => readinessKind(props.def) === "exit";
  const label = () => props.def?.label || props.id;
  const ports = () => (props.def?.ports || []).map((port: { port: number }) => port.port).join(", ");
  const stateLabel = () => (finite() && shown() === "stopped" ? "not run" : shown());
  const meta = () => [stateLabel(), ports(), props.def?.disabled ? "disabled" : "", finite() ? "job" : ""].filter(Boolean).join(" · ");
  const detail = () => props.service.error || (shown() === "failed" ? props.service.readinessDetail : "");
  const links = () => (props.urls as any[]).filter((item) => item.serviceId === props.id && (!item.requiresRunning || isUp(shown())));
  const runLabel = () => (shown() === "succeeded" || finite() ? "Run" : "Start");
  return (
    <div class={`grid grid-cols-[minmax(0,1fr)_auto] items-center gap-2 border-b px-3 py-2 ${props.selected ? "bg-accent/70" : ""}`}>
      <button type="button" class="grid min-w-0 grid-cols-[10px_minmax(0,1fr)] items-center gap-x-2 text-left" onClick={props.onSelect}>
        <span class={`row-span-2 size-2 rounded-full ${dotClass[shown()] || dotClass.stopped} ${shown() === "starting" || shown() === "stopping" || shown() === "queued" ? "animate-pulse" : ""}`} />
        <span class="truncate text-sm font-medium">{label()}</span>
        <span class="truncate text-xs text-muted-foreground">{meta()}</span>
      </button>
      <div class="flex flex-wrap justify-end gap-1">
        <RowActions shown={shown()} busy={props.busy} disabled={!!props.def?.disabled} runLabel={runLabel()} onStart={() => props.operate(props.id, "start")} onStop={() => props.operate(props.id, "stop")} onRestart={() => props.operate(props.id, "restart")} onKill={() => props.reclaim(props.id, props.service.error)} />
      </div>
      <Show when={detail()}><p class="col-span-2 text-xs text-muted-foreground">{detail()}</p></Show>
      <Show when={links().length}>
        <div class="col-span-2 flex flex-wrap items-center gap-2">
          <For each={links()}>
            {(url) => (
              <>
                <a class="text-sm underline" href={url.url} target="_blank" rel="noreferrer">{url.label || url.url}</a>
                <IconButton label={props.copied === url.url ? "Copied" : "Copy link"} onClick={() => props.copyText(url.url, url.url)}>
                  {props.copied === url.url ? <Check /> : <Clipboard />}
                </IconButton>
              </>
            )}
          </For>
        </div>
      </Show>
    </div>
  );
}

function RowActions(props: { shown: string; busy: boolean; disabled: boolean; runLabel: string; onStart: () => void; onStop: () => void; onRestart: () => void; onKill: () => void }) {
  if (props.disabled) return null;
  if (props.busy && props.shown !== "stopping") {
    return (
      <>
        <IconButton label="Working" pending />
        <IconButton label="Stop" onClick={props.onStop}><Square /></IconButton>
      </>
    );
  }
  if (props.shown === "external") {
    return (
      <>
        <IconButton label="Kill and start" variant="destructive" onClick={props.onKill}><OctagonAlert /></IconButton>
        <IconButton label="Stop" onClick={props.onStop}><Square /></IconButton>
      </>
    );
  }
  if (props.shown === "orphaned") {
    return (
      <>
        <IconButton label="Start" variant="default" onClick={props.onStart}><Play /></IconButton>
        <IconButton label="Stop" onClick={props.onStop}><Square /></IconButton>
      </>
    );
  }
  if (props.shown === "queued") return <IconButton label="Cancel start" onClick={props.onStop}><X /></IconButton>;
  if (props.shown === "ready" || props.shown === "running" || props.shown === "starting") {
    return (
      <>
        <IconButton label="Restart" onClick={props.onRestart}><RotateCw /></IconButton>
        <IconButton label="Stop" onClick={props.onStop}><Square /></IconButton>
      </>
    );
  }
  if (props.shown === "succeeded" || props.shown === "stopped" || props.shown === "failed") {
    return <IconButton label={props.runLabel} variant="default" onClick={props.onStart}><Play /></IconButton>;
  }
  return null;
}

function RecipeRow(props: {
  recipe: { name: string; versions?: string[] };
  instances: { id: string }[];
  busy: Record<string, boolean>;
  onInstall: (service: string) => void;
}) {
  const versions = () => props.recipe.versions || [];
  const [version, setVersion] = createSignal(versions()[0] || "");
  const chosen = () => `${props.recipe.name}@${version()}`;
  const installed = () => props.instances.some((instance) => instance.id === chosen());
  return (
    <form
      class="flex items-center justify-between gap-3 border-b py-2"
      onSubmit={(event) => {
        event.preventDefault();
        if (!installed()) void props.onInstall(chosen());
      }}
    >
      <strong class="text-sm">{props.recipe.name}</strong>
      <div class="flex items-center gap-2">
        <select
          class="h-8 rounded-md border bg-background px-2 text-sm"
          aria-label={`${props.recipe.name} version`}
          value={version()}
          onChange={(event) => setVersion(event.currentTarget.value)}
        >
          <For each={versions()}>{(item) => <option value={item}>{item}</option>}</For>
        </select>
        <Button type="submit" size="sm" disabled={installed() || !!props.busy[chosen()]} aria-busy={!!props.busy[chosen()]}>
          {props.busy[chosen()] ? <Loader2 class="animate-spin" /> : <Download />} {installed() ? "Installed" : "Install"}
        </Button>
      </div>
    </form>
  );
}

function Shared(props: any) {
  let logEl: HTMLPreElement | undefined;
  async function install(service: string) {
    props.setState("sharedBusy", service, true);
    try {
      await api("POST", "/api/shared/install", { service });
      await props.refresh();
    } catch (error) {
      props.banner((error as Error).message);
    } finally {
      props.setState("sharedBusy", service, false);
    }
  }
  async function act(id: string, action: string, killUnowned = false) {
    props.setState("sharedBusy", id, true);
    try {
      const accepted = await api("POST", "/api/shared/operations", { serviceId: id, action, killUnowned });
      const op = accepted.operation?.id;
      const deadline = Date.now() + 180000;
      while (op && Date.now() < deadline) {
        const current = await api("GET", `/api/shared/operations/${encodeURIComponent(op)}`);
        const status = current.operation?.status;
        if (status === "succeeded") break;
        if (status === "failed") throw new Error(current.operation?.error?.message || "The operation failed.");
        await new Promise((resolve) => setTimeout(resolve, 300));
      }
    } catch (error) {
      props.banner((error as Error).message);
    } finally {
      props.setState("sharedBusy", id, false);
      await props.refresh();
    }
  }
  async function remove(instance: any) {
    const attached = (instance.attachments || []).length;
    const ok = await props.ask({
      title: attached ? `Remove ${instance.id} and its project data?` : `Remove ${instance.id}?`,
      message: attached
        ? `The service is stopped and its install and data under ~/.hearth/shared are deleted. ${attached} attached project${attached === 1 ? "" : "s"} will lose their connection.`
        : "The service is stopped and its install and data under ~/.hearth/shared are deleted.",
      confirm: "Remove",
      danger: true,
    });
    if (!ok) return;
    try {
      await api("POST", "/api/shared/remove", { service: instance.id, force: attached > 0 });
      await props.refresh();
    } catch (error) {
      props.banner((error as Error).message);
    }
  }
  async function showLog(id: string) {
    props.setState("selection", id);
    try {
      const slice = await api("GET", `/api/shared/logs?service=${encodeURIComponent(id)}`);
      if (logEl) logEl.textContent = slice.data || "";
    } catch (error) {
      if (logEl) logEl.textContent = (error as Error).message;
    }
  }
  const instances = () => props.state.shared?.instances || [];
  return (
    <div class="grid h-full min-h-0 grid-rows-[auto_minmax(0,1fr)_minmax(140px,32%)]">
      <header class="border-b px-4 py-3"><h1 class="text-base font-semibold">Shared services</h1></header>
      <div class="min-h-0 overflow-auto px-4 py-2">
        <Show when={props.state.sharedPending && !props.state.sharedReady}><DeskSkeleton /></Show>
        <h2 class="mb-2 text-sm font-semibold">Recipes</h2>
        <For each={props.state.recipes}>
          {(recipe) => (
            <RecipeRow
              recipe={recipe}
              instances={instances()}
              busy={props.state.sharedBusy}
              onInstall={install}
            />
          )}
        </For>
        <h2 class="mb-2 mt-4 text-sm font-semibold">Installed</h2>
        <Show when={props.state.sharedReady && instances().length === 0}><p class="text-sm text-muted-foreground">Nothing installed.</p></Show>
        <For each={instances()}>
          {(instance) => {
            const shown = () => displayState(instance.state?.actualState);
            const attached = () => (instance.attachments || []).length;
            return (
              <div class="border-b py-2">
                <div class="flex items-center justify-between gap-3">
                  <button type="button" class="min-w-0 text-left" onClick={() => void showLog(instance.id)}>
                    <span class="block truncate text-sm font-medium">{instance.id}</span>
                    <span class="block truncate text-xs text-muted-foreground">{[instance.installState || "unknown", shown(), `port ${instance.port}`, attached() ? `${attached()} project${attached() === 1 ? "" : "s"}` : ""].filter(Boolean).join(" · ")}</span>
                  </button>
                  <div class="flex gap-1">
                    <Show when={props.state.sharedBusy[instance.id]}>
                      <IconButton label="Working" pending />
                    </Show>
                    <Show when={!props.state.sharedBusy[instance.id] && (shown() === "ready" || shown() === "running")}>
                      <IconButton label="Restart" onClick={() => void act(instance.id, "restart")}><RotateCw /></IconButton>
                      <IconButton label="Stop" onClick={() => void act(instance.id, "stop")}><Square /></IconButton>
                    </Show>
                    <Show when={!props.state.sharedBusy[instance.id] && shown() !== "ready" && shown() !== "running"}>
                      <IconButton label="Start" variant="default" onClick={() => void act(instance.id, "start")}><Play /></IconButton>
                    </Show>
                    <IconButton label="Remove" variant="destructive" pending={props.state.sharedBusy[instance.id]} onClick={() => void remove(instance)}><Trash2 /></IconButton>
                  </div>
                </div>
              </div>
            );
          }}
        </For>
      </div>
      <section class="flex min-h-0 flex-col border-t">
        <div class="flex items-center gap-2 px-3 py-2"><strong class="text-sm">Log</strong></div>
        <pre ref={logEl} class="min-h-0 flex-1 overflow-auto whitespace-pre-wrap p-3 font-mono text-xs" />
      </section>
    </div>
  );
}
