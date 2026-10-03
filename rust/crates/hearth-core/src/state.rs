//! Wire/persistence types — port of `src/core/state.ts`.
//!
//! `PROTOCOL_VERSION` is bumped on any breaking change to the daemon/TUI/MCP wire protocol or
//! persisted-state shape; a daemon and a client built against different `PROTOCOL_VERSION`s must
//! refuse to talk to each other (`x-hearth-protocol` header check in `manager/http.rs`) rather
//! than silently misbehaving.
use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::catalog::ServiceId;

pub const PROTOCOL_VERSION: u32 = 2;
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
    /// In flight for `readiness: exit`. A one-time command has no ready state: it stays `running`
    /// until the process exits (`succeeded` or `failed`). `kind: process` does not use this variant;
    /// it settles in `RunningUnready`.
    Running,
    RunningUnready,
    Ready,
    /// A `readiness: exit` command finished with exit code 0. Desired is `stopped`, so a later
    /// reconcile does not run it again. Not an active state.
    Succeeded,
    Stopping,
    Failed,
    Orphaned,
    ExternallyOwned,
}

impl ActualServiceState {
    /// The kebab-case wire encoding — identical to this enum's serde representation, and the only
    /// form any client understands.
    ///
    /// Never reach for `format!("{:?}", state)` to build a client-facing string: `Debug` yields
    /// `QueuedStart`/`ExternallyOwned`, so lowercasing it gives `queuedstart`, which matches
    /// neither the serde encoding the same daemon emits elsewhere nor the `queued-start` every
    /// client matches on. Both mistakes were live bugs (a shutdown guard that never saw a
    /// non-terminal service, and `service.lifecycle` events whose `actualState` disagreed with the
    /// manager's own events for the same transition). `wire_encoding_matches_serde` below pins the
    /// two encodings together.
    pub fn as_wire_str(self) -> &'static str {
        match self {
            Self::Stopped => "stopped",
            Self::QueuedStart => "queued-start",
            Self::Preparing => "preparing",
            Self::Starting => "starting",
            Self::Running => "running",
            Self::RunningUnready => "running-unready",
            Self::Ready => "ready",
            Self::Succeeded => "succeeded",
            Self::Stopping => "stopping",
            Self::Failed => "failed",
            Self::Orphaned => "orphaned",
            Self::ExternallyOwned => "externally-owned",
        }
    }

    /// Every variant, so exhaustive tests cannot silently miss one added later.
    pub const ALL: [Self; 12] = [
        Self::Stopped,
        Self::QueuedStart,
        Self::Preparing,
        Self::Starting,
        Self::Running,
        Self::RunningUnready,
        Self::Ready,
        Self::Succeeded,
        Self::Stopping,
        Self::Failed,
        Self::Orphaned,
        Self::ExternallyOwned,
    ];
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ServiceReadiness {
    Unknown,
    NotReady,
    Ready,
    Failed,
}

impl ServiceReadiness {
    /// The kebab-case wire encoding, matching this enum's serde representation.
    pub fn as_wire_str(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::NotReady => "not-ready",
            Self::Ready => "ready",
            Self::Failed => "failed",
        }
    }

    pub const ALL: [Self; 4] = [Self::Unknown, Self::NotReady, Self::Ready, Self::Failed];
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ServiceOperationKind {
    Start,
    Stop,
    Restart,
    Status,
}

impl ServiceOperationKind {
    /// The kebab-case wire encoding, matching this enum's serde representation — same rule as
    /// `ActualServiceState::as_wire_str`: never build a client-facing string from `Debug`.
    pub fn as_wire_str(self) -> &'static str {
        match self {
            Self::Start => "start",
            Self::Stop => "stop",
            Self::Restart => "restart",
            Self::Status => "status",
        }
    }

    pub const ALL: [Self; 4] = [Self::Start, Self::Stop, Self::Restart, Self::Status];
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OperationStatus {
    Queued,
    Running,
    Succeeded,
    Failed,
}

impl OperationStatus {
    /// The lowercase wire encoding, matching this enum's serde representation.
    pub fn as_wire_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
        }
    }

    pub const ALL: [Self; 4] = [Self::Queued, Self::Running, Self::Succeeded, Self::Failed];
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
    /// `readiness: { kind: exit }` — the run command itself is the job.
    Exit,
    /// Reserved: never produced. There is no custom readiness probe (a closure can't cross the
    /// YAML/JSON boundary — `command` is the stand-in); the variant stays only so a persisted or
    /// wire value that carries it still decodes. Removing it is a `PROTOCOL_VERSION` bump.
    #[doc = "reserved: never produced"]
    Custom,
}

