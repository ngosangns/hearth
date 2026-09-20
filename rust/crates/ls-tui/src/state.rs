//! Port of `src/tui/state.ts` — pure TUI state (no terminal, no network), driven entirely through
//! the connection/request/selection fence counters so a stale async response can never overwrite
//! state a newer request/selection has already superseded.
use std::collections::HashMap;
use std::sync::Arc;

use ls_core::catalog::ServiceKind;
use ls_core::state::{ActualServiceState, Operation, OperationStatus, ServiceLifecycleState};

/// Looks up a service's `kind` for display — pass the consumer's `ServiceCatalog` lookup, or a
/// closure returning `None` for no kind badges.
pub type ServiceKindLookup = Arc<dyn Fn(&str) -> Option<ServiceKind> + Send + Sync>;

pub fn no_service_kind() -> ServiceKindLookup {
    Arc::new(|_| None)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Service {
    pub name: String,
    pub kind: Option<ServiceKind>,
    pub state: String,
    pub generation: Option<u64>,
    pub current_operation_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TuiFence {
    pub connection: u64,
    pub request: u64,
    pub selection: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionKind {
    Start,
    Stop,
    Restart,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActionFence {
    pub fence: TuiFence,
    pub service: String,
    pub action: ActionKind,
}

fn actual_state_str(state: ActualServiceState) -> &'static str {
    match state {
        ActualServiceState::Stopped => "stopped",
        ActualServiceState::QueuedStart => "queued-start",
        ActualServiceState::Preparing => "preparing",
        ActualServiceState::Starting => "starting",
        ActualServiceState::Running => "running",
        ActualServiceState::RunningUnready => "running-unready",
        ActualServiceState::Ready => "ready",
        ActualServiceState::Stopping => "stopping",
        ActualServiceState::Failed => "failed",
        ActualServiceState::Orphaned => "orphaned",
        ActualServiceState::ExternallyOwned => "externally-owned",
    }
}

pub fn service_from_lifecycle(service: &ServiceLifecycleState, service_kind: &ServiceKindLookup) -> Service {
    let state = if service.actual_state == ActualServiceState::RunningUnready { "degraded".to_string() } else { actual_state_str(service.actual_state).to_string() };
    Service { name: service.service_id.clone(), kind: service_kind(&service.service_id), state, generation: Some(service.generation), current_operation_id: service.current_operation_id.clone() }
}

pub const DEFAULT_LOG_TAIL_LIMIT: usize = 16 * 1024;

/// Keeps only the last `limit` *characters* (Unicode scalar values) of `current` + `next` — the TS
/// source measures UTF-16 code units instead, so this diverges only for text containing characters
/// outside the Basic Multilingual Plane, which real process log output essentially never contains.
pub fn bounded_tail(current: &str, next: &str, limit: usize) -> String {
    let combined = format!("{current}{next}");
    let char_count = combined.chars().count();
    if char_count <= limit {
        combined
    } else {
        combined.chars().skip(char_count - limit).collect()
    }
}

#[derive(Debug, Clone, Default)]
pub struct ServiceSelection {
    pub services: Vec<Service>,
    pub selected_name: String,
}

impl ServiceSelection {
    pub fn set_services(&mut self, services: Vec<Service>) {
        self.services = services;
        if !self.services.iter().any(|s| s.name == self.selected_name) {
            self.selected_name = self.services.first().map(|s| s.name.clone()).unwrap_or_default();
        }
    }

    /// Moves the selection by `delta` positions; a no-op (returns `false`) if the result would
    /// land outside the current service list.
    pub fn move_by(&mut self, delta: i64) -> bool {
        let Some(index) = self.services.iter().position(|s| s.name == self.selected_name) else { return false };
        let next = index as i64 + delta;
        if next < 0 || next >= self.services.len() as i64 {
            return false;
        }
        self.selected_name = self.services[next as usize].name.clone();
        true
    }

    pub fn select(&mut self, name: &str) -> bool {
        if name == self.selected_name || !self.services.iter().any(|s| s.name == name) {
            return false;
        }
        self.selected_name = name.to_string();
        true
    }

    pub fn selected(&self) -> Option<&Service> {
        self.services.iter().find(|s| s.name == self.selected_name)
    }
}

#[derive(Debug, Clone, Default)]
struct LogStream {
    data: String,
    cursor: Option<u64>,
    generation: Option<u64>,
}

#[derive(Debug, Clone, Default)]
pub struct LogCursor {
    pub cursor: Option<u64>,
    pub generation: Option<u64>,
}

pub struct LogSlice {
    pub data: String,
    pub next_cursor: u64,
    pub generation: u64,
    pub reset: bool,
}

/// Pure TUI state with connection, response, selection, and action fences.
pub struct TuiState {
    pub selection: ServiceSelection,
    pub log: String,
    pub operation: Option<Operation>,
    pub notice: String,
    logs: HashMap<String, LogStream>,
    service_kind: ServiceKindLookup,
    connection: u64,
    request: u64,
    selection_generation: u64,
}

impl TuiState {
    pub fn new(service_kind: ServiceKindLookup) -> Self {
        Self { selection: ServiceSelection::default(), log: "Loading…".to_string(), operation: None, notice: String::new(), logs: HashMap::new(), service_kind, connection: 0, request: 0, selection_generation: 0 }
    }

    pub fn begin_connection(&mut self) -> TuiFence {
        self.connection += 1;
        self.request += 1;
        self.fence()
    }

    pub fn begin_request(&mut self) -> TuiFence {
        self.request += 1;
        self.fence()
    }

    pub fn begin_selection(&mut self) -> TuiFence {
        self.selection_generation += 1;
        self.request += 1;
        self.sync_selected_log();
        self.operation = None;
        self.fence()
    }

    pub fn begin_action(&mut self, service: &str, action: ActionKind) -> ActionFence {
        ActionFence { fence: self.begin_request(), service: service.to_string(), action }
    }

    fn fence(&self) -> TuiFence {
        TuiFence { connection: self.connection, request: self.request, selection: self.selection_generation }
    }

    pub fn current(&self, fence: TuiFence) -> bool {
        self.connected(fence) && fence.request == self.request && fence.selection == self.selection_generation
    }

    pub fn connected(&self, fence: TuiFence) -> bool {
        fence.connection == self.connection
    }

    pub fn current_action(&self, fence: &ActionFence) -> bool {
        self.current(fence.fence) && self.selection.selected_name == fence.service
    }

    pub fn apply_snapshot(&mut self, fence: TuiFence, services: &[ServiceLifecycleState]) -> bool {
        if !self.current(fence) {
            return false;
        }
        let selected = self.selection.selected_name.clone();
        self.selection.set_services(services.iter().map(|s| service_from_lifecycle(s, &self.service_kind)).collect());
        if selected != self.selection.selected_name {
            self.sync_selected_log();
        }
        true
    }

    pub fn log_cursor(&self, service: &str) -> LogCursor {
        match self.logs.get(service) {
            Some(stream) => LogCursor { cursor: stream.cursor, generation: stream.generation },
            None => LogCursor::default(),
        }
    }

    pub fn apply_log(&mut self, fence: TuiFence, service: &str, slice: &LogSlice) -> bool {
        if !self.current(fence) || service != self.selection.selected_name {
            return false;
        }
        let current = self.logs.get(service);
        let replace = slice.reset || current.and_then(|c| c.generation) != Some(slice.generation) || current.map(|c| c.cursor.is_none()).unwrap_or(true);
        let data = if replace { bounded_tail("", &slice.data, DEFAULT_LOG_TAIL_LIMIT) } else { bounded_tail(&current.unwrap().data, &slice.data, DEFAULT_LOG_TAIL_LIMIT) };
        let stream = LogStream { data, cursor: Some(slice.next_cursor), generation: Some(slice.generation) };
        self.log = if stream.data.is_empty() { "No log yet.".to_string() } else { stream.data.clone() };
        self.logs.insert(service.to_string(), stream);
        true
    }

    pub fn replace_log(&mut self, fence: TuiFence, service: &str, slice: &LogSlice) -> bool {
        if !self.current(fence) || service != self.selection.selected_name {
            return false;
        }
        let data = bounded_tail("", &slice.data, DEFAULT_LOG_TAIL_LIMIT);
        self.log = if data.is_empty() { "No log yet.".to_string() } else { data.clone() };
        self.logs.insert(service.to_string(), LogStream { data, cursor: Some(slice.next_cursor), generation: Some(slice.generation) });
        true
    }

    pub fn apply_operation(&mut self, fence: TuiFence, operation: Operation) -> bool {
        if !self.current(fence) || operation.service_id.as_deref() != Some(self.selection.selected_name.as_str()) {
            return false;
        }
        self.operation = Some(operation);
        true
    }

    pub fn apply_action(&mut self, fence: &ActionFence, operation: Operation) -> bool {
        let expected_action = match fence.action {
            ActionKind::Start => ls_core::state::ServiceOperationKind::Start,
            ActionKind::Stop => ls_core::state::ServiceOperationKind::Stop,
            ActionKind::Restart => ls_core::state::ServiceOperationKind::Restart,
        };
        if !self.current_action(fence) || operation.service_id.as_deref() != Some(fence.service.as_str()) || operation.action != Some(expected_action) {
            return false;
        }
        self.operation = Some(operation);
        if fence.action == ActionKind::Start {
            self.clear_log(&fence.service);
        }
        self.notice = format!("{} {}: {}", action_str(fence.action), fence.service, self.operation.as_ref().unwrap().id);
        true
    }

    pub fn apply_action_failure(&mut self, fence: &ActionFence, error: &str) -> bool {
        if !self.current_action(fence) {
            return false;
        }
        self.notice = format!("{} {} failed: {error}", action_str(fence.action), fence.service);
        true
    }

    pub fn apply_event(&mut self, fence: TuiFence, event_type: &str, data: &serde_json::Map<String, serde_json::Value>) -> bool {
        if !self.connected(fence) {
            return false;
        }
        let service_id = data.get("serviceId").and_then(|v| v.as_str());
        if let Some(service_id) = service_id {
            if event_type == "service.lifecycle" {
                if let Some(service) = self.selection.services.iter_mut().find(|s| s.name == service_id) {
                    if let Some(actual_state) = data.get("actualState").and_then(|v| v.as_str()) {
                        service.state = if actual_state == "running-unready" { "degraded".to_string() } else { actual_state.to_string() };
                    }
                    if let Some(generation) = data.get("generation").and_then(|v| v.as_u64()) {
                        service.generation = Some(generation);
                    }
                    service.current_operation_id = data.get("operationId").and_then(|v| v.as_str()).map(str::to_string);
                }
            }
        }
        true
    }

    pub fn detail(&self) -> String {
        let Some(service) = self.selection.selected() else { return "No services.".to_string() };
        let mut parts = vec![format!("state={}", service.state), format!("generation={}", service.generation.unwrap_or(0))];
        if let Some(operation) = &self.operation {
            if operation.service_id.as_deref() == Some(service.name.as_str()) {
                parts.push(format!("operation={} {}", operation.id, operation_status_str(operation.status)));
                parts.extend(operation.trace.iter().map(|entry| format!("{} {}", entry.at, entry.message)));
                if let Some(error) = &operation.error {
                    parts.push(format!("operation error={}", error.message));
                }
            }
        }
        parts.join("\n")
    }

    fn clear_log(&mut self, service: &str) {
        let previous = self.logs.get(service);
        let (cursor, generation) = (previous.and_then(|p| p.cursor), previous.and_then(|p| p.generation));
        self.logs.insert(service.to_string(), LogStream { data: String::new(), cursor, generation });
        if service == self.selection.selected_name {
            self.log = "No log yet.".to_string();
        }
    }

    fn sync_selected_log(&mut self) {
        self.log = self.logs.get(&self.selection.selected_name).map(|s| s.data.clone()).filter(|d| !d.is_empty()).unwrap_or_else(|| "Loading…".to_string());
    }
}

fn action_str(action: ActionKind) -> &'static str {
    match action {
        ActionKind::Start => "start",
        ActionKind::Stop => "stop",
        ActionKind::Restart => "restart",
    }
}

fn operation_status_str(status: OperationStatus) -> &'static str {
    match status {
        OperationStatus::Queued => "queued",
        OperationStatus::Running => "running",
        OperationStatus::Succeeded => "succeeded",
        OperationStatus::Failed => "failed",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ls_core::state::{DesiredServiceState, ServiceReadiness};

    fn service() -> ServiceLifecycleState {
        ServiceLifecycleState {
            service_id: "metadata".to_string(),
            desired_state: DesiredServiceState::Running,
            actual_state: ActualServiceState::Ready,
            readiness: ServiceReadiness::Ready,
            generation: 1,
            identity: None,
            readiness_kind: None,
            readiness_detail: None,
            created_at: "2026-09-08T00:00:00.000Z".to_string(),
            updated_at: "2026-09-08T00:00:00.000Z".to_string(),
            exited_at: None,
            exit_code: None,
            error: None,
            current_operation_id: None,
        }
    }

    fn operation() -> Operation {
        Operation {
            id: "start-1".to_string(),
            request_id: "request-1".to_string(),
            kind: ls_core::state::OperationKind::Service,
            service_id: Some("metadata".to_string()),
            target_service_ids: None,
            action: Some(ls_core::state::ServiceOperationKind::Start),
            status: OperationStatus::Queued,
            created_at: "2026-09-08T00:00:00.000Z".to_string(),
            updated_at: "2026-09-08T00:00:00.000Z".to_string(),
            trace: vec![],
            error: None,
        }
    }

    #[test]
    fn shows_all_profile_free_services_and_lifecycle_state() {
        let mut state = TuiState::new(no_service_kind());
        let fence = state.begin_connection();
        assert!(state.apply_snapshot(fence, &[service()]));
        assert_eq!(state.selection.selected_name, "metadata");
        assert!(state.detail().contains("generation=1"));
        assert!(!state.detail().contains("reload"));
    }

    #[test]
    fn keeps_queued_start_lifecycle_snapshots_distinct_from_stopped_services() {
        let mut state = TuiState::new(no_service_kind());
        let fence = state.begin_connection();
        let mut s = service();
        s.actual_state = ActualServiceState::QueuedStart;
        s.readiness = ServiceReadiness::Unknown;
        s.generation = 0;
        assert!(state.apply_snapshot(fence, &[s]));
        let selected = state.selection.selected().unwrap();
        assert_eq!(selected.state, "queued-start");
        assert_eq!(selected.generation, Some(0));
        assert!(state.detail().contains("state=queued-start"));
    }

    #[test]
    fn clears_retained_logs_when_the_selected_service_starts() {
        let mut state = TuiState::new(no_service_kind());
        let fence = state.begin_connection();
        state.apply_snapshot(fence, &[service()]);
        state.apply_log(fence, "metadata", &LogSlice { data: "old output\n".to_string(), generation: 1, next_cursor: 11, reset: false });
        let action = state.begin_action("metadata", ActionKind::Start);
        assert!(state.apply_action(&action, operation()));
        assert_eq!(state.log, "No log yet.");
        let cursor = state.log_cursor("metadata");
        assert_eq!(cursor.cursor, Some(11));
        assert_eq!(cursor.generation, Some(1));
    }

    #[test]
    fn selects_only_a_known_service() {
        let mut state = TuiState::new(no_service_kind());
        let fence = state.begin_connection();
        let mut second = service();
        second.service_id = "question".to_string();
        state.apply_snapshot(fence, &[service(), second]);

        assert!(state.selection.select("question"));
        assert_eq!(state.selection.selected_name, "question");
        assert!(!state.selection.select("missing"));
        assert_eq!(state.selection.selected_name, "question");
    }

    #[test]
    fn looks_up_service_kind_through_the_injected_lookup() {
        let lookup: ServiceKindLookup = Arc::new(|id: &str| if id == "mongo" { Some(ServiceKind::Infrastructure) } else { Some(ServiceKind::Application) });
        let mut state = TuiState::new(lookup);
        let fence = state.begin_connection();
        let mut s = service();
        s.service_id = "mongo".to_string();
        state.apply_snapshot(fence, &[s]);
        assert_eq!(state.selection.selected().unwrap().kind, Some(ServiceKind::Infrastructure));
    }
}
