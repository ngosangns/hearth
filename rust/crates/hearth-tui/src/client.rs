//! The TUI's daemon client: typed calls through `hearth_cli::ManagerClient` (the same client the
//! CLI and MCP use) plus the SSE reconnect loop. The TUI has no direct access to processes. Tests
//! drive a real bootstrapped `HearthManager` over real HTTP rather than a fake transport.
use std::path::PathBuf;
use std::time::Duration;

use futures_util::StreamExt;
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use hearth_cli::{LocalctlError, ManagerClient, EXIT_FAILED};
use hearth_core::catalog::ServiceCatalog;
use hearth_core::state::{ManagerEvent, Operation, ServiceLifecycleState, ServiceOperationKind};

use crate::state::LogSlice;

pub const LOG_TAIL_BYTES: u64 = 16 * 1024;
pub const MAX_SSE_FRAME_BYTES: usize = 64 * 1024;
const RECONNECT_DELAY_MS: u64 = 250;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EventReplay {
    pub epoch: String,
    pub reset: bool,
    pub latest_sequence: u64,
}

/// Emitted by `watch()`'s reconnect loop, in strict order, over an `mpsc` channel. The *owner* of a
/// `TuiState` (the single-threaded `run_tui` event loop) turns `BeginConnection` into a real
/// `TuiFence` by calling `state.begin_connection()`, then applies that same fence to every message
/// that follows until the next `BeginConnection` — this is what lets a stale reconnect attempt's
/// leftover messages never overwrite state a newer connection attempt has already superseded,
/// without `watch()` itself needing to know anything about fences.
pub enum WatchEvent {
    BeginConnection,
    Snapshot(Vec<ServiceLifecycleState>),
    Replay(EventReplay),
    ManagerEvent(ManagerEvent),
    Unavailable(String),
    /// A background "start all" wait finished — sent by the task `run_all` spawns, not by
    /// `watch()`, and not fenced: it reports an operation, not connection state.
    BulkStartFinished(Result<Operation, String>),
}

/// Replaces control characters so a daemon-supplied message can't move the cursor or break a row.
pub(crate) fn safe_message(message: &str) -> String {
    message.chars().map(|c| if c == '\r' || c == '\n' || (c as u32) < 0x20 || c as u32 == 0x7f { ' ' } else { c }).collect()
}

pub struct ManagerTuiClient {
    api: ManagerClient,
}

impl ManagerTuiClient {
    pub fn new(root: PathBuf, catalog: ServiceCatalog) -> Self {
        Self { api: ManagerClient::new(root, catalog) }
    }

    pub fn root(&self) -> &std::path::Path {
        self.api.root()
    }

    pub async fn snapshot(&self) -> Result<Vec<ServiceLifecycleState>, LocalctlError> {
        self.api.services().await
    }

    /// Every resolved service URL (`GET /v1/urls`).
    pub async fn urls(&self) -> Result<Vec<hearth_core::catalog::ResolvedServiceUrl>, LocalctlError> {
        let body = self.api.urls().await?;
        serde_json::from_value(body["urls"].clone()).map_err(|e| LocalctlError { exit_code: EXIT_FAILED, message: e.to_string() })
    }

    /// `GET /v1/daemon/log` — the daemon's own log, not a service id.
    pub async fn daemon_log(&self) -> Result<String, LocalctlError> {
        let body = self.api.request("/v1/daemon/log?bytes=16384", reqwest::Method::GET, None).await?;
        Ok(body.get("data").and_then(|v| v.as_str()).unwrap_or("").to_string())
    }

    pub async fn catalog(&self) -> Result<hearth_core::catalog::ServiceCatalog, LocalctlError> {
        self.api.catalog().await
    }

    pub async fn log(&self, service_id: &str, cursor: Option<u64>, generation: Option<u64>) -> Result<LogSlice, LocalctlError> {
        let slice = self.api.log(service_id, cursor, generation, Some(LOG_TAIL_BYTES)).await?;
        Ok(LogSlice { data: slice.data, next_cursor: slice.next_cursor, generation: slice.generation, reset: slice.reset })
    }

    pub async fn operation(&self, id: &str) -> Result<Operation, LocalctlError> {
        self.api.operation(id).await
    }

    pub async fn action(&self, service_id: &str, action: ServiceOperationKind, kill_unowned: bool) -> Result<Operation, LocalctlError> {
        self.api.submit(action, service_id, kill_unowned).await
    }

    pub async fn bulk_start(&self, targets: &[String]) -> Result<Operation, LocalctlError> {
        self.api.bulk_start(targets, false).await
    }

    pub async fn wait_operation(&self, id: &str) -> Result<Operation, LocalctlError> {
        self.api.wait(id, None).await
    }