impl ReadinessKind {
    /// The lowercase wire encoding, matching this enum's serde representation. Same rule as
    /// `ActualServiceState::as_wire_str`: `Debug` would give `Tcp`, which leaks into
    /// `readinessDetail` — a string persisted in `state.json` and rendered by the TUI.
    pub fn as_wire_str(self) -> &'static str {
        match self {
            Self::Process => "process",
            Self::Tcp => "tcp",
            Self::Http => "http",
            Self::Container => "container",
            Self::Tailnet => "tailnet",
            Self::Command => "command",
            Self::Exit => "exit",
            Self::Custom => "custom",
        }
    }

    pub const ALL: [Self; 8] = [
        Self::Process,
        Self::Tcp,
        Self::Http,
        Self::Container,
        Self::Tailnet,
        Self::Command,
        Self::Exit,
        Self::Custom,
    ];
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
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
#[serde(rename_all = "camelCase")]
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

impl ProcessIdentity {
    pub fn manager_instance_id(&self) -> &str {
        match self {
            Self::Posix(p) => &p.manager_instance_id,
            Self::Docker(d) => &d.manager_instance_id,
        }
    }

    pub fn service_id(&self) -> &ServiceId {
        match self {
            Self::Posix(p) => &p.service_id,
            Self::Docker(d) => &d.service_id,
        }
    }

    pub fn generation(&self) -> u64 {
        match self {
            Self::Posix(p) => p.generation,
            Self::Docker(d) => d.generation,
        }
    }

