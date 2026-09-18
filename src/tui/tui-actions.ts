import { Key, matchesKey } from "@oh-my-pi/pi-tui";

import type { Service } from "./state";

export type TuiAction = "quit" | "up" | "down" | "start-all" | "stop-all" | "start" | "stop" | "restart";

export function keyboardAction(data: string, selected: Service | undefined): TuiAction | undefined {
  if (matchesKey(data, Key.ctrl("c")) || matchesKey(data, "q")) return "quit";
  if (matchesKey(data, Key.up) || matchesKey(data, "k")) return "up";
  if (matchesKey(data, Key.down) || matchesKey(data, "j")) return "down";
  if (matchesKey(data, "r")) return "restart";
  if (matchesKey(data, "shift+r")) return "restart";
  if (matchesKey(data, "x")) return "stop";
  if (matchesKey(data, "a")) return "start-all";
  if (matchesKey(data, "s")) return "stop-all";
  if (matchesKey(data, Key.enter) || matchesKey(data, Key.space)) return selected?.state === "stopped" || selected?.state === "queued-start" ? "start" : "stop";
  return undefined;
}
