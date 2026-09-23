//! Values for the placeholders a catalog's service URLs may contain (`SERVICE_URL_PLACEHOLDERS`).
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// How long a looked-up tailnet host is reused. `/v1/urls` is fetched on every client connect and
/// catalog reload; spawning `tailscale` each time is needless, but the host can change (a re-login,
/// a renamed machine), so it is not cached forever either.
const TAILNET_HOST_TTL: Duration = Duration::from_secs(60);

static TAILNET_HOST: Mutex<Option<(Instant, Option<String>)>> = Mutex::new(None);

/// Answers one placeholder name for `resolve_service_urls`.
pub fn lookup_placeholder(name: &str) -> Option<String> {
    match name {
        "tailnetHost" => tailnet_host(),
        _ => None,
    }
}

/// This machine's Tailscale DNS name (e.g. `macbook.tail2b20d9.ts.net`), from
/// `$HEARTH_TAILNET_HOST` if set, otherwise `tailscale status --json`'s `Self.DNSName`.
/// `None` when Tailscale is not installed, not running, or not logged in.
pub fn tailnet_host() -> Option<String> {
    if let Some(value) = std::env::var("HEARTH_TAILNET_HOST").ok().filter(|v| !v.trim().is_empty()) {
        return Some(value.trim().trim_end_matches('.').to_string());
    }
    let mut cache = TAILNET_HOST.lock().unwrap();
    if let Some((at, value)) = cache.as_ref() {
        if at.elapsed() < TAILNET_HOST_TTL {
            return value.clone();
        }
    }
    let value = query_tailnet_host();
    *cache = Some((Instant::now(), value.clone()));
    value
}

fn query_tailnet_host() -> Option<String> {
    let output = std::process::Command::new("tailscale").args(["status", "--json"]).output().ok()?;
    if !output.status.success() {
        return None;
    }
    parse_tailnet_host(&String::from_utf8_lossy(&output.stdout))
}

/// `Self.DNSName` without its trailing root dot.
pub fn parse_tailnet_host(status_json: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(status_json).ok()?;
    let name = value.get("Self")?.get("DNSName")?.as_str()?.trim_end_matches('.');
    (!name.is_empty()).then(|| name.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_dns_name_without_its_trailing_dot() {
        assert_eq!(parse_tailnet_host(r#"{"Self":{"DNSName":"macbook.tail2b20d9.ts.net."}}"#).as_deref(), Some("macbook.tail2b20d9.ts.net"));
    }

    #[test]
    fn a_logged_out_or_malformed_status_has_no_host() {
        assert_eq!(parse_tailnet_host(r#"{"Self":{"DNSName":""}}"#), None);
        assert_eq!(parse_tailnet_host(r#"{"BackendState":"NeedsLogin"}"#), None);
        assert_eq!(parse_tailnet_host("not json"), None);
    }
}
