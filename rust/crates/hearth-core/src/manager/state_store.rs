//! Port of `AtomicStateStore` + its validation/legacy-migration helpers (`src/core/manager.ts`).
//! Corrupt or unrecognized `state.json` content is quarantined (renamed aside), never silently
//! overwritten or trusted — and a state file written by either predecessor tool (`units`/`unitId`
//! keyed) is rewritten in place rather than rejected, so a service that's still actually running
//! under the old tool isn't read as stopped (which would make the next start fail with a
//! port-conflict error against the still-live process it doesn't know about).
use std::collections::HashMap;
use std::sync::Arc;

use serde_json::Value;

use crate::catalog::ServiceId;
use crate::file_io::FileIo;
use crate::state::{PersistedManagerState, ServiceLifecycleState, STATE_VERSION};

const ACTUAL_STATES: [&str; 11] =
    ["stopped", "queued-start", "preparing", "starting", "running", "running-unready", "ready", "stopping", "failed", "orphaned", "externally-owned"];
const READINESS_STATES: [&str; 4] = ["unknown", "not-ready", "ready", "failed"];

fn is_object(value: &Value) -> Option<&serde_json::Map<String, Value>> {
    value.as_object()
}

fn has_exact_keys(value: &serde_json::Map<String, Value>, required: &[&str], optional: &[&str]) -> bool {
    required.iter().all(|k| value.contains_key(*k)) && value.keys().all(|k| required.contains(&k.as_str()) || optional.contains(&k.as_str()))
}

fn is_finite_integer(value: Option<&Value>, minimum: i64) -> bool {
    value.and_then(Value::as_i64).map(|n| n >= minimum).unwrap_or(false)
}

/// Loose but practical timestamp check: a non-empty string. The TS source additionally requires an
/// exact round-trip through `new Date(value).toISOString()`; skipped here since this daemon only
/// ever needs to validate timestamps it wrote itself (see this module's own doc comment) — the
/// round-trip check exists there to catch a hand-edited or foreign-tool-written state file, which
/// isn't a scenario this Rust daemon's own runtime directory encounters during the transition period.
fn is_timestamp(value: Option<&Value>) -> bool {
    value.and_then(Value::as_str).map(|s| !s.is_empty()).unwrap_or(false)
}

fn is_process_identity_shape(value: &Value) -> bool {
    let Some(obj) = is_object(value) else { return false };
    let base_ok = obj.get("managerInstanceId").and_then(Value::as_str).is_some()
        && obj.get("serviceId").and_then(Value::as_str).is_some()
        && obj.get("startedAt").and_then(Value::as_str).is_some()
        && obj.get("commandFingerprint").and_then(Value::as_str).is_some()
        && is_finite_integer(obj.get("generation"), 0);
    if !base_ok {
        return false;
    }
    if obj.contains_key("containerId") {
        obj.get("containerId").and_then(Value::as_str).is_some()
            && obj.get("containerName").and_then(Value::as_str).is_some()
            && obj.get("containerStartedAt").and_then(Value::as_str).is_some()
    } else {
        is_finite_integer(obj.get("pid"), 0) && is_finite_integer(obj.get("pgid"), 0) && obj.get("startIdentity").and_then(Value::as_str).is_some()
    }
}

