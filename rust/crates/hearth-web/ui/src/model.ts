export const DAEMON = "$daemon";

export type Workspace = {
  id: string;
  path: string;
  trusted: boolean;
  name: string;
  displayPath: string;
  exists: boolean;
};

export type ServiceDef = {
  id: string;
  label?: string;
  disabled?: boolean;
  ports?: { port: number }[];
  profiles?: { run?: { readiness?: { kind?: string } } };
};

export type ServiceState = {
  serviceId: string;
  actualState?: string;
  error?: string;
  readinessDetail?: string;
};

export type ServiceUrl = { serviceId: string; url: string; label?: string; requiresRunning?: boolean };

export type Snapshot = {
  workspace: Workspace;
  trusted: boolean;
  missing?: boolean;
  error?: string;
  catalogError?: string;
  catalog?: { services?: ServiceDef[]; groupTree?: { name: string; members: string[] }[] };
  services?: ServiceState[];
  urls?: ServiceUrl[];
  daemon?: { pid?: number };
  configRevision?: number;
};

export function displayState(actual?: string) {
  switch (actual) {
    case "ready":
      return "ready";
    case "queued-start":
      return "queued";
    case "running":
    case "running-unready":
      return "running";
    case "starting":
    case "preparing":
      return "starting";
    case "stopping":
      return "stopping";
    case "succeeded":
      return "succeeded";
    case "failed":
      return "failed";
    case "orphaned":
      return "orphaned";
    case "externally-owned":
      return "external";
    default:
      return "stopped";
  }
}

export function isUp(state: string) {
  return state === "ready" || state === "running";
}

export function readinessKind(def?: ServiceDef) {
  return def?.profiles?.run?.readiness?.kind || "";
}

export function serviceOrder(catalog: Snapshot["catalog"], services: ServiceState[] = []) {
  const known = new Set(services.map((service) => service.serviceId));
  const order = (catalog?.services || []).map((service) => service.id);
  for (const service of services) {
    if (!order.includes(service.serviceId)) order.push(service.serviceId);
  }
  return order.filter((id) => known.has(id) || (catalog?.services || []).some((service) => service.id === id));
}

export function sectionsOf(catalog: Snapshot["catalog"], order: string[]) {
  const tree = catalog?.groupTree || [];
  if (!tree.length) return [{ name: null as string | null, ids: order }];
  const first = new Map<string, string>();
  for (const group of tree) {
    for (const member of group.members || []) if (!first.has(member)) first.set(member, group.name);
  }
  const built = tree.map((group) => ({ name: group.name as string | null, ids: [] as string[] }));
  const rest: string[] = [];
  for (const id of order) {
    const section = built.find((item) => item.name === first.get(id));
    if (section) section.ids.push(id);
    else rest.push(id);
  }
  const sections = built.filter((section) => section.ids.length);
  if (rest.length) sections.push({ name: tree.length ? null : "Services", ids: rest });
  return sections.length ? sections : [{ name: null, ids: order }];
}

export function counts(catalog: Snapshot["catalog"], services: ServiceState[] = []) {
  const defs = new Map((catalog?.services || []).map((service) => [service.id, service]));
  let ready = 0;
  let failed = 0;
  let total = 0;
  for (const id of serviceOrder(catalog, services)) {
    const actual = services.find((service) => service.serviceId === id)?.actualState;
    const state = displayState(actual);
    if (readinessKind(defs.get(id)) === "exit" && state !== "failed") continue;
    total += 1;
    if (isUp(state)) ready += 1;
    else if (state === "failed") failed += 1;
  }
  return { ready, failed, total };
}

export const dotClass: Record<string, string> = {
  ready: "bg-emerald-500",
  succeeded: "bg-emerald-500",
  running: "bg-sky-500",
  starting: "bg-amber-500",
  stopping: "bg-amber-500",
  queued: "bg-amber-500",
  failed: "bg-red-500",
  orphaned: "bg-red-500",
  external: "bg-violet-500",
  stopped: "bg-muted-foreground",
};
