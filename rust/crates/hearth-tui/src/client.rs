//! The TUI's daemon client: typed calls through `hearth_cli::ManagerClient` (the same client the
//! CLI and MCP use) plus the SSE reconnect loop. The TUI has no direct access to processes. Tests
//! drive a real bootstrapped `HearthManager` over real HTTP rather than a fake transport.
use std::path::PathBuf;
use std::time::Duration;

use eventsource_stream::{EventStream, EventStreamError};
use futures_util::{Stream, StreamExt};
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
    message
        .chars()
        .map(|c| {
            if c == '\r' || c == '\n' || (c as u32) < 0x20 || c as u32 == 0x7f {
                ' '
            } else {
                c
            }
        })
        .collect()
}

pub struct ManagerTuiClient {
    api: ManagerClient,
}

impl ManagerTuiClient {
    pub fn new(root: PathBuf, catalog: ServiceCatalog) -> Self {
        Self {
            api: ManagerClient::new(root, catalog),
        }
    }

    pub fn root(&self) -> &std::path::Path {
        self.api.root()
    }

    pub async fn snapshot(&self) -> Result<Vec<ServiceLifecycleState>, LocalctlError> {
        let started = std::time::Instant::now();
        let result = self.api.services().await;
        crate::profile::http("snapshot", started.elapsed());
        result
    }

    /// Every resolved service URL (`GET /v1/urls`).
    pub async fn urls(
        &self,
    ) -> Result<Vec<hearth_core::catalog::ResolvedServiceUrl>, LocalctlError> {
        let started = std::time::Instant::now();
        let body = self.api.urls().await;
        crate::profile::http("urls", started.elapsed());
        let body = body?;
        serde_json::from_value(body["urls"].clone()).map_err(|e| LocalctlError {
            exit_code: EXIT_FAILED,
            message: e.to_string(),
        })
    }

    /// `GET /v1/daemon/log` — the daemon's own log, not a service id.
    pub async fn daemon_log(&self) -> Result<String, LocalctlError> {
        let started = std::time::Instant::now();
        let body = self
            .api
            .request("/v1/daemon/log?bytes=16384", reqwest::Method::GET, None)
            .await;
        crate::profile::http("daemon_log", started.elapsed());
        let body = body?;
        Ok(body
            .get("data")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string())
    }

    pub async fn catalog(&self) -> Result<hearth_core::catalog::ServiceCatalog, LocalctlError> {
        self.api.catalog().await
    }

    pub async fn log(
        &self,
        service_id: &str,
        cursor: Option<u64>,
        generation: Option<u64>,
    ) -> Result<LogSlice, LocalctlError> {
        let started = std::time::Instant::now();
        let slice = self
            .api
            .log(service_id, cursor, generation, Some(LOG_TAIL_BYTES))
            .await;
        crate::profile::http("log", started.elapsed());
        let slice = slice?;
        Ok(LogSlice {
            data: slice.data,
            next_cursor: slice.next_cursor,
            generation: slice.generation,
            reset: slice.reset,
        })
    }

    pub async fn operation(&self, id: &str) -> Result<Operation, LocalctlError> {
        self.api.operation(id).await
    }

    pub async fn action(
        &self,
        service_id: &str,
        action: ServiceOperationKind,
        kill_unowned: bool,
    ) -> Result<Operation, LocalctlError> {
        self.api.submit(action, service_id, kill_unowned).await
    }

    pub async fn bulk_start(&self, targets: &[String]) -> Result<Operation, LocalctlError> {
        self.api.bulk_start(targets, false).await
    }

    pub async fn wait_operation(&self, id: &str) -> Result<Operation, LocalctlError> {
        self.api.wait(id, None).await
    }

    pub async fn request(
        &self,
        path: &str,
        method: reqwest::Method,
        body: Option<&Value>,
    ) -> Result<Value, LocalctlError> {
        let started = std::time::Instant::now();
        let result = self.api.request(path, method, body).await;
        crate::profile::http("request", started.elapsed());
        result
    }