    pub async fn request(&self, path: &str, method: reqwest::Method, body: Option<&Value>) -> Result<Value, LocalctlError> {
        self.api.request(path, method, body).await
    }

    /// Pid from the daemon lock. Fails when no daemon is up; callers leave the header blank.
    pub async fn daemon_pid(&self) -> Result<i64, LocalctlError> {
        Ok(self.api.connection().await?.metadata.pid)
    }

    /// The current connection, for work that must outlive a borrow of this client (a spawned wait).
    pub async fn connection(&self) -> Result<hearth_cli::Client, LocalctlError> {
        self.api.connection().await
    }

    /// Runs the reconnect loop until `cancel` fires: on each attempt, fetches a fresh `/v1/services`
    /// snapshot, then streams `/v1/events/stream` until it errors, sending every step as a
    /// `WatchEvent` in order. Never returns early on a transport error — only `cancel` stops it.
    pub async fn watch(&self, tx: mpsc::Sender<WatchEvent>, cancel: CancellationToken) {
        let mut after: Option<u64> = None;
        let mut epoch: Option<String> = None;
        while !cancel.is_cancelled() {
            let _ = tx.send(WatchEvent::BeginConnection).await;
            if let Err(message) = self.watch_once(&mut after, &mut epoch, &tx, &cancel).await {
                if cancel.is_cancelled() {
                    return;
                }
                let _ = tx.send(WatchEvent::Unavailable(safe_message(&message))).await;
            }
            if cancel.is_cancelled() {
                return;
            }
            tokio::select! {
                _ = cancel.cancelled() => return,
                _ = tokio::time::sleep(Duration::from_millis(RECONNECT_DELAY_MS)) => {}
            }
        }
    }

    async fn watch_once(&self, after: &mut Option<u64>, epoch: &mut Option<String>, tx: &mpsc::Sender<WatchEvent>, cancel: &CancellationToken) -> Result<(), String> {
        let services = self.api.services().await.map_err(|e| e.message)?;
        let _ = tx.send(WatchEvent::Snapshot(services)).await;
        self.stream_events(after, epoch, tx, cancel).await
    }

    async fn stream_events(&self, after: &mut Option<u64>, epoch: &mut Option<String>, tx: &mpsc::Sender<WatchEvent>, cancel: &CancellationToken) -> Result<(), String> {
        let response = self.api.event_stream(*after, epoch.as_deref()).await.map_err(|e| e.message)?;
        let mut stream = response.bytes_stream();
        // Raw bytes, decoded per complete frame: a multi-byte character split across two TCP
        // chunks must not turn into U+FFFD.
        let mut buffer: Vec<u8> = Vec::new();
        loop {
            let chunk = tokio::select! {
                _ = cancel.cancelled() => return Ok(()),
                chunk = stream.next() => chunk,
            };
            let bytes = match chunk {
                None => return Ok(()),
                Some(Err(error)) => return Err(error.to_string()),
                Some(Ok(bytes)) => bytes,
            };
            append_sse_chunk(&mut buffer, &bytes)?;
            while let Some(frame) = next_sse_frame(&mut buffer) {
                if is_sse_comment(&frame) {
                    continue;
                }
                let parsed = parse_sse(&frame).ok_or_else(|| "event stream frame is malformed".to_string())?;
                if parsed.event_type == "replay" {
                    let replay: EventReplay = serde_json::from_value(parsed.data).map_err(|_| "event stream payload is malformed".to_string())?;
                    *epoch = Some(replay.epoch.clone());
                    if replay.reset {
                        *after = Some(replay.latest_sequence);
                    }
                    let _ = tx.send(WatchEvent::Replay(replay)).await;
                } else {
                    let event: ManagerEvent = serde_json::from_value(parsed.data).map_err(|_| "event stream payload is malformed".to_string())?;
                    *after = Some(event.sequence);
                    let _ = tx.send(WatchEvent::ManagerEvent(event)).await;
                }
            }
        }
    }
}

/// Appends one network chunk to the pending-frame buffer, failing once a single frame would exceed
/// `MAX_SSE_FRAME_BYTES`.
pub fn append_sse_chunk(buffer: &mut Vec<u8>, chunk: &[u8]) -> Result<(), String> {
    if buffer.len() + chunk.len() > MAX_SSE_FRAME_BYTES {
        return Err("event stream frame exceeds limit".to_string());
    }
    buffer.extend_from_slice(chunk);
    Ok(())
}

/// Removes and decodes the next complete (blank-line-terminated) frame from `buffer`, if any.
pub fn next_sse_frame(buffer: &mut Vec<u8>) -> Option<String> {
    let boundary = buffer.windows(2).position(|pair| pair == b"\n\n")?;
    let frame = String::from_utf8_lossy(&buffer[..boundary]).into_owned();
    buffer.drain(..boundary + 2);
    Some(frame)
}

