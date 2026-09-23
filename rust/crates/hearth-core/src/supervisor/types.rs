//! Traits and shared types for `ProcessSupervisor` — port of the top of `src/core/supervisor.ts`
//! (everything above the `ProcessSupervisor` class itself).
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::oneshot;

use crate::catalog::{CommandSpec, ServiceCatalog, ServiceCommand, ServiceId};
use crate::state::{ProcessIdentity, ServiceLifecycleState};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessSignal {
    Sigterm,
    Sigkill,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PosixProcessRecord {
    pub pid: i64,
    pub pgid: i64,
    pub start_identity: String,
    pub command_fingerprint: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DockerContainerRecord {
    pub container_name: String,
    pub container_id: String,
    pub container_started_at: String,
    pub command_fingerprint: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProcessRecord {
    Posix(PosixProcessRecord),
    Docker(DockerContainerRecord),
}

impl ProcessRecord {
    pub fn command_fingerprint(&self) -> &str {
        match self {
            ProcessRecord::Posix(r) => &r.command_fingerprint,
            ProcessRecord::Docker(r) => &r.command_fingerprint,
        }
    }

    pub fn is_docker(&self) -> bool {
        matches!(self, ProcessRecord::Docker(_))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservedProcess {
    pub record: ProcessRecord,
    pub alive: bool,
}

/// A spawned, live process/container — the `exited` channel is consumed exactly once (mirrors the
/// TS `Promise<number>` being `.then()`-ed exactly once in `spawnAndWait`).
pub struct ManagedProcess {
    pub record: ProcessRecord,
    pub exited: oneshot::Receiver<i32>,
}

pub struct SpawnInput {
    pub command: ServiceCommand,
    pub command_fingerprint: String,
    pub service_id: ServiceId,
}

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct SupervisorError(pub String);

impl From<&str> for SupervisorError {
    fn from(value: &str) -> Self {
        SupervisorError(value.to_string())
    }
}
impl From<String> for SupervisorError {
    fn from(value: String) -> Self {
        SupervisorError(value)
    }
}

pub type OnOutput = Arc<dyn Fn(&str) + Send + Sync>;

/// Per-call opt-ins for `ProcessSupervisor::start_with_options`. `kill_unowned` is the
/// wire-level echo of the user's explicit "yes, kill the process holding this port" — the
/// engine never sets it itself, so a queued restart or a sync path can never turn into a kill.
#[derive(Debug, Default, Clone, Copy)]
pub struct StartOptions {
    pub kill_unowned: bool,
}

/// Where a service's stdout/stderr stream lives — what `ProcessAdapter::attach_output` should tail
/// for it. `Process` covers spawned/adopted POSIX services (the raw capture file their stdio was
/// redirected into); `Container` covers Docker services, including `ownership: external` ones that
/// are adopted without ever gaining a `ProcessIdentity`. `since`/`tail` bound the replayed
/// backlog — without either, the container's whole retained log would be re-forwarded on every
/// re-attach.
pub enum OutputSource<'a> {
    Process,
    Container { container_name: &'a str, since: Option<&'a str>, tail: Option<u64> },
}

/// A live output tail from `ProcessAdapter::attach_output`. `is_done` reports whether the tail
/// ended on its own (`docker logs --follow` exits when its container does, so a container restart
/// kills the follower while the map still holds its stop handle) — a caller keying re-attach on
/// "is there an entry" would otherwise never notice.
pub struct OutputTail {
    stop: Option<Box<dyn FnOnce() + Send>>,
    done: Arc<AtomicBool>,
}

impl OutputTail {
    pub fn new(stop: Box<dyn FnOnce() + Send>, done: Arc<AtomicBool>) -> Self {
        Self { stop: Some(stop), done }
    }
    pub fn is_done(&self) -> bool {
        self.done.load(Ordering::SeqCst)
    }
    pub fn stop(mut self) {
        if let Some(stop) = self.stop.take() {
            stop();
        }
        self.done.store(true, Ordering::SeqCst);
    }
}

#[async_trait]
pub trait ProcessAdapter: Send + Sync {
    async fn spawn(&self, input: SpawnInput, on_output: OnOutput) -> Result<ManagedProcess, SupervisorError>;
    async fn inspect(&self, identity: &ProcessIdentity) -> Option<ObservedProcess>;
    async fn signal_group(&self, pgid: i64, signal: ProcessSignal);
    /// Signals exactly one pid, and only if the pid still presents `expected_start_identity` —
    /// the pid-reuse guard lives here so the signal can never land on a recycled process.
    /// Used for unowned port-holders, where `signal_group` is unsafe: an unowned process's group
    /// membership is untrusted (a shared job can hold innocent siblings; pgid 1 must never be
    /// signalled), so only the resolved holder pids are signalled, one at a time.
    async fn signal_pid(&self, pid: i64, expected_start_identity: &str, signal: ProcessSignal);
    /// `None` if unsupported by this adapter (the TS "optional" `stopContainer`/`attachOutput`).
    async fn stop_container(&self, _command: &ServiceCommand, _on_output: OnOutput) -> Option<Result<(), SupervisorError>> {
        None
    }
    /// Returns a live tail handle if output attachment is supported; `None` otherwise.
    fn attach_output(&self, _service_id: &ServiceId, _source: OutputSource<'_>, _on_output: OnOutput) -> Option<OutputTail> {
        None
    }
}

#[async_trait]
pub trait PreparationAdapter: Send + Sync {
    async fn prepare(&self, service_id: &ServiceId, steps: &[String]) -> Result<(), SupervisorError>;
}

/// One process holding a TCP listen socket on a catalog port — what `port_in_use` reports only as
/// a bool. `pgid`/`command` describe it for the "held by pid N (`cmd`)" refusal/prompt text;
/// `start_identity` is the `ps lstart` value the pid-reuse guard compares before signalling, so a
/// holder whose pid was recycled between resolve and kill can never pull an unrelated tree in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortHolder {
    pub pid: i64,
    pub pgid: i64,
    pub start_identity: String,
    pub command: String,
}

#[async_trait]
pub trait ProbeAdapter: Send + Sync {
    async fn tcp(&self, port: u16) -> bool;
    async fn http(&self, url: &str) -> bool;
    async fn container(&self, container_name: &str) -> bool;
    async fn tailnet(&self) -> bool;
    async fn port_in_use(&self, _port: u16) -> Option<bool> {
        None
    }
    /// Who holds the port, when the adapter can resolve it. `None` means "no adapter capability"
    /// (degrades to the generic "an unowned process" wording); `Some(vec![])` means it looked and
    /// the port is free/nobody identifiable was found.
    async fn port_holders(&self, _port: u16) -> Option<Vec<PortHolder>> {
        None
    }
    /// Backs `{ kind: "command" }` readiness. `None` (not just `Some(false)`) means "no adapter
    /// configured" — `ProcessSupervisor::probe` degrades that to `false` (normal readiness-timeout
    /// path), matching the TS optional-adapter sharp edge (never throws).
    async fn command(&self, _command: &CommandSpec, _cwd: Option<&str>) -> Option<bool> {
        None
    }
}

#[async_trait]
pub trait SupervisorClock: Send + Sync {
    fn now_millis(&self) -> i64;
    fn now(&self) -> String {
        format_iso8601_millis(self.now_millis())
    }
    async fn sleep(&self, millis: i64);
}

pub struct SystemClock;
#[async_trait]
impl SupervisorClock for SystemClock {
    fn now_millis(&self) -> i64 {
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as i64
    }
    async fn sleep(&self, millis: i64) {
        tokio::time::sleep(std::time::Duration::from_millis(millis.max(0) as u64)).await;
    }
}

/// Minimal ISO-8601 UTC formatter (`YYYY-MM-DDTHH:MM:SS.sssZ`) — avoids pulling in a datetime crate
/// for the one thing this crate needs a timestamp for. Not a general-purpose calendar library: valid
/// for any real Unix millisecond timestamp, using the standard civil-from-days algorithm.
pub fn format_iso8601_millis(millis: i64) -> String {
    let secs = millis.div_euclid(1000);
    let ms = millis.rem_euclid(1000);
    let days = secs.div_euclid(86400);
    let secs_of_day = secs.rem_euclid(86400);
    let (year, month, day) = civil_from_days(days);
    let hour = secs_of_day / 3600;
    let minute = (secs_of_day % 3600) / 60;
    let second = secs_of_day % 60;
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{ms:03}Z")
}

/// Howard Hinnant's `civil_from_days` algorithm (public domain), days-since-epoch -> (y, m, d).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32; // [1, 12]
    let year = if m <= 2 { y + 1 } else { y };
    (year, m, d)
}

pub fn parse_iso8601_millis(text: &str) -> Option<i64> {
    // Only needs to parse what `format_iso8601_millis` produces (and what the TS side produces,
    // same shape) — not a general ISO-8601 parser.
    let bytes = text.as_bytes();
    if bytes.len() < 24 || bytes[4] != b'-' || bytes[7] != b'-' || bytes[10] != b'T' {
        return None;
    }
    let year: i64 = text.get(0..4)?.parse().ok()?;
    let month: i64 = text.get(5..7)?.parse().ok()?;
    let day: i64 = text.get(8..10)?.parse().ok()?;
    let hour: i64 = text.get(11..13)?.parse().ok()?;
    let minute: i64 = text.get(14..16)?.parse().ok()?;
    let second: i64 = text.get(17..19)?.parse().ok()?;
    let millis: i64 = text.get(20..23)?.parse().ok()?;
    let days = days_from_civil(year, month, day);
    Some(((days * 86400 + hour * 3600 + minute * 60 + second) * 1000) + millis)
}

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

pub struct SupervisorOptions {
    pub process: Arc<dyn ProcessAdapter>,
    pub run_build: Arc<dyn RunBuild>,
    pub probes: Arc<dyn ProbeAdapter>,
    pub preparation: Option<Arc<dyn PreparationAdapter>>,
    pub clock: Arc<dyn SupervisorClock>,
    pub readiness_timeout_ms: i64,
    pub readiness_backoff_ms: i64,
    pub termination_grace_ms: i64,
    pub is_closing: Arc<dyn Fn() -> bool + Send + Sync>,
}

#[async_trait]
pub trait RunBuild: Send + Sync {
    async fn run(
        &self,
        command: &ServiceCommand,
        on_output: OnOutput,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<(), SupervisorError>;
}

#[async_trait]
pub trait Host: Send + Sync {
    fn instance_id(&self) -> String;
    fn catalog(&self) -> Arc<ServiceCatalog>;
    fn service_states(&self) -> Vec<ServiceLifecycleState>;
    /// One service's state, without cloning every other service's.
    ///
    /// The supervisor asks for a single service's state on every log chunk, every readiness-poll
    /// iteration and every transition; routing those through `service_states()` deep-cloned the
    /// whole catalog's states (each with ~8 `String`s) to then discard all but one — hundreds of
    /// allocations per log line on a 30-service catalog. The default implementation preserves that
    /// behaviour for any `Host` that doesn't override it.
    fn service_state(&self, service_id: &ServiceId) -> Option<ServiceLifecycleState> {
        self.service_states().into_iter().find(|s| &s.service_id == service_id)
    }
    async fn set_service_state(&self, next: ServiceLifecycleState);
    async fn append_log(&self, service_id: &str, data: &str);
    fn publish(&self, event_type: &str, data: serde_json::Value);
    fn record_background_error(&self, _scope: &str, _error: &str) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso8601_roundtrip() {
        let cases: [i64; 5] = [0, 1_726_800_000_000, -1, 1_000, 253_402_300_799_000];
        for millis in cases {
            let text = format_iso8601_millis(millis);
            let parsed = parse_iso8601_millis(&text).unwrap();
            assert_eq!(parsed, millis, "roundtrip failed for {millis} -> {text}");
        }
    }

    #[test]
    fn iso8601_known_value() {
        // 2024-01-01T00:00:00.000Z
        assert_eq!(format_iso8601_millis(1_704_067_200_000), "2024-01-01T00:00:00.000Z");
    }
}