    /// Pid from the daemon lock. Fails when no daemon is up; callers leave the header blank.
    pub async fn daemon_pid(&self) -> Result<i64, LocalctlError> {
        let started = std::time::Instant::now();
        let connection = self.api.connection().await;
        crate::profile::http("daemon_pid", started.elapsed());
        Ok(connection?.metadata.pid)
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
                let _ = tx
                    .send(WatchEvent::Unavailable(safe_message(&message)))
                    .await;
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

    async fn watch_once(
        &self,
        after: &mut Option<u64>,
        epoch: &mut Option<String>,
        tx: &mpsc::Sender<WatchEvent>,
        cancel: &CancellationToken,
    ) -> Result<(), String> {
        let services = self.api.services().await.map_err(|e| e.message)?;
        let _ = tx.send(WatchEvent::Snapshot(services)).await;
        self.stream_events(after, epoch, tx, cancel).await
    }

    async fn stream_events(
        &self,
        after: &mut Option<u64>,
        epoch: &mut Option<String>,
        tx: &mpsc::Sender<WatchEvent>,
        cancel: &CancellationToken,
    ) -> Result<(), String> {
        let response = self
            .api
            .event_stream(*after, epoch.as_deref())
            .await
            .map_err(|e| e.message)?;
        // `eventsource-stream` decodes complete frames (comment keep-alives dropped, multi-line
        // `data:` joined, UTF-8 split across chunks intact); `capped_sse_bytes` keeps the
        // `MAX_SSE_FRAME_BYTES` bound it buffers against.
        let mut events = EventStream::new(capped_sse_bytes(response.bytes_stream()));
        loop {
            let event = tokio::select! {
                _ = cancel.cancelled() => return Ok(()),
                event = events.next() => event,
            };
            let event = match event {
                None => return Ok(()),
                Some(Err(EventStreamError::Transport(message))) => return Err(message),
                Some(Err(_)) => return Err("event stream frame is malformed".to_string()),
                Some(Ok(event)) => event,
            };
            if event.event == "replay" {
                let replay: EventReplay = serde_json::from_str(&event.data)
                    .map_err(|_| "event stream payload is malformed".to_string())?;
                *epoch = Some(replay.epoch.clone());
                if replay.reset {
                    *after = Some(replay.latest_sequence);
                }
                let _ = tx.send(WatchEvent::Replay(replay)).await;
            } else {
                let event: ManagerEvent = serde_json::from_str(&event.data)
                    .map_err(|_| "event stream payload is malformed".to_string())?;
                *after = Some(event.sequence);
                let _ = tx.send(WatchEvent::ManagerEvent(event)).await;
            }
        }
    }
}

/// Counts bytes in the current (not yet terminated) SSE frame across chunks, so `capped_sse_bytes`
/// can fail a stream whose pending frame outgrows the cap — `EventStream` buffers internally, so
/// the limit has to live underneath it on the raw bytes.
#[derive(Default)]
struct SseFrameCap {
    /// Bytes consumed since the last completed frame boundary.
    pending: usize,
    /// The last parsed unit was a complete line terminator.
    last_term: bool,
    /// The last byte was a CR whose CRLF may continue into the next byte.
    pending_cr: bool,
}

impl SseFrameCap {
    /// Returns `Err` once the pending frame exceeds `limit`. A frame boundary is two adjacent
    /// line terminators, where each terminator is `\n`, `\r`, or `\r\n` — possibly split across
    /// chunks.
    fn feed(&mut self, chunk: &[u8], limit: usize) -> Result<(), String> {
        // Index after the last boundary completed inside this chunk, if any.
        let mut boundary: Option<usize> = None;
        for (i, &byte) in chunk.iter().enumerate() {
            match byte {
                b'\r' => {
                    if self.last_term {
                        boundary = Some(i + 1);
                    }
                    self.last_term = true;
                    self.pending_cr = true;
                }
                b'\n' => {
                    if self.pending_cr {
                        // Completes the CRLF; if that CR had just finished a boundary, the
                        // boundary ends here instead.
                        if boundary == Some(i) {
                            boundary = Some(i + 1);
                        }
                        self.pending_cr = false;
                    } else {
                        if self.last_term {
                            boundary = Some(i + 1);
                        }
                        self.last_term = true;
                    }
                }
                _ => {
                    self.last_term = false;
                    self.pending_cr = false;
                }
            }
        }
        self.pending = match boundary {
            Some(index) => chunk.len() - index,
            None => self.pending + chunk.len(),
        };
        if self.pending > limit {
            return Err("event stream frame exceeds limit".to_string());
        }
        Ok(())
    }
}

/// Maps a response byte stream to one that fails with `event stream frame exceeds limit` as soon
/// as a single frame's bytes pass `MAX_SSE_FRAME_BYTES` mid-buffer — the same bound the removed
/// hand-rolled parser enforced on its own buffer.
fn capped_sse_bytes<S, B, E>(inner: S) -> impl Stream<Item = Result<B, String>>
where
    S: Stream<Item = Result<B, E>>,
    B: AsRef<[u8]>,
    E: std::fmt::Display,
{
    inner
        .map(|item| item.map_err(|e| e.to_string()))
        .scan(SseFrameCap::default(), |cap, item| {
            std::future::ready(Some(item.and_then(|bytes| {
                cap.feed(bytes.as_ref(), MAX_SSE_FRAME_BYTES)
                    .map(|_| bytes)
            })))
        })
}

/// A log read the shell applies on the UI thread. `Replace` is the full tail when the incremental
/// read filled the byte cap, so appending it would still be truncated.
pub enum FetchedLog {
    Append(LogSlice),
    Replace(LogSlice),
}

/// Reads the log off the UI thread. The caller applies it with the fence it captured at spawn.
pub async fn fetch_log(
    client: &ManagerTuiClient,
    service: &str,
    cursor: crate::state::LogCursor,
) -> Result<FetchedLog, String> {
    let delta = client
        .log(service, cursor.cursor, cursor.generation)
        .await
        .map_err(|error| error.message)?;
    if cursor.cursor.is_none() || delta.data.len() < LOG_TAIL_BYTES as usize {
        return Ok(FetchedLog::Append(delta));
    }
    let latest = client
        .log(service, None, None)
        .await
        .map_err(|error| error.message)?;
    Ok(FetchedLog::Replace(latest))
}

/// Re-fetches the selected service's log tail at its current cursor, falling back to a
/// from-scratch fetch (and a `replace_log` rather than an incremental append) when the incremental
/// fetch came back truncated at exactly the tail-byte cap.
pub async fn refresh_selected_log(
    client: &ManagerTuiClient,
    state: &mut crate::state::TuiState,
    fence: crate::state::TuiFence,
) -> Result<bool, String> {
    let service = state.selection.selected_name.clone();
    if service.is_empty() {
        return Ok(false);
    }
    let cursor = state.log_cursor(&service);
    let delta = client
        .log(&service, cursor.cursor, cursor.generation)
        .await
        .map_err(|e| e.message)?;
    let data_len = delta.data.len();
    if !state.apply_log(fence, &service, &delta) {
        return Ok(false);
    }
    if cursor.cursor.is_none() || data_len < LOG_TAIL_BYTES as usize {
        return Ok(true);
    }
    let latest = client
        .log(&service, None, None)
        .await
        .map_err(|e| e.message)?;
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

    /// Feeds byte chunks through `capped_sse_bytes` + `EventStream` — the same pipeline
    /// `stream_events` runs — and returns the decoded events or the first transport error.
    async fn events_of(chunks: &[&[u8]]) -> Result<Vec<eventsource_stream::Event>, String> {
        let byte_stream = futures_util::stream::iter(
            chunks
                .iter()
                .map(|c| Ok::<_, String>(bytes::Bytes::copy_from_slice(c))),
        );
        let mut events = Vec::new();
        let mut stream = EventStream::new(capped_sse_bytes(byte_stream));
        while let Some(item) = stream.next().await {
            events.push(item.map_err(|e| match e {
                EventStreamError::Transport(message) => message,
                other => other.to_string(),
            })?);
        }
        Ok(events)
    }

    #[tokio::test]
    async fn parses_a_replay_frame() {
        let events = events_of(&[
            b"event: replay\ndata: {\"epoch\":\"e1\",\"reset\":false,\"latestSequence\":5}\n\n",
        ])
        .await
        .unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event, "replay");
        let replay: EventReplay = serde_json::from_str(&events[0].data).unwrap();
        assert_eq!(replay.epoch, "e1");
        assert_eq!(replay.latest_sequence, 5);
    }