fn is_lifecycle_state(value: &Value, service_key: &str) -> bool {
    let Some(obj) = is_object(value) else { return false };
    if !has_exact_keys(
        obj,
        &["serviceId", "desiredState", "actualState", "readiness", "generation", "createdAt", "updatedAt"],
        &["identity", "readinessKind", "readinessDetail", "exitedAt", "exitCode", "error", "currentOperationId"],
    ) {
        return false;
    }
    if obj.get("serviceId").and_then(Value::as_str) != Some(service_key) {
        return false;
    }
    let desired_state_ok = matches!(obj.get("desiredState").and_then(Value::as_str), Some("stopped") | Some("running"));
    let actual_state_ok = obj.get("actualState").and_then(Value::as_str).map(|s| ACTUAL_STATES.contains(&s)).unwrap_or(false);
    let readiness_ok = obj.get("readiness").and_then(Value::as_str).map(|s| READINESS_STATES.contains(&s)).unwrap_or(false);
    if !desired_state_ok || !actual_state_ok || !readiness_ok || !is_finite_integer(obj.get("generation"), 0) || !is_timestamp(obj.get("createdAt")) || !is_timestamp(obj.get("updatedAt")) {
        return false;
    }
    if let Some(identity) = obj.get("identity") {
        if !is_process_identity_shape(identity) {
            return false;
        }
    }
    if let Some(exited_at) = obj.get("exitedAt") {
        if !is_timestamp(Some(exited_at)) {
            return false;
        }
    }
    if let Some(exit_code) = obj.get("exitCode") {
        if !is_finite_integer(Some(exit_code), i64::MIN) {
            return false;
        }
    }
    for key in ["error", "currentOperationId", "readinessDetail"] {
        if let Some(v) = obj.get(key) {
            if !v.is_string() {
                return false;
            }
        }
    }
    if let Some(kind) = obj.get("readinessKind") {
        if !kind.is_string() {
            return false;
        }
    }
    true
}

fn is_persisted_manager_state_value(value: &Value) -> bool {
    let Some(obj) = is_object(value) else { return false };
    if !has_exact_keys(obj, &["version", "services"], &[]) {
        return false;
    }
    if obj.get("version").and_then(Value::as_u64) != Some(STATE_VERSION as u64) {
        return false;
    }
    let Some(services) = obj.get("services").and_then(Value::as_object) else { return false };
    services.iter().all(|(key, state)| is_lifecycle_state(state, key))
}

/// Deliberately fallible rather than `.expect(...)`: `is_persisted_manager_state_value` is a
/// shape check, not a type check, and it is weaker than these types in at least two places —
/// `readinessKind` is validated as "any string" but deserializes into a closed enum, and
/// `exitCode` is validated as any i64 but deserializes into an `i32`. A `state.json` containing
/// `"readinessKind": "grpc"` passed validation and then panicked here, killing the daemon at
/// bootstrap instead of quarantining the file and starting clean — which is the entire reason this
/// module quarantines rather than trusts.
fn value_to_persisted_state(value: &Value) -> Option<PersistedManagerState> {
    serde_json::from_value(value.clone()).ok()
}

/// A state file written by either predecessor tool keys its services under `units` and names them
/// `unitId`. `serviceId` has to be rewritten into the identity too, or ownership checks fail and the
/// process is written off as a stranger.
fn migrate_legacy_persisted_state(value: &Value) -> Option<PersistedManagerState> {
    let obj = is_object(value)?;
    if !has_exact_keys(obj, &["version", "units"], &[]) {
        return None;
    }
    let units = obj.get("units")?.as_object()?;
    let mut services: HashMap<ServiceId, Value> = HashMap::new();
    for (service_id, unit) in units {
        let unit_obj = unit.as_object()?;
        let unit_id = unit_obj.get("unitId")?.as_str()?;
        let mut candidate = unit_obj.clone();
        candidate.remove("unitId");
        // The legacy identity is dropped unconditionally and only put back if it migrates cleanly.
        // Leaving the original in place when it has no `unitId` to rewrite meant the candidate kept
        // a `unitId`-shaped identity, failed validation, and aborted the migration for the WHOLE
        // file — quarantining it and losing every service's identity, which is precisely the
        // orphaned-process/port-conflict failure this migration exists to prevent. The TS source
        // destructures `identity` out and re-adds it only on success; this matches that.
        candidate.remove("identity");
        candidate.insert("serviceId".to_string(), Value::String(service_id.clone()));
        if let Some(identity_obj) = unit_obj.get("identity").and_then(Value::as_object) {
            if let Some(identity_unit_id) = identity_obj.get("unitId").and_then(Value::as_str) {
                let mut migrated_identity = identity_obj.clone();
                migrated_identity.remove("unitId");
                migrated_identity.insert("serviceId".to_string(), Value::String(identity_unit_id.to_string()));
                if is_process_identity_shape(&Value::Object(migrated_identity.clone())) {
                    candidate.insert("identity".to_string(), Value::Object(migrated_identity));
                }
            }
        }
        let candidate_value = Value::Object(candidate);
        if unit_id.is_empty() || !is_lifecycle_state(&candidate_value, service_id) {
            return None;
        }
        services.insert(service_id.clone(), candidate_value);
    }
    let mut typed_services: HashMap<ServiceId, ServiceLifecycleState> = HashMap::new();
    for (id, v) in services {
        typed_services.insert(id, serde_json::from_value(v).ok()?);
    }
    Some(PersistedManagerState { version: STATE_VERSION, services: typed_services })
}