/// A frame of only comment (`:`) or blank lines — the server's keep-alive. Carries no event.
pub fn is_sse_comment(frame: &str) -> bool {
    frame.lines().all(|line| line.is_empty() || line.starts_with(':'))
}

pub struct SseFrame {
    pub event_type: String,
    pub data: Value,
}

pub fn parse_sse(frame: &str) -> Option<SseFrame> {
    if frame.len() > MAX_SSE_FRAME_BYTES {
        return None;
    }
    let event_type = frame.lines().find(|l| l.starts_with("event:")).map(|l| l["event:".len()..].trim().to_string()).unwrap_or_else(|| "message".to_string());
    let raw = frame.lines().filter(|l| l.starts_with("data:")).map(|l| l["data:".len()..].trim()).collect::<Vec<_>>().join("\n");
    if raw.is_empty() {
        return None;
    }
    serde_json::from_str(&raw).ok().map(|data| SseFrame { event_type, data })
}

/// Re-fetches the selected service's log tail at its current cursor, falling back to a
/// from-scratch fetch (and a `replace_log` rather than an incremental append) when the incremental
/// fetch came back truncated at exactly the tail-byte cap.
pub async fn refresh_selected_log(client: &ManagerTuiClient, state: &mut crate::state::TuiState, fence: crate::state::TuiFence) -> Result<bool, String> {
    let service = state.selection.selected_name.clone();
    if service.is_empty() {
        return Ok(false);
    }
    let cursor = state.log_cursor(&service);
    let delta = client.log(&service, cursor.cursor, cursor.generation).await.map_err(|e| e.message)?;
    let data_len = delta.data.len();
    if !state.apply_log(fence, &service, &delta) {
        return Ok(false);
    }
    if cursor.cursor.is_none() || data_len < LOG_TAIL_BYTES as usize {
        return Ok(true);
    }
    let latest = client.log(&service, None, None).await.map_err(|e| e.message)?;
    Ok(state.replace_log(fence, &service, &latest))
}

#[cfg(test)]
mod tests {
    use super::*;
    use hearth_core::manager::{HearthManager, ShutdownMode};
    use std::sync::Arc;