    #[tokio::test]
    async fn parses_a_multi_line_data_field() {
        let events = events_of(&[
            b"event: service.lifecycle\ndata: {\"sequence\":1,\ndata: \"type\":\"x\"}\n\n",
        ])
        .await
        .unwrap();
        assert_eq!(events[0].event, "service.lifecycle");
        let data: serde_json::Value = serde_json::from_str(&events[0].data).unwrap();
        assert_eq!(data, serde_json::json!({"sequence": 1, "type": "x"}));
    }

    #[tokio::test]
    async fn defaults_the_event_type_to_message_without_an_event_line() {
        let events = events_of(&[b"data: {\"a\":1}\n\n"]).await.unwrap();
        assert_eq!(events[0].event, "message");
    }

    /// A frame with no `data:` field is not dispatched (SSE spec) — no event, no error.
    #[tokio::test]
    async fn a_frame_with_no_data_field_emits_nothing() {
        let events = events_of(&[b"event: replay\n\n"]).await.unwrap();
        assert!(events.is_empty());
    }

    #[tokio::test]
    async fn an_oversized_frame_fails_the_stream() {
        let huge = vec![b'x'; MAX_SSE_FRAME_BYTES + 1];
        assert!(events_of(&[&huge]).await.is_err());
    }

    /// axum's keep-alive is a bare `:` comment frame; it must be skipped, not treated as a
    /// malformed frame that tears the stream down.
    #[tokio::test]
    async fn keep_alive_comment_frames_are_skipped() {
        let events = events_of(&[
            b":\n\n: ping\n\n",
            b"event: replay\ndata: {\"epoch\":\"e\",\"reset\":false,\"latestSequence\":1}\n\n",
        ])
        .await
        .unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event, "replay");
    }

    /// A multi-byte character split across two network chunks decodes intact once the frame
    /// completes.
    #[tokio::test]
    async fn a_character_split_across_chunks_decodes_intact() {
        let frame = b"data: {\"message\":\"h\xc3\xa9llo\"}\n\n";
        let split = frame.iter().position(|b| *b == 0xC3).unwrap() + 1;
        let events = events_of(&[&frame[..split], &frame[split..]])
            .await
            .unwrap();
        let data: serde_json::Value = serde_json::from_str(&events[0].data).unwrap();
        assert_eq!(data["message"], "héllo");
    }

    /// The frame cap survives a `\n\n` boundary split across chunk edges — the byte after the
    /// boundary starts the next frame.
    #[test]
    fn frame_cap_tracks_a_boundary_split_across_chunks() {
        let mut cap = SseFrameCap::default();
        cap.feed(b"data: x\n", 7).unwrap_err();
        let mut cap = SseFrameCap::default();
        cap.feed(b"data: x\n", 16).unwrap();
        cap.feed(b"\nnext: y", 16).unwrap();
        assert_eq!(cap.pending, "next: y".len());
    }

    /// End-to-end through a real daemon: `action(..., kill_unowned: true)` is the TUI's version of
    /// the confirmed reclaim — the squatter is terminated and the start continues on the freed port.
    #[tokio::test]
    #[allow(clippy::zombie_processes)] // the squatter is killed and reaped at the end of the test
    async fn action_with_kill_unowned_reclaims_the_held_port() {
        use hearth_core::catalog::{
            CommandSpec, ReadinessSpec, ServiceCatalog, ServiceCommand, ServiceDefinition,
            ServiceKind, ServiceProfiles, ServiceRunProfile, StartFailurePolicy,
        };
        use hearth_core::manager::{bootstrap, HearthManagerOptions};
        use hearth_core::state::OperationStatus;
        use std::collections::HashMap;
        use std::os::unix::process::CommandExt;

        let dir = tempfile::tempdir().unwrap();
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let mut squatter = std::process::Command::new("nc")
            .args(["-lk", &port.to_string()])
            .process_group(0)
            .spawn()
            .unwrap();
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
            restart: None,
            disabled: false,
            profiles: ServiceProfiles {
                run: ServiceRunProfile::Verified {
                    command: ServiceCommand {
                        command: CommandSpec::Shell {
                            shell: format!("exec nc -lk {port}"),
                            exec: Some(true),
                        },
                        cwd: "/tmp".to_string(),
                        environment: None,
                        container_name: None,
                        docker_stop_command: None,
                    },
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
        let refused = client
            .action("api", ServiceOperationKind::Start, false)
            .await
            .unwrap();
        assert_eq!(
            client.wait_operation(&refused.id).await.unwrap().status,
            OperationStatus::Failed
        );
        assert!(squatter.try_wait().unwrap().is_none());

        let accepted = client
            .action("api", ServiceOperationKind::Start, true)
            .await
            .unwrap();
        assert_eq!(
            client.wait_operation(&accepted.id).await.unwrap().status,
            OperationStatus::Succeeded
        );
        for _ in 0..50 {
            if squatter.try_wait().unwrap().is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(
            squatter.try_wait().unwrap().is_some(),
            "the confirmed reclaim must terminate the squatter"
        );
        let _ = squatter.kill();
        let _ = squatter.wait();
        manager.shutdown(ShutdownMode::StopServices).await;
    }
}
