//! Port of `src/tui/tui-client.ts` — a daemon HTTP+SSE client. The TUI has no direct access to
//! processes; it is just another client of the manager, same as the CLI (whose `ls_cli::{discover,
//! request, require_client, wait_operation}` this reuses directly rather than re-implementing).
//!
//! **Deliberate deviation from the TS source**: the TS `ManagerTuiClient` takes an injectable
//! `runtime` (discover/request/sleep/now/eventStream) purely for test doubles. This port has no such
//! injection point — every test drives a real bootstrapped `LocalServicesManager` over real HTTP,
//! the same methodology `ls-core`'s and `ls-cli`'s own test suites already use, so a fake transport
//! layer would be redundant rather than a capability gap.
use std::path::PathBuf;
use std::time::Duration;

use futures_util::StreamExt;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use ls_cli::{operation_id, require_client, request as cli_request, wait_operation as cli_wait_operation, Client, LocalctlError, LocalctlOptions, EXIT_FAILED, EXIT_UNAVAILABLE};
use ls_core::state::{ManagerEvent, Operation, ServiceLifecycleState};

use crate::state::{ActionKind, LogSlice};

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
}

fn unavailable_error(message: String) -> LocalctlError {
    LocalctlError { exit_code: EXIT_UNAVAILABLE, message }
}

fn malformed_error(error: serde_json::Error) -> LocalctlError {
    LocalctlError { exit_code: EXIT_FAILED, message: error.to_string() }
}

fn safe_message(message: &str) -> String {
    message.chars().map(|c| if c == '\r' || c == '\n' || (c as u32) < 0x20 || c as u32 == 0x7f { ' ' } else { c }).collect()
}

pub struct ManagerTuiClient<'a> {
    root: PathBuf,
    options: &'a LocalctlOptions,
}

impl<'a> ManagerTuiClient<'a> {
    pub fn new(root: PathBuf, options: &'a LocalctlOptions) -> Self {
        Self { root, options }
    }

    async fn client(&self) -> Result<Client, LocalctlError> {
        require_client(&self.root, self.options).await
    }

    pub async fn snapshot(&self) -> Result<Vec<ServiceLifecycleState>, LocalctlError> {
        let client = self.client().await?;
        let body = cli_request(&client, "/v1/services", reqwest::Method::GET, None, None).await.map_err(unavailable_error)?;
        serde_json::from_value(body["services"].clone()).map_err(malformed_error)
    }

    pub async fn log(&self, service_id: &str, cursor: Option<u64>, generation: Option<u64>) -> Result<LogSlice, LocalctlError> {
        let client = self.client().await?;
        let mut query = format!("limit={LOG_TAIL_BYTES}");
        if let Some(c) = cursor {
            query.push_str(&format!("&cursor={c}"));
        }
        if let Some(g) = generation {
            query.push_str(&format!("&generation={g}"));
        }
        let path = format!("/v1/logs/{}?{query}", encode_path_segment(service_id));
        let body = cli_request(&client, &path, reqwest::Method::GET, None, None).await.map_err(unavailable_error)?;
        Ok(LogSlice {
            data: body["data"].as_str().unwrap_or_default().to_string(),
            next_cursor: body["nextCursor"].as_u64().unwrap_or(0),
            generation: body["generation"].as_u64().unwrap_or(0),
            reset: body["reset"].as_bool().unwrap_or(false),
        })
    }

    pub async fn operation(&self, id: &str) -> Result<Operation, LocalctlError> {
        let client = self.client().await?;
        let encoded = operation_id(id)?;
        let body = cli_request(&client, &format!("/v1/operations/{encoded}"), reqwest::Method::GET, None, None).await.map_err(unavailable_error)?;
        serde_json::from_value(body["operation"].clone()).map_err(malformed_error)
    }