    pub fn command_fingerprint(&self) -> &str {
        match self {
            Self::Posix(p) => &p.command_fingerprint,
            Self::Docker(d) => &d.command_fingerprint,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum OperationKind {
    Service,
    BulkStart,
    ManagerShutdown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
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
#[serde(rename_all = "camelCase")]
pub struct ManagerMetadata {
    pub version: u32,
    pub protocol_version: u32,
    pub instance_id: String,
    pub pid: i64,
    pub port: u16,
    pub started_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
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
#[serde(rename_all = "camelCase")]
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
#[serde(rename_all = "camelCase")]
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

#[cfg(test)]
mod tests {
    use super::*;

    // Wire-format regression: every type sent over HTTP (and every macOS Swift `Codable` struct
    // decoding it) expects camelCase JSON field names, matching what `JSON.stringify` naturally
    // produces from the TS source's camelCase object literals — Rust's derive(Serialize) defaults
    // to the field's own (snake_case) spelling unless told otherwise. This is the single most
    // safety-critical serialization detail in the whole rewrite: get it wrong and every downstream
    // consumer's strict decode breaks silently until someone notices a parse failure.
    #[test]
    fn service_lifecycle_state_serializes_camel_case() {
        let state = ServiceLifecycleState {
            service_id: "api".to_string(),
            desired_state: DesiredServiceState::Running,
            actual_state: ActualServiceState::Ready,
            readiness: ServiceReadiness::Ready,
            generation: 1,
            identity: None,
            readiness_kind: Some(ReadinessKind::Tcp),
            readiness_detail: None,
            created_at: "2024-01-01T00:00:00.000Z".to_string(),
            updated_at: "2024-01-01T00:00:00.000Z".to_string(),
            exited_at: None,
            exit_code: None,
            error: None,
            current_operation_id: None,
        };
        let json = serde_json::to_value(&state).unwrap();
        let obj = json.as_object().unwrap();
        assert!(obj.contains_key("serviceId"), "{obj:?}");
        assert!(obj.contains_key("desiredState"), "{obj:?}");
        assert!(obj.contains_key("actualState"), "{obj:?}");
        assert!(obj.contains_key("readinessKind"), "{obj:?}");
        assert!(obj.contains_key("createdAt"), "{obj:?}");
        assert!(obj.contains_key("updatedAt"), "{obj:?}");
        assert!(!obj.contains_key("service_id"), "{obj:?}");
    }

    #[test]
    fn manager_metadata_and_info_serialize_camel_case() {
        let metadata = ManagerMetadata {
            version: 1,
            protocol_version: 1,
            instance_id: "x".to_string(),
            pid: 1,
            port: 8080,
            started_at: "2024-01-01T00:00:00.000Z".to_string(),
        };
        let json = serde_json::to_value(&metadata).unwrap();
        assert!(json.get("protocolVersion").is_some(), "{json:?}");
        assert!(json.get("instanceId").is_some(), "{json:?}");
        assert!(json.get("startedAt").is_some(), "{json:?}");

        let info = ManagerInfo {
            protocol_version: 1,
            instance_id: "x".to_string(),
            pid: 1,
            port: 8080,
            started_at: "2024-01-01T00:00:00.000Z".to_string(),
            metadata_version: 1,
            runtime_directory: "/tmp".to_string(),
        };
        let json = serde_json::to_value(&info).unwrap();
        assert!(json.get("runtimeDirectory").is_some(), "{json:?}");
        assert!(json.get("metadataVersion").is_some(), "{json:?}");
    }

    #[test]
    fn log_slice_serializes_camel_case() {
        let slice = LogSlice {
            service_id: "api".to_string(),
            generation: 1,
            cursor: 0,
            next_cursor: 10,
            data: "hi".to_string(),
            reset: false,
            truncated: false,
        };
        let json = serde_json::to_value(&slice).unwrap();
        assert!(json.get("nextCursor").is_some(), "{json:?}");
        assert!(json.get("serviceId").is_some(), "{json:?}");
    }

    #[test]
    fn process_identity_serializes_camel_case() {
        let identity = ProcessIdentity::Posix(PosixProcessIdentity {
            manager_instance_id: "instance".to_string(),
            service_id: "api".to_string(),
            generation: 1,
            pid: 100,
            pgid: 100,
            started_at: "2024-01-01T00:00:00.000Z".to_string(),
            start_identity: "Mon Jan  1 00:00:00 2024".to_string(),
            command_fingerprint: "abc".to_string(),
        });
        let json = serde_json::to_value(&identity).unwrap();
        assert!(json.get("managerInstanceId").is_some(), "{json:?}");
        assert!(json.get("startIdentity").is_some(), "{json:?}");
        assert!(json.get("commandFingerprint").is_some(), "{json:?}");
    }

    #[test]
    fn wire_encoding_matches_serde_for_every_actual_state() {
        for state in ActualServiceState::ALL {
            let serde_form = serde_json::to_value(state).expect("serializable");
            assert_eq!(
                serde_form.as_str().expect("a string"),
                state.as_wire_str(),
                "as_wire_str drifted from the serde encoding for {state:?}"
            );
        }
    }

    #[test]
    fn wire_encoding_matches_serde_for_every_readiness_kind() {
        for kind in ReadinessKind::ALL {
            let serde_form = serde_json::to_value(kind).expect("serializable");
            assert_eq!(
                serde_form.as_str().expect("a string"),
                kind.as_wire_str(),
                "as_wire_str drifted from serde for {kind:?}"
            );
        }
    }

    #[test]
    fn wire_encoding_matches_serde_for_every_service_readiness() {
        for readiness in ServiceReadiness::ALL {
            let serde_form = serde_json::to_value(readiness).expect("serializable");
            assert_eq!(
                serde_form.as_str().expect("a string"),
                readiness.as_wire_str(),
                "as_wire_str drifted from serde for {readiness:?}"
            );
        }
    }

    #[test]
    fn wire_encoding_matches_serde_for_every_service_operation_kind() {
        for kind in ServiceOperationKind::ALL {
            let serde_form = serde_json::to_value(kind).expect("serializable");
            assert_eq!(
                serde_form.as_str().expect("a string"),
                kind.as_wire_str(),
                "as_wire_str drifted from serde for {kind:?}"
            );
        }
    }

    #[test]
    fn wire_encoding_matches_serde_for_every_operation_status() {
        for status in OperationStatus::ALL {
            let serde_form = serde_json::to_value(status).expect("serializable");
            assert_eq!(
                serde_form.as_str().expect("a string"),
                status.as_wire_str(),
                "as_wire_str drifted from serde for {status:?}"
            );
        }
    }

    #[test]
    fn debug_formatting_is_not_a_usable_wire_encoding() {
        // Pins the reason `as_wire_str` exists: the `format!("{:?}", ..).to_lowercase()` shortcut
        // silently produces a string no client matches, for exactly the multi-word variants.
        assert_eq!(
            ActualServiceState::QueuedStart.as_wire_str(),
            "queued-start"
        );
        assert_eq!(
            format!("{:?}", ActualServiceState::QueuedStart).to_lowercase(),
            "queuedstart"
        );
    }
}
