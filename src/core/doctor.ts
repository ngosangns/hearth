import { existsSync } from "node:fs";
import { connect } from "node:net";

import { validateCatalog, type ServiceCatalog } from "./catalog";
import { isSupportedLocalServicesPlatform, unsupportedPlatformMessage, type LocalServicesPlatform } from "./platform";

export type DoctorCheck = { name: string; ok: boolean; detail: string };
export type DoctorReport = { ok: boolean; checks: DoctorCheck[]; unresolvedProfiles: string[] };
export type CommandResult = { ok: boolean; output: string };
export type DoctorAdapter = { command(command: string, args: string[]): Promise<CommandResult>; path(path: string): boolean; port(port: number): Promise<boolean>; platform?(): LocalServicesPlatform };

const tcp = (port: number): Promise<boolean> =>
  new Promise((resolve) => {
    const socket = connect({ host: "127.0.0.1", port });
    const done = (ok: boolean): void => {
      socket.destroy();
      resolve(ok);
    };
    socket.once("connect", () => done(true));
    socket.once("error", () => done(false));
    socket.setTimeout(250, () => done(false));
  });

export const defaultDoctorAdapter: DoctorAdapter = {
  command: async (command, args) => {
    try {
      const child = Bun.spawn([command, ...args], { stdout: "pipe", stderr: "pipe" });
      const [stdout, stderr, code] = await Promise.all([new Response(child.stdout).text(), new Response(child.stderr).text(), child.exited]);
      return { ok: code === 0, output: `${stdout}\n${stderr}`.trim() };
    } catch {
      return { ok: false, output: "" };
    }
  },
  path: existsSync,
  port: tcp,
};

export type DoctorCommandCheck = { name: string; command: string; args: string[]; ok?: (result: CommandResult) => boolean; detail?: (result: CommandResult) => string };
export type DoctorPathCheck = { name: string; path: string };
export type DoctorPortCheck = { name: string; port: number };
export type DoctorChecks = { commands?: readonly DoctorCommandCheck[]; paths?: readonly DoctorPathCheck[]; ports?: readonly DoctorPortCheck[] };

/** Thin generic engine (tcp probe / command-exec probe / path-exists probe) driving a caller-supplied
 * check list — infra and viclass each need a completely different toolchain/port list, so nothing
 * project-specific belongs here. */
export async function runDoctor(catalog: ServiceCatalog, checks: DoctorChecks = {}, adapter: DoctorAdapter = defaultDoctorAdapter): Promise<DoctorReport> {
  const platform = adapter.platform?.() ?? process.platform;
  const results: DoctorCheck[] = [{ name: "platform", ok: isSupportedLocalServicesPlatform(platform), detail: isSupportedLocalServicesPlatform(platform) ? platform : unsupportedPlatformMessage(platform) }];
  for (const check of checks.commands ?? []) {
    const result = await adapter.command(check.command, check.args);
    results.push({ name: check.name, ok: check.ok ? check.ok(result) : result.ok, detail: check.detail ? check.detail(result) : check.command });
  }
  for (const check of checks.paths ?? []) results.push({ name: check.name, ok: adapter.path(check.path), detail: check.path });
  for (const check of checks.ports ?? []) results.push({ name: check.name, ok: await adapter.port(check.port), detail: `127.0.0.1:${check.port}` });
  const validation = validateCatalog(catalog);
  const unresolvedProfiles = validation.warnings.filter((warning) => warning.includes("command is unresolved"));
  return { ok: results.every((check) => check.ok) && !validation.errors.length, checks: results, unresolvedProfiles };
}