    pub async fn action(&self, service_id: &str, action: ActionKind) -> Result<Operation, LocalctlError> {
        let client = self.client().await?;
        let action_str = match action {
            ActionKind::Start => "start",
            ActionKind::Stop => "stop",
            ActionKind::Restart => "restart",
        };
        let body = json!({ "requestId": uuid::Uuid::new_v4().to_string(), "serviceId": service_id, "action": action_str });
        let response = cli_request(&client, "/v1/operations", reqwest::Method::POST, Some(&body), None).await.map_err(unavailable_error)?;
        serde_json::from_value(response["operation"].clone()).map_err(malformed_error)
    }

    pub async fn bulk_start(&self, targets: &[String]) -> Result<Operation, LocalctlError> {
        let client = self.client().await?;
        let body = json!({ "requestId": uuid::Uuid::new_v4().to_string(), "targets": targets });
        let response = cli_request(&client, "/v1/operations/bulk-start", reqwest::Method::POST, Some(&body), None).await.map_err(unavailable_error)?;
        serde_json::from_value(response["operation"].clone()).map_err(malformed_error)
    }

    pub async fn wait_operation(&self, id: &str) -> Result<Operation, LocalctlError> {
        let client = self.client().await?;
        cli_wait_operation(&client, id).await
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
        let client = self.client().await.map_err(|e| e.message)?;
        let body = cli_request(&client, "/v1/services", reqwest::Method::GET, None, None).await?;
        let services: Vec<ServiceLifecycleState> = serde_json::from_value(body["services"].clone()).map_err(|_| "manager returned malformed service state".to_string())?;
        let _ = tx.send(WatchEvent::Snapshot(services)).await;
        self.stream_events(&client, after, epoch, tx, cancel).await
    }

    async fn stream_events(&self, client: &Client, after: &mut Option<u64>, epoch: &mut Option<String>, tx: &mpsc::Sender<WatchEvent>, cancel: &CancellationToken) -> Result<(), String> {
        let mut query: Vec<(&str, String)> = Vec::new();
        if let Some(a) = after {
            query.push(("after", a.to_string()));
        }
        if let Some(e) = epoch.as_ref() {
            query.push(("epoch", e.clone()));
        }
        let url = format!("http://127.0.0.1:{}/v1/events/stream", client.metadata.port);
        let response = reqwest::Client::new()
            .get(&url)
            .query(&query)
            .bearer_auth(&client.token)
            .header("x-local-services-protocol", ls_core::state::PROTOCOL_VERSION.to_string())
            .header("accept", "text/event-stream")
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if !response.status().is_success() {
            return Err(format!("event stream failed: {}", response.status()));
        }
        let mut stream = response.bytes_stream();
        let mut buffer = String::new();
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
            buffer = append_sse_chunk(&buffer, &String::from_utf8_lossy(&bytes))?;
            while let Some(boundary) = buffer.find("\n\n") {
                let frame = buffer[..boundary].to_string();
                buffer = buffer[boundary + 2..].to_string();
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

fn encode_path_segment(segment: &str) -> String {
    percent_encoding::utf8_percent_encode(segment, percent_encoding::NON_ALPHANUMERIC).to_string()
}

pub fn append_sse_chunk(buffer: &str, chunk: &str) -> Result<String, String> {
    let next = format!("{buffer}{chunk}");
    if next.len() > MAX_SSE_FRAME_BYTES {
        return Err("event stream frame exceeds limit".to_string());
    }
    Ok(next)
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

/// Port of `refreshSelectedLog` — re-fetches the selected service's log tail at its current cursor,
/// falling back to a from-scratch fetch (and a `replace_log` rather than an incremental append) when
/// the incremental fetch came back truncated at exactly the tail-byte cap.
pub async fn refresh_selected_log(client: &ManagerTuiClient<'_>, state: &mut crate::state::TuiState, fence: crate::state::TuiFence) -> Result<bool, String> {
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
        let huge = "x".repeat(MAX_SSE_FRAME_BYTES + 1);
        assert!(append_sse_chunk("", &huge).is_err());
    }
}