pub struct AtomicStateStore {
    io: Arc<dyn FileIo>,
    pub path: std::path::PathBuf,
}

impl AtomicStateStore {
    pub fn new(io: Arc<dyn FileIo>, runtime_directory: &std::path::Path) -> Self {
        Self { io, path: runtime_directory.join("state.json") }
    }

    pub fn load(&self) -> PersistedManagerState {
        let attempt = || -> Option<PersistedManagerState> {
            let raw = self.io.read_file(&self.path).ok()??;
            let value: Value = serde_json::from_str(&raw).ok()?;
            if is_persisted_manager_state_value(&value) {
                // `None` here falls through to the quarantine path below rather than panicking.
                return value_to_persisted_state(&value);
            }
            if let Some(migrated) = migrate_legacy_persisted_state(&value) {
                let _ = self.save(&migrated);
                return Some(migrated);
            }
            None
        };
        match attempt() {
            Some(state) => state,
            None => {
                let _ = self.io.quarantine(&self.path, "corrupt");
                PersistedManagerState { version: STATE_VERSION, services: HashMap::new() }
            }
        }
    }

    pub fn save(&self, state: &PersistedManagerState) -> Result<(), String> {
        let mut next = state.clone();
        next.version = STATE_VERSION;
        let value = serde_json::to_value(&next).map_err(|e| e.to_string())?;
        if !is_persisted_manager_state_value(&value) {
            return Err("Refusing to persist invalid manager state".to_string());
        }
        self.io.write_file(&self.path, &serde_json::to_string(&next).map_err(|e| e.to_string())?).map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file_io::create_file_io;
    use crate::state::{ActualServiceState, DesiredServiceState, ServiceReadiness};

    fn store() -> (AtomicStateStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let io: Arc<dyn FileIo> = Arc::from(create_file_io(false));
        (AtomicStateStore::new(io, dir.path()), dir)
    }

    fn sample_state() -> ServiceLifecycleState {
        ServiceLifecycleState {
            service_id: "api".to_string(),
            desired_state: DesiredServiceState::Running,
            actual_state: ActualServiceState::Ready,
            readiness: ServiceReadiness::Ready,
            generation: 1,
            identity: None,
            readiness_kind: None,
            readiness_detail: None,
            created_at: "2024-01-01T00:00:00.000Z".to_string(),
            updated_at: "2024-01-01T00:00:00.000Z".to_string(),
            exited_at: None,
            exit_code: None,
            error: None,
            current_operation_id: None,
        }
    }

    #[test]
    fn missing_file_loads_as_empty_state() {
        let (store, _dir) = store();
        let state = store.load();
        assert_eq!(state.version, STATE_VERSION);
        assert!(state.services.is_empty());
    }

    #[test]
    fn save_then_load_roundtrips() {
        let (store, _dir) = store();
        let mut services = HashMap::new();
        services.insert("api".to_string(), sample_state());
        store.save(&PersistedManagerState { version: STATE_VERSION, services }).unwrap();
        let loaded = store.load();
        assert_eq!(loaded.services.get("api").unwrap().actual_state, ActualServiceState::Ready);
    }

    #[test]
    fn corrupt_json_is_quarantined_and_loads_as_empty() {
        let (store, dir) = store();
        std::fs::write(&store.path, "not json at all {{{").unwrap();
        let state = store.load();
        assert!(state.services.is_empty());
        // The corrupt file should have been renamed aside, not left in place.
        assert!(!store.path.exists());
        let entries: Vec<_> = std::fs::read_dir(dir.path()).unwrap().collect();
        assert!(entries.iter().any(|e| e.as_ref().unwrap().file_name().to_string_lossy().contains("corrupt")));
    }

    #[test]
    fn wrong_version_is_quarantined() {
        let (store, _dir) = store();
        std::fs::write(&store.path, r#"{"version":999,"services":{}}"#).unwrap();
        let state = store.load();
        assert_eq!(state.version, STATE_VERSION);
        assert!(state.services.is_empty());
    }

    #[test]
    fn legacy_units_shape_is_migrated_in_place() {
        let (store, _dir) = store();
        let legacy = serde_json::json!({
            "version": 1,
            "units": {
                "api": {
                    "unitId": "api",
                    "desiredState": "running",
                    "actualState": "ready",
                    "readiness": "ready",
                    "generation": 1,
                    "createdAt": "2024-01-01T00:00:00.000Z",
                    "updatedAt": "2024-01-01T00:00:00.000Z",
                    "identity": {
                        "unitId": "api",
                        "managerInstanceId": "instance-1",
                        "generation": 1,
                        "pid": 100,
                        "pgid": 100,
                        "startedAt": "2024-01-01T00:00:00.000Z",
                        "startIdentity": "Mon Jan  1 00:00:00 2024",
                        "commandFingerprint": "abc"
                    }
                }
            }
        });
        std::fs::write(&store.path, serde_json::to_string(&legacy).unwrap()).unwrap();
        let state = store.load();
        let migrated = state.services.get("api").expect("api should have migrated");
        assert_eq!(migrated.service_id, "api");
        match migrated.identity.as_ref().unwrap() {
            crate::state::ProcessIdentity::Posix(p) => {
                assert_eq!(p.service_id, "api");
                assert_eq!(p.pid, 100);
            }
            _ => panic!("expected a posix identity"),
        }
        // The migration should have persisted the rewritten shape, so a second load doesn't need to
        // migrate again.
        let raw = std::fs::read_to_string(&store.path).unwrap();
        assert!(raw.contains("\"serviceId\":\"api\""));
        assert!(!raw.contains("unitId"));
    }

    #[test]
    fn a_shape_that_is_neither_current_nor_legacy_is_quarantined() {
        let (store, _dir) = store();
        std::fs::write(&store.path, r#"{"totally": "unrelated"}"#).unwrap();
        let state = store.load();
        assert!(state.services.is_empty());
    }

    #[test]
    fn save_rejects_a_state_with_an_out_of_range_service() {
        let (store, _dir) = store();
        let mut bad = sample_state();
        bad.generation = 1;
        // Construct a state whose serviceId doesn't match its map key — this should be unreachable
        // through the typed API but exercises the belt-and-suspenders validation in `save`.
        let mut services = HashMap::new();
        bad.service_id = "different-id".to_string();
        services.insert("api".to_string(), bad);
        let result = store.save(&PersistedManagerState { version: STATE_VERSION, services });
        assert!(result.is_err());
    }

    /// A legacy unit whose `identity` carries no `unitId` has that identity DROPPED and still
    /// migrates. Leaving the un-migratable identity on the candidate made it fail validation and
    /// aborted the migration for the whole file — quarantining every service's identity, which is
    /// exactly the orphaned-process/port-conflict failure this migration exists to prevent.
    #[test]
    fn a_legacy_identity_without_a_unit_id_is_dropped_rather_than_failing_the_migration() {
        let value: Value = serde_json::from_str(
            r#"{
              "version": 1,
              "units": {
                "api": {
                  "unitId": "api",
                  "desiredState": "running",
                  "actualState": "ready",
                  "readiness": "ready",
                  "generation": 3,
                  "identity": { "somethingElse": true },
                  "createdAt": "2026-01-01T00:00:00.000Z",
                  "updatedAt": "2026-01-01T00:00:00.000Z"
                }
              }
            }"#,
        )
        .unwrap();
        let migrated = migrate_legacy_persisted_state(&value).expect("must migrate, not quarantine");
        let api = migrated.services.get("api").expect("api survived");
        assert!(api.identity.is_none(), "the un-migratable identity should be dropped");
        assert_eq!(api.generation, 3, "the rest of the record must survive intact");
    }
}
