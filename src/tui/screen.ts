import { truncateToWidth, type Component } from "@oh-my-pi/pi-tui";

import type { Service } from "./state";
import { sanitizeTerminalText } from "./text.utils";

type Viewport = Pick<{ columns: number; rows: number }, "columns" | "rows">;

type ScreenState = {
  services: Service[];
  selectedName: string;
  logService: string;
  log: string;
  notice: string;
  /** The focused service's URLs, one per line (`label  url`). A single string rather than an array
   * so `update`'s `===` change detection keeps working. */
  urls: string;
};

/** At most this many URL rows are shown, so a service with many URLs cannot push its own log off
 * the screen. */
const maxUrlRows = 4;

const headerHeight = 3;

export class ServiceScreen implements Component {
  #state: ScreenState = { services: [], selectedName: "", logService: "", log: "Loading…", notice: "", urls: "" };
  #serviceOffset = 0;
  #revision = 0;
  #cacheKey = "";
  #cache: readonly string[] = [];

  constructor(private readonly terminal: Viewport) {}

  update(state: Partial<ScreenState>): void {
    const selectionChanged = state.selectedName !== undefined && state.selectedName !== this.#state.selectedName;
    if (Object.entries(state).every(([key, value]) => (key === "services" ? sameServices(this.#state.services, value as Service[]) : this.#state[key as Exclude<keyof ScreenState, "services">] === value))) return;
    Object.assign(this.#state, state);
    if (selectionChanged) this.#revealSelected(this.#serviceHeight(this.terminal.rows));
    this.#revision++;
  }

  /** Applies wheel movement inside service rows and returns the nearest visible service. */
  handleWheel(row: number, delta: -1 | 1, viewportRows = this.terminal.rows): string | undefined {
    const serviceHeight = this.#serviceHeight(viewportRows);
    const firstServiceRow = headerHeight + 1;
    if (row < firstServiceRow || row >= firstServiceRow + serviceHeight) return undefined;
    const offset = Math.min(this.#maxServiceOffset(serviceHeight), Math.max(0, this.#serviceOffset + delta));
    if (offset !== this.#serviceOffset) {
      this.#serviceOffset = offset;
      this.#revision++;
    }
    const selectedIndex = this.#state.services.findIndex((service) => service.name === this.#state.selectedName);
    const nearestIndex = selectedIndex < this.#serviceOffset ? this.#serviceOffset : selectedIndex >= this.#serviceOffset + serviceHeight ? this.#serviceOffset + serviceHeight - 1 : selectedIndex;
    return this.#state.services[nearestIndex]?.name;
  }

  /** Returns the service rendered at a left-click row, if any. */
  serviceAt(row: number, viewportRows = this.terminal.rows): string | undefined {
    const serviceRow = row - headerHeight - 1;
    if (serviceRow < 0 || serviceRow >= this.#serviceHeight(viewportRows)) return undefined;
    return this.#state.services[this.#clampServiceOffset(this.#serviceHeight(viewportRows)) + serviceRow]?.name;
  }

  render(width: number, viewportRows = this.terminal.rows): readonly string[] {
    const rows = Math.max(0, viewportRows);
    const cacheKey = `${width}:${rows}:${this.#revision}`;
    if (cacheKey === this.#cacheKey) return this.#cache;

    const bodyHeight = Math.max(0, rows - headerHeight);
    const serviceHeight = this.#serviceHeight(rows);
    const urlRows = this.#state.urls ? this.#state.urls.split("\n").slice(0, maxUrlRows) : [];
    const logHeight = Math.max(0, bodyHeight - serviceHeight - urlRows.length - (serviceHeight > 0 ? 2 : 1));
    const start = this.#clampServiceOffset(serviceHeight);
    const services = this.#state.services.slice(start, start + serviceHeight);
    const hasServiceScrollbar = this.#state.services.length > serviceHeight;
    const serviceWidth = Math.max(0, width - (hasServiceScrollbar ? 1 : 0));
    const logs = splitLines(this.#state.log, logHeight);
    const lines = [
      fit("Local services", width),
      fit("↑/k ↓/j select • Enter toggle • x stop focused • r/R rebuild and restart focused • a start all • s stop all • q quit", width),
      fit(this.#state.notice, width),
    ].slice(0, rows);

    if (serviceHeight > 0) {
      lines.push(fit("SERVICES", width));
      for (let index = 0; index < serviceHeight; index++) {
        const service = services[index];
        const kind = service?.kind === "infrastructure" ? " infra" : "";
        const content = service ? `${service.name === this.#state.selectedName ? ">" : " "} ${statusLabel(service.state)} ${service.name}${kind}` : "";
        lines.push(`${fit(content, serviceWidth)}${hasServiceScrollbar ? scrollbarCell(index, start, serviceHeight, this.#state.services.length) : ""}`);
      }
    }
    for (const url of urlRows) if (lines.length < rows) lines.push(fit(`URL ${url}`, width));
    if (lines.length < rows) {
      lines.push(fit(`LOG — ${this.#state.logService}`, width));
      lines.push(...logs.map((log) => fit(log, width)));
    }
    while (lines.length < rows) lines.push(" ".repeat(width));

    this.#cacheKey = cacheKey;
    this.#cache = lines.slice(0, rows);
    return this.#cache;
  }

  #serviceHeight(viewportRows: number): number {
    const bodyHeight = Math.max(0, viewportRows - headerHeight);
    return bodyHeight < 3 ? 0 : Math.min(this.#state.services.length, Math.max(1, Math.floor((bodyHeight - 2) / 3)));
  }

  #maxServiceOffset(serviceHeight: number): number {
    return Math.max(0, this.#state.services.length - serviceHeight);
  }

  #clampServiceOffset(serviceHeight: number): number {
    return (this.#serviceOffset = Math.min(this.#maxServiceOffset(serviceHeight), this.#serviceOffset));
  }

  /** Keep keyboard selection visible without discarding a manual viewport otherwise. */
  #revealSelected(serviceHeight: number): void {
    if (serviceHeight <= 0) return;
    const selectedIndex = this.#state.services.findIndex((service) => service.name === this.#state.selectedName);
    if (selectedIndex < 0) return;
    if (selectedIndex < this.#serviceOffset) this.#serviceOffset = selectedIndex;
    else if (selectedIndex >= this.#serviceOffset + serviceHeight) this.#serviceOffset = selectedIndex - serviceHeight + 1;
    this.#clampServiceOffset(serviceHeight);
  }

  invalidate(): void {
    this.#cacheKey = "";
  }
}

function fit(text: string, width: number): string {
  return truncateToWidth(sanitizeTerminalText(text), width, "", true);
}

const statusColour: Readonly<Record<string, number>> = { ready: 32, running: 36, preparing: 33, "queued-start": 33, starting: 33, stopping: 33, degraded: 33, stopped: 90, failed: 31, orphaned: 31, "externally-owned": 31 };
const statusLabel = (state: string): string => `\x1b[${statusColour[state] ?? 37}m${state.padEnd(9)}\x1b[0m`;

function scrollbarCell(index: number, start: number, height: number, total: number): string {
  const thumbHeight = Math.max(1, Math.ceil((height * height) / total));
  const maxThumbStart = height - thumbHeight;
  const maxStart = total - height;
  const thumbStart = maxStart === 0 ? 0 : Math.round((start * maxThumbStart) / maxStart);
  return index >= thumbStart && index < thumbStart + thumbHeight ? "█" : "░";
}

function splitLines(text: string, limit: number): string[] {
  return limit <= 0 ? [] : text.split("\n").slice(-limit);
}

function sameServices(left: Service[], right: Service[]): boolean {
  return left.length === right.length && left.every((service, index) => service.name === right[index]?.name && service.kind === right[index]?.kind && service.state === right[index]?.state);
}
