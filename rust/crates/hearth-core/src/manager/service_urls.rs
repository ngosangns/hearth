//! Values for the placeholders a catalog's service URLs may contain (`SERVICE_URL_PLACEHOLDERS`).
use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// How long a looked-up tailnet host is reused. `/v1/urls` is fetched on every client connect and
/// catalog reload; spawning `tailscale` each time is needless, but the host can change (a re-login,
/// a renamed machine), so it is not cached forever either.
const TAILNET_HOST_TTL: Duration = Duration::from_secs(60);
/// `tailscale status` against a wedged tailscaled would otherwise block `/v1/urls` indefinitely.
const TAILNET_QUERY_TIMEOUT: Duration = Duration::from_secs(3);

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
    // The cache lock is never held across the `tailscale` spawn: a slow query would otherwise stall
    // every other caller behind it. Two concurrent misses both query, which is harmless.
    if let Some((at, value)) = TAILNET_HOST.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).as_ref() {
        if at.elapsed() < TAILNET_HOST_TTL {
            return value.clone();
        }
    }
    let value = query_tailnet_host();
    *TAILNET_HOST.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some((Instant::now(), value.clone()));
    value
}

fn query_tailnet_host() -> Option<String> {
    let mut child = Command::new("tailscale").args(["status", "--json"]).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().ok()?;
    let mut stdout = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut text = String::new();
        let _ = stdout.read_to_string(&mut text);
        text
    });
    let deadline = Instant::now() + TAILNET_QUERY_TIMEOUT;
    let succeeded = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status.success(),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            _ => {
                // Killing it closes its stdout, which ends the reader thread.
                let _ = child.kill();
                let _ = child.wait();
                break false;
            }
        }
    };
    let text = reader.join().ok()?;
    if !succeeded {
        return None;
    }
    parse_tailnet_host(&text)
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