    /// Reaps spawned services (`nc -lk` listeners) even when the test panics before its
    /// explicit shutdown — without this each failing run leaves orphaned listeners behind.
    struct StopAllOnDrop(Arc<HearthManager>);
    impl Drop for StopAllOnDrop {
        fn drop(&mut self) {
            let manager = self.0.clone();
            let _ = std::thread::spawn(move || {
                if let Ok(rt) = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    rt.block_on(async move {
                        let _ = tokio::time::timeout(
                            std::time::Duration::from_secs(15),
                            manager.shutdown(ShutdownMode::StopServices),
                        )
                        .await;
                    });
                }
            })
            .join();
        }
    }

    #[test]
    fn parses_a_replay_frame() {
        let frame = "event: replay\ndata: {\"epoch\":\"e1\",\"reset\":false,\"latestSequence\":5}";
        let parsed = parse_sse(frame).unwrap();
        assert_eq!(parsed.event_type, "replay");
        let replay: EventReplay = serde_json::from_value(parsed.data).unwrap();
        assert_eq!(replay.epoch, "e1");
        assert_eq!(replay.latest_sequence, 5);
    }

    #[test]
    fn parses_a_multi_line_data_field() {
        let frame = "event: service.lifecycle\ndata: {\"sequence\":1,\ndata: \"type\":\"x\"}";
        let parsed = parse_sse(frame).unwrap();
        assert_eq!(parsed.event_type, "service.lifecycle");
        assert_eq!(parsed.data, serde_json::json!({"sequence": 1, "type": "x"}));
    }

    #[test]
    fn defaults_the_event_type_to_message_without_an_event_line() {
        let frame = "data: {\"a\":1}";
        let parsed = parse_sse(frame).unwrap();
        assert_eq!(parsed.event_type, "message");
    }

    #[test]
    fn rejects_a_frame_with_no_data_field() {
        assert!(parse_sse("event: replay").is_none());
    }

    #[test]
    fn rejects_an_oversized_chunk_append() {
        let huge = vec![b'x'; MAX_SSE_FRAME_BYTES + 1];
        assert!(append_sse_chunk(&mut Vec::new(), &huge).is_err());
    }

    /// axum's keep-alive is a bare `:` comment frame; it must be skipped, not treated as a
    /// malformed frame that tears the stream down.
    #[test]
    fn keep_alive_comment_frames_are_skipped() {
        assert!(is_sse_comment(":"));
        assert!(is_sse_comment(": ping"));
        assert!(is_sse_comment(""));
        assert!(!is_sse_comment("event: replay\ndata: {}"));
        let mut buffer = Vec::new();
        append_sse_chunk(&mut buffer, b":\n\nevent: replay\ndata: {\"epoch\":\"e\",\"reset\":false,\"latestSequence\":1}\n\n").unwrap();
        let frames: Vec<String> = std::iter::from_fn(|| next_sse_frame(&mut buffer)).filter(|f| !is_sse_comment(f)).collect();
        assert_eq!(frames.len(), 1);
        assert_eq!(parse_sse(&frames[0]).unwrap().event_type, "replay");
        assert!(buffer.is_empty());
    }

    /// A multi-byte character split across two network chunks decodes intact once the frame
    /// completes.
    #[test]
    fn a_character_split_across_chunks_decodes_intact() {
        let frame = "data: {\"message\":\"héllo\"}\n\n".as_bytes();
        let split = frame.iter().position(|b| *b == 0xC3).unwrap() + 1;
        let mut buffer = Vec::new();
        append_sse_chunk(&mut buffer, &frame[..split]).unwrap();
        assert!(next_sse_frame(&mut buffer).is_none());
        append_sse_chunk(&mut buffer, &frame[split..]).unwrap();
        let parsed = parse_sse(&next_sse_frame(&mut buffer).unwrap()).unwrap();
        assert_eq!(parsed.data["message"], "héllo");
    }

    /// End-to-end through a real daemon: `action(..., kill_unowned: true)` is the TUI's version of
    /// the confirmed reclaim — the squatter is terminated and the start continues on the freed port.
    #[tokio::test]
    #[allow(clippy::zombie_processes)] // the squatter is killed and reaped at the end of the test
    async fn action_with_kill_unowned_reclaims_the_held_port() {
        use hearth_core::catalog::{CommandSpec, ReadinessSpec, ServiceCatalog, ServiceCommand, ServiceDefinition, ServiceKind, ServiceProfiles, ServiceRunProfile, StartFailurePolicy};
        use hearth_core::manager::{bootstrap, HearthManagerOptions};
        use hearth_core::state::OperationStatus;
        use std::collections::HashMap;
        use std::os::unix::process::CommandExt;

        let dir = tempfile::tempdir().unwrap();
        let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let mut squatter = std::process::Command::new("nc").args(["-lk", &port.to_string()]).process_group(0).spawn().unwrap();
        for _ in 0..100 {
            if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        let service = ServiceDefinition {
            id: "api".to_string(),
            label: None,
            kind: Some(ServiceKind::Application),
            ownership: None,
            disabled: false,
            profiles: ServiceProfiles {
                run: ServiceRunProfile::Verified {
                    command: ServiceCommand { command: CommandSpec::Shell { shell: format!("exec nc -lk {port}"), exec: Some(true) }, cwd: "/tmp".to_string(), environment: None, container_name: None, docker_stop_command: None },
                    readiness: ReadinessSpec::Tcp { port },
                    readiness_timeout_ms: Some(5_000),
                    preparation: None,
                    preparation_command: None,
                },
                build: None,
            },
            ports: None,
            urls: None,
            artifact: None,
        };
        let catalog = ServiceCatalog {
            services: vec![service],
            groups: HashMap::new(),
            group_tree: Vec::new(),
            compose_file: None,
            runtime_directory: Some(dir.path().to_string_lossy().to_string()),
            start_failure_policy: StartFailurePolicy::StopOnFirstFailureKeepStarted,
            private_file_guard: Some(false),
        };
        let manager = bootstrap(HearthManagerOptions {
            runtime_directory: None,
            root: Some(PathBuf::from("/tmp")),
            catalog: catalog.clone(),
            event_capacity: None,
            log_tail_bytes: None,
            log_max_bytes: None,
            log_rotation_count: None,
            supervisor: None,
        shared: None,
        })
        .await
        .unwrap();
        let _stop_all = StopAllOnDrop(manager.clone());
        let client = ManagerTuiClient::new(PathBuf::from("/tmp"), catalog);

        // Without the flag the start refuses — and the squatter is left alone.
        let refused = client.action("api", ServiceOperationKind::Start, false).await.unwrap();
        assert_eq!(client.wait_operation(&refused.id).await.unwrap().status, OperationStatus::Failed);
        assert!(squatter.try_wait().unwrap().is_none());

        let accepted = client.action("api", ServiceOperationKind::Start, true).await.unwrap();
        assert_eq!(client.wait_operation(&accepted.id).await.unwrap().status, OperationStatus::Succeeded);
        for _ in 0..50 {
            if squatter.try_wait().unwrap().is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(squatter.try_wait().unwrap().is_some(), "the confirmed reclaim must terminate the squatter");
        let _ = squatter.kill();
        let _ = squatter.wait();
        manager.shutdown(ShutdownMode::StopServices).await;
    }
}
