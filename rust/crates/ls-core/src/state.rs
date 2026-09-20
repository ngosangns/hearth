//! Wire/persistence types — port of `src/core/state.ts`.
//!
//! `PROTOCOL_VERSION` is bumped on any breaking change to the daemon/TUI/MCP wire protocol or
//! persisted-state shape; a daemon and a client built against different `PROTOCOL_VERSION`s must
//! refuse to talk to each other (`x-local-services-protocol` header check in `manager.rs`) rather
//! than silently misbehaving.
use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::catalog::ServiceId;

pub const PROTOCOL_VERSION: u32 = 1;
pub const STATE_VERSION: u32 = 1;

pub const STALE_LOCK_MARKER_NAME: &str = "quarantine.json";
pub const LOCK_RELEASE_MARKER_NAME: &str = "releasing.json";
pub const OWNERSHIP_KEY_NAME: &str = "ownership.key";
pub const LOCK_OWNERSHIP_PROOF_NAME: &str = "ownership.json";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DesiredServiceState {
    Stopped,
    Running,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ActualServiceState {
    Stopped,
    QueuedStart,
    Preparing,
    Starting,
    Running,
    RunningUnready,
    Ready,
    Stopping,
    Failed,
    Orphaned,
    ExternallyOwned,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ServiceReadiness {
    Unknown,
    NotReady,
    Ready,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ServiceOperationKind {
    Start,
    Stop,
    Restart,
    Status,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OperationStatus {
    Queued,
    Running,
    Succeeded,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReadinessKind {
    Process,
    Tcp,
    Http,
    Container,
    Tailnet,
    Command,
    Custom,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PosixProcessIdentity {
    pub manager_instance_id: String,
    pub service_id: ServiceId,
    pub generation: u64,
    pub pid: i64,
    pub pgid: i64,
    pub started_at: String,
    /// `ps`'s `lstart` field — the pid-reuse guard everywhere in the supervisor compares against
    /// this exact 24-char string, not a parsed timestamp.
    pub start_identity: String,
    pub command_fingerprint: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DockerContainerIdentity {
    pub manager_instance_id: String,
    pub service_id: ServiceId,
    pub generation: u64,
    pub started_at: String,
    pub command_fingerprint: String,
    pub container_name: String,
    pub container_id: String,
    pub container_started_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ProcessIdentity {
    Posix(PosixProcessIdentity),
    Docker(DockerContainerIdentity),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ServiceLifecycleState {
    pub service_id: ServiceId,
    pub desired_state: DesiredServiceState,
    pub actual_state: ActualServiceState,
    pub readiness: ServiceReadiness,
    pub generation: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub identity: Option<ProcessIdentity>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub readiness_kind: Option<ReadinessKind>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub readiness_detail: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exited_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_operation_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperationTraceEntry {
    pub at: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperationError {
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum OperationKind {
    Service,
    BulkStart,
    ManagerShutdown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Operation {
    pub id: String,
    pub request_id: String,
    pub kind: OperationKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service_id: Option<ServiceId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_service_ids: Option<Vec<ServiceId>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub action: Option<ServiceOperationKind>,
    pub status: OperationStatus,
    pub created_at: String,
    pub updated_at: String,
    pub trace: Vec<OperationTraceEntry>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<OperationError>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManagerEvent {
    pub sequence: u64,
    pub at: String,
    #[serde(rename = "type")]
    pub event_type: String,
    pub data: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManagerMetadata {
    pub version: u32,
    pub protocol_version: u32,
    pub instance_id: String,
    pub pid: i64,
    pub port: u16,
    pub started_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManagerInfo {
    pub protocol_version: u32,
    pub instance_id: String,
    pub pid: i64,
    pub port: u16,
    pub started_at: String,
    pub metadata_version: u32,
    pub runtime_directory: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedManagerState {
    pub version: u32,
    pub services: HashMap<ServiceId, ServiceLifecycleState>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogSlice {
    pub service_id: ServiceId,
    pub generation: u64,
    pub cursor: u64,
    pub next_cursor: u64,
    pub data: String,
    pub reset: bool,
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LockOwnershipProof {
    pub version: u32,
    pub metadata: ManagerMetadata,
    pub token_digest: String,
    pub signature: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum StaleLockAction {
    StaleLock,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StaleLockMarker {
    pub version: u32,
    pub action: StaleLockAction,
    pub original: ManagerMetadata,
    pub proof: LockOwnershipProof,
}
