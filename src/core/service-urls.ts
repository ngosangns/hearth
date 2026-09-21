// Values for the placeholders a catalog's service URLs may contain (`SERVICE_URL_PLACEHOLDERS`).
// Mirrors rust/crates/ls-core/src/manager/service_urls.rs.

/** How long a looked-up tailnet host is reused: `/v1/urls` is fetched on every client connect and
 * catalog reload, but the host can change (a re-login, a renamed machine). */
const tailnetHostTtlMs = 60_000;
let tailnetHostCache: { at: number; value: string | undefined } | undefined;

/** Answers one placeholder name for `resolveServiceUrls`. */
export function lookupPlaceholder(name: string): string | undefined {
  return name === "tailnetHost" ? tailnetHost() : undefined;
}

/** This machine's Tailscale DNS name, from `$LOCAL_SERVICES_TAILNET_HOST` if set, otherwise
 * `tailscale status --json`'s `Self.DNSName`. `undefined` when Tailscale is unavailable. */
export function tailnetHost(now: number = Date.now()): string | undefined {
  const override = process.env.LOCAL_SERVICES_TAILNET_HOST?.trim();
  if (override) return override.replace(/\.$/, "");
  if (tailnetHostCache && now - tailnetHostCache.at < tailnetHostTtlMs) return tailnetHostCache.value;
  let value: string | undefined;
  try {
    const result = Bun.spawnSync(["tailscale", "status", "--json"], { stdout: "pipe", stderr: "ignore" });
    value = result.exitCode === 0 ? parseTailnetHost(result.stdout.toString()) : undefined;
  } catch {
    value = undefined;
  }
  tailnetHostCache = { at: now, value };
  return value;
}

/** `Self.DNSName` without its trailing root dot. */
export function parseTailnetHost(statusJson: string): string | undefined {
  try {
    const parsed = JSON.parse(statusJson) as { Self?: { DNSName?: unknown } };
    const name = typeof parsed.Self?.DNSName === "string" ? parsed.Self.DNSName.replace(/\.$/, "") : "";
    return name || undefined;
  } catch {
    return undefined;
  }
}
