//! `HearthManager` — ties together the event store (`ManagerEventStore`),
//! `OperationScheduler`, `CursorLogStore`, `AtomicStateStore`, the lock-claim protocol and a
//! `ProcessSupervisor`, behind an `axum` server on a loopback, OS-assigned port.
//! Route handlers live in [`routes`].
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Map, Value};
use tokio::net::TcpListener;
use tokio::sync::watch;
use uuid::Uuid;

use crate::catalog::{validate_catalog, ServiceCatalog, ServiceId, ServiceOwnership};
use crate::file_io::{create_file_io, FileIo};
use crate::platform::{is_supported_hearth_platform, unsupported_platform_message};
use crate::state::{
    ActualServiceState, ManagerInfo, ManagerMetadata, PersistedManagerState, ServiceLifecycleState,
    PROTOCOL_VERSION, STATE_VERSION,
};
use crate::supervisor::engine::ACTIVE_STATES;
use crate::supervisor::types::{format_iso8601_millis, Host, SupervisorOptions};
use crate::supervisor::ProcessSupervisor;

use super::event_store::ManagerEventStore;
use super::lock::{
    claim_lock, prepare_owned_lock_release, read_lock_ownership_key, release_owned_lock,
    ClaimLockError, LockHandle,
};
use super::log_store::CursorLogStore;
use super::operations::{OperationScheduler, RequestIdConflict};
use super::state_store::AtomicStateStore;
use super::ShutdownMode;

mod routes;
pub use routes::router;
pub(crate) use routes::{ensure_not_closing, parse_bool_flag, strict_body};

const MANAGER_METADATA_VERSION: u32 = 1;

pub(crate) fn now() -> String {
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    format_iso8601_millis(millis)
}

/// The one `service.lifecycle` payload shape, published once per transition by
/// `HearthManager::set_service_state` and by the queued-start bookkeeping below.
fn lifecycle_event(state: &ServiceLifecycleState) -> Map<String, Value> {
    let mut data = Map::new();
    data.insert("serviceId".to_string(), json!(state.service_id));
    data.insert(
        "actualState".to_string(),
        json!(state.actual_state.as_wire_str()),
    );
    data.insert("readiness".to_string(), json!(state.readiness));
    data.insert("generation".to_string(), json!(state.generation));
    data.insert("operationId".to_string(), json!(state.current_operation_id));
    data
}

fn default_daemon_owned(catalog: &ServiceCatalog, service_id: &str) -> bool {
    !matches!(
        catalog
            .services
            .iter()
            .find(|s| s.id == service_id)
            .and_then(|s| s.ownership),
        Some(ServiceOwnership::External)
    )
}

// ---------------------------------------------------------------------------------------------
// HTTP error envelope
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct ManagerHttpError {
    pub status: StatusCode,
    pub code: String,
    pub message: String,
}
impl ManagerHttpError {
    pub(crate) fn new(status: StatusCode, code: &str, message: impl Into<String>) -> Self {
        Self {
            status,
            code: code.to_string(),
            message: message.into(),
        }
    }
}
impl IntoResponse for ManagerHttpError {
    fn into_response(self) -> Response {
        let body = json!({ "error": { "code": self.code, "message": self.message } });
        (self.status, [("cache-control", "no-store")], Json(body)).into_response()
    }
}
impl From<RequestIdConflict> for ManagerHttpError {
    fn from(_: RequestIdConflict) -> Self {
        Self::new(
            StatusCode::CONFLICT,
            "request_id_conflict",
            "requestId is already used by a different operation",
        )
    }
}
pub(crate) type HttpResult<T> = Result<T, ManagerHttpError>;

pub(crate) fn json_response(body: impl serde::Serialize, status: StatusCode) -> Response {
    (status, [("cache-control", "no-store")], Json(body)).into_response()
}

// ---------------------------------------------------------------------------------------------
// HearthManager
// ---------------------------------------------------------------------------------------------

pub struct HearthManagerOptions {
    pub runtime_directory: Option<PathBuf>,
    pub root: Option<PathBuf>,
    pub catalog: ServiceCatalog,
    pub event_capacity: Option<usize>,
    pub log_tail_bytes: Option<u64>,
    pub log_max_bytes: Option<u64>,
    pub log_rotation_count: Option<usize>,
    pub supervisor: Option<SupervisorOptions>,
    /// Set only for the smp daemon (`hearth smp`) — enables the `/v1/shared/*` route surface.
    pub shared: Option<Arc<crate::shared::SharedContext>>,
}

pub struct HearthManager {
    pub instance_id: String,
    pub events: Arc<ManagerEventStore>,
    pub operations: Arc<OperationScheduler>,
    pub logs: Arc<CursorLogStore>,
    pub state_store: AtomicStateStore,
    pub runtime_directory: PathBuf,
    io: Arc<dyn FileIo>,
    lock: LockHandle,
    token: String,
    catalog: RwLock<Arc<ServiceCatalog>>,
    state: Mutex<PersistedManagerState>,
    metadata: Mutex<Option<ManagerMetadata>>,
    closed: AtomicBool,
    pub(crate) closing: AtomicBool,
    lifecycle: tokio::sync::Mutex<()>,
    catalog_reload_serial: tokio::sync::Mutex<()>,
    /// Serializes the snapshot→write→fsync sequence so saves stay ordered while the file I/O runs
    /// AFTER `state`'s lock is released — an F_FULLFSYNC can cost tens of ms and must not block
    /// `service_states()` or the supervisor.
    persist_lock: Mutex<()>,
    /// `requestId` → (catalog, response) for recent `/v1/manager/reload` calls, so a retried reload
    /// replays its original answer instead of re-applying (and reporting nothing changed).
    reload_requests: Mutex<VecDeque<(String, Value, Value)>>,
    reload_requests_serial: tokio::sync::Mutex<()>,
    supervisor: OnceLock<Arc<ProcessSupervisor>>,
    external_sync_task: Mutex<Option<tokio::task::JoinHandle<()>>>,
    shutdown_done: watch::Sender<bool>,
    pub shared: Option<Arc<crate::shared::SharedContext>>,
}

pub struct ReloadOutcome {
    pub stopped: Vec<ServiceId>,
    pub changed: Vec<ServiceId>,
}

/// Why a catalog reload was refused. On every variant the previous catalog stays in place.
#[derive(Debug)]
pub enum ReloadError {
    Closing,
    Invalid(Vec<String>),
    /// Removed services that were active and failed to stop — `(service id, stop error)` — plus
    /// the ones that DID stop before the failure. Swapping the catalog anyway would drop a
    /// still-running service from every UI.
    StopFailed {
        failures: Vec<(ServiceId, String)>,
        stopped: Vec<ServiceId>,
    },
}

impl std::fmt::Display for ReloadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Closing => f.write_str("manager is shutting down"),
            Self::Invalid(errors) => f.write_str(&errors.join("; ")),
            Self::StopFailed { failures, stopped } => {
                let detail = failures
                    .iter()
                    .map(|(id, error)| format!("{id}: {error}"))
                    .collect::<Vec<_>>()
                    .join("; ");
                let stopped_note = if stopped.is_empty() {
                    String::new()
                } else {
                    format!("; stopped before the failure: {}", stopped.join(", "))
                };
                write!(f, "catalog not reloaded; removed services failed to stop ({detail}){stopped_note}")
            }
        }
    }
}

impl From<ReloadError> for ManagerHttpError {
    fn from(error: ReloadError) -> Self {
        match &error {
            ReloadError::Closing => Self::new(
                StatusCode::CONFLICT,
                "manager_closing",
                "Manager is shutting down",
            ),
            ReloadError::Invalid(_) => Self::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "invalid_catalog",
                error.to_string(),
            ),
            ReloadError::StopFailed { .. } => {
                Self::new(StatusCode::CONFLICT, "stop_failed", error.to_string())
            }
        }
    }
}

impl HearthManager {
    pub(crate) fn supervisor(&self) -> &Arc<ProcessSupervisor> {
        self.supervisor.get().expect(
            "supervisor is set immediately after construction, before any other method can run",
        )
    }

    pub fn info(&self) -> ManagerInfo {
        let metadata = self
            .metadata
            .lock()
            .unwrap()
            .clone()
            .expect("manager has not started");
        ManagerInfo {
            protocol_version: metadata.protocol_version,
            instance_id: metadata.instance_id,
            pid: metadata.pid,
            port: metadata.port,
            started_at: metadata.started_at,
            metadata_version: metadata.version,
            runtime_directory: self.runtime_directory.display().to_string(),
        }
    }

    pub fn bearer_token(&self) -> &str {
        &self.token
    }

    pub fn base_url(&self) -> String {
        let metadata = self
            .metadata
            .lock()
            .unwrap()
            .clone()
            .expect("manager has not started");
        format!("http://127.0.0.1:{}", metadata.port)
    }

    pub fn catalog(&self) -> Arc<ServiceCatalog> {
        self.catalog.read().unwrap().clone()
    }

    fn lifecycle_generation(&self, service_id: &str) -> u64 {
        self.state
            .lock()
            .unwrap()
            .services
            .get(service_id)
            .map(|s| s.generation)
            .unwrap_or(0)
    }

    /// Swaps in a new catalog after validating it. A service removed from the new catalog that is
    /// currently active gets stopped first, using the *old* catalog (the supervisor needs the old
    /// definition to know how to stop it) — only then does the swap happen, so a removed-but-still-
    /// stopping service is never briefly invisible from `service_states()`/`/v1/services` while its
    /// process is still alive. `external`-owned removed services are left alone. A service that
    /// stays present but whose definition changed is left running as-is and reported in `changed`.
    /// A removed service that fails to stop aborts the whole reload (`StopFailed`) and the old
    /// catalog stays — a stop must never be reported without stopping something.
    /// Serialized against itself (not `self.lifecycle`, which `supervisor.stop`'s own state writes
    /// run through — nesting into that from here would deadlock).
    pub async fn reload_catalog(
        &self,
        next_catalog: ServiceCatalog,
    ) -> Result<ReloadOutcome, ReloadError> {
        let _guard = self.catalog_reload_serial.lock().await;
        if self.closing.load(Ordering::SeqCst) {
            return Err(ReloadError::Closing);
        }
        let validation = validate_catalog(&next_catalog);
        if !validation.errors.is_empty() {
            return Err(ReloadError::Invalid(validation.errors));
        }
        let previous = self.catalog.read().unwrap().clone();
        let next_ids: std::collections::HashSet<&ServiceId> =
            next_catalog.services.iter().map(|s| &s.id).collect();
        let removed_ids: Vec<ServiceId> = previous
            .services
            .iter()
            .filter(|s| !next_ids.contains(&s.id))
            .map(|s| s.id.clone())
            .collect();
        let changed: Vec<ServiceId> = previous
            .services
            .iter()
            .filter(|s| {
                next_catalog
                    .services
                    .iter()
                    .find(|n| n.id == s.id)
                    .map(|n| n != *s)
                    .unwrap_or(false)
            })
            .map(|s| s.id.clone())
            .collect();

        let mut stopped = Vec::new();
        let mut failed = Vec::new();
        for service_id in &removed_ids {
            if !default_daemon_owned(&previous, service_id) {
                continue;
            }
            let is_active = self
                .state
                .lock()
                .unwrap()
                .services
                .get(service_id)
                .map(|s| ACTIVE_STATES.contains(&s.actual_state))
                .unwrap_or(false);
            if !is_active {
                continue;
            }
            match self.supervisor().stop(service_id, None).await {
                Ok(()) => stopped.push(service_id.clone()),
                Err(error) => failed.push((service_id.clone(), error.0)),
            }
        }
        if !failed.is_empty() {
            return Err(ReloadError::StopFailed {
                failures: failed,
                stopped,
            });
        }
        let has_external = next_catalog
            .services
            .iter()
            .any(|s| matches!(s.ownership, Some(ServiceOwnership::External)));
        *self.catalog.write().unwrap() = Arc::new(next_catalog);
        let mut data = Map::new();
        data.insert("removed".to_string(), json!(removed_ids));
        data.insert("changed".to_string(), json!(changed));
        data.insert("stopped".to_string(), json!(stopped));
        self.events.publish("manager.catalog-reloaded", data);
        if has_external {
            // The first `ownership: external` service can arrive by reload (a new `shared:` entry)
            // — adopt it now rather than on the next 2s tick. Spawned, not awaited: the pass takes
            // each per-service lock in turn, and a slow probe or an in-flight attach would
            // otherwise stall the reload response past the client's manager timeout.
            let sup = self.supervisor().clone();
            tokio::spawn(async move {
                sup.sync_external_services().await;
            });
        }
        Ok(ReloadOutcome { stopped, changed })
    }

    /// Persists the current `state` after the caller has mutated and RELEASED its lock — the
    /// snapshot is taken and written under `persist_lock` so consecutive saves stay ordered, and
    /// the write+fsync never runs inside `state`'s lock. A failed save surfaces through
    /// `manager.error` instead of stopping persistence silently.
    fn persist(&self) {
        let _ordered = self.persist_lock.lock().unwrap();
        let snapshot = self.state.lock().unwrap().clone();
        if let Err(error) = self.state_store.save(&snapshot) {
            <Self as Host>::record_background_error(self, "state-store", &error);
        }
    }

    pub fn shutdown_completion(&self) -> watch::Receiver<bool> {
        self.shutdown_done.subscribe()
    }

    /// Alias for HTTP `"refuse-if-active"` after the caller has already confirmed nothing is active:
    /// shut down and leave managed services running for re-adoption.
    pub async fn close(self: &Arc<Self>) {
        self.shutdown(ShutdownMode::LeaveServices).await;
    }

    /// Shut the manager down. [`ShutdownMode::StopServices`] stops managed processes first;
    /// [`ShutdownMode::LeaveServices`] detaches them for the next daemon to re-adopt (also what
    /// SIGTERM / `hearth manager restart` use once past any refuse-if-active guard).
    pub async fn shutdown(self: &Arc<Self>, mode: ShutdownMode) {
        self.closing.store(true, Ordering::SeqCst);
        self.operations.close_mutations();
        self.supervisor().begin_shutdown();
        self.operations.drain_services().await;
        if mode.stops_services() {
            self.supervisor().shutdown().await;
        } else {
            // Services stay up; the `docker logs` followers this daemon spawned for them do not.
            self.supervisor().detach_all_output();
        }
        let _guard = self.lifecycle.lock().await;
        self.close_locked().await;
    }

    async fn close_locked(&self) {
        if self.closed.swap(true, Ordering::SeqCst) {
            return;
        }
        if let Some(handle) = self.external_sync_task.lock().unwrap().take() {
            handle.abort();
        }
        let release_prepared =
            prepare_owned_lock_release(self.io.as_ref(), &self.lock, &self.token).await;
        self.events.publish(
            "manager.stopped",
            json!({ "instanceId": self.instance_id })
                .as_object()
                .unwrap()
                .clone(),
        );
        if release_prepared {
            release_owned_lock(self.io.as_ref(), &self.lock, &self.token).await;
        }
        let _ = self.shutdown_done.send(true);
    }
}

#[async_trait::async_trait]
impl Host for HearthManager {
    fn instance_id(&self) -> String {
        self.instance_id.clone()
    }
    fn catalog(&self) -> Arc<ServiceCatalog> {
        self.catalog.read().unwrap().clone()
    }
    fn service_states(&self) -> Vec<ServiceLifecycleState> {
        let timestamp = now();
        let state = self.state.lock().unwrap();
        self.catalog
            .read()
            .unwrap()
            .services
            .iter()
            .map(|s| default_state_or(state.services.get(&s.id), &s.id, &timestamp))
            .collect()
    }
    fn service_state(&self, service_id: &ServiceId) -> Option<ServiceLifecycleState> {
        let catalog = self.catalog.read().unwrap();
        let definition = catalog.services.iter().find(|s| &s.id == service_id)?;
        let timestamp = now();
        let state = self.state.lock().unwrap();
        Some(default_state_or(
            state.services.get(&definition.id),
            &definition.id,
            &timestamp,
        ))
    }
    async fn set_service_state(&self, next: ServiceLifecycleState) {
        let _guard = self.lifecycle.lock().await;
        if self.closed.load(Ordering::SeqCst) {
            return;
        }
        let event = lifecycle_event(&next);
        let mut state = self.state.lock().unwrap();
        state.services.insert(next.service_id.clone(), next);
        drop(state);
        self.persist();
        // The only `service.lifecycle` publish for a supervisor transition — the engine does not
        // publish its own copy.
        self.events.publish("service.lifecycle", event);
    }
    async fn append_log(&self, service_id: &str, data: &str) {
        if self.closed.load(Ordering::SeqCst) {
            return;
        }
        if let Err(error) = self.logs.append(&service_id.to_string(), data).await {
            self.record_background_error(&format!("service-log:{service_id}"), &error.to_string());
            return;
        }
        let mut event_data = Map::new();
        event_data.insert("serviceId".to_string(), json!(service_id));
        self.events.publish("service.log", event_data);
    }
    fn publish(&self, event_type: &str, data: Value) {
        let map = data.as_object().cloned().unwrap_or_default();
        self.events.publish(event_type, map);
    }
    fn record_background_error(&self, scope: &str, error: &str) {
        let mut data = Map::new();
        data.insert("scope".to_string(), json!(scope));
        data.insert("message".to_string(), json!(error));
        self.events.publish("manager.error", data);
    }
}

fn default_state_or(
    existing: Option<&ServiceLifecycleState>,
    service_id: &str,
    timestamp: &str,
) -> ServiceLifecycleState {
    existing.cloned().unwrap_or_else(|| ServiceLifecycleState {
        service_id: service_id.to_string(),
        desired_state: crate::state::DesiredServiceState::Stopped,
        actual_state: ActualServiceState::Stopped,
        readiness: crate::state::ServiceReadiness::Unknown,
        generation: 0,
        identity: None,
        readiness_kind: None,
        readiness_detail: None,
        created_at: timestamp.to_string(),
        updated_at: timestamp.to_string(),
        exited_at: None,
        exit_code: None,
        error: None,
        current_operation_id: None,
    })
}

#[derive(Debug, thiserror::Error)]
pub enum BootstrapError {
    #[error("{0}")]
    Message(String),
    #[error(transparent)]
    ClaimLock(#[from] ClaimLockError),
}

pub async fn bootstrap(
    options: HearthManagerOptions,
) -> Result<Arc<HearthManager>, BootstrapError> {
    let platform = crate::platform::current_platform();
    if !is_supported_hearth_platform(platform) {
        return Err(BootstrapError::Message(unsupported_platform_message(
            platform,
        )));
    }
    let validation = validate_catalog(&options.catalog);
    if !validation.errors.is_empty() {
        return Err(BootstrapError::Message(format!(
            "Invalid service catalog: {}",
            validation.errors.join("; ")
        )));
    }
    let root = match options.root.clone() {
        Some(root) => root,
        None => std::env::current_dir().map_err(|e| {
            BootstrapError::Message(format!("cannot resolve the current directory: {e}"))
        })?,
    };
    let runtime_directory = options.runtime_directory.clone().unwrap_or_else(|| {
        crate::paths::resolve_runtime_directory(&root, options.catalog.runtime_directory.as_deref())
    });
    let io: Arc<dyn FileIo> = Arc::from(create_file_io(
        options.catalog.private_file_guard != Some(false),
    ));
    let token = super::lock::random_token();
    let http_client = reqwest::Client::new();
    let bootstrap_metadata = ManagerMetadata {
        version: MANAGER_METADATA_VERSION,
        protocol_version: PROTOCOL_VERSION,
        instance_id: Uuid::new_v4().to_string(),
        pid: std::process::id() as i64,
        port: 0,
        started_at: now(),
    };
    let lock = claim_lock(
        io.as_ref(),
        &runtime_directory,
        &bootstrap_metadata,
        &token,
        &http_client,
    )
    .await?;

    let events = ManagerEventStore::new(
        options.event_capacity,
        Some(bootstrap_metadata.instance_id.clone()),
    );
    let operations = OperationScheduler::new(events.clone());
    let logs = CursorLogStore::new(
        io.clone(),
        runtime_directory.join("logs"),
        options.log_tail_bytes,
        options.log_max_bytes,
        options.log_rotation_count,
    );
    let state_store = AtomicStateStore::new(io.clone(), &runtime_directory).with_error_reporter({
        let events = events.clone();
        Arc::new(move |message: &str| {
            let mut data = Map::new();
            data.insert("scope".to_string(), json!("state-store"));
            data.insert("message".to_string(), json!(message));
            events.publish("manager.error", data);
        })
    });
    let (shutdown_done, _rx) = watch::channel(false);

    let manager = Arc::new(HearthManager {
        instance_id: bootstrap_metadata.instance_id.clone(),
        events,
        operations,
        logs: Arc::new(logs),
        state_store,
        runtime_directory: runtime_directory.clone(),
        io: io.clone(),
        lock: lock.clone(),
        token: token.clone(),
        catalog: RwLock::new(Arc::new(options.catalog.clone())),
        state: Mutex::new(PersistedManagerState {
            version: STATE_VERSION,
            services: HashMap::new(),
        }),
        metadata: Mutex::new(None),
        closed: AtomicBool::new(false),
        closing: AtomicBool::new(false),
        lifecycle: tokio::sync::Mutex::new(()),
        persist_lock: Mutex::new(()),
        catalog_reload_serial: tokio::sync::Mutex::new(()),
        reload_requests: Mutex::new(VecDeque::new()),
        reload_requests_serial: tokio::sync::Mutex::new(()),
        supervisor: OnceLock::new(),
        external_sync_task: Mutex::new(None),
        shutdown_done,
        shared: options.shared.clone(),
    });

    let mut supervisor_options = options.supervisor.unwrap_or_else(|| {
        crate::supervisor::default_adapters::default_supervisor_options(
            root.clone(),
            Some(runtime_directory.clone()),
            None,
        )
    });
    let manager_for_closing = manager.clone();
    supervisor_options.is_closing =
        Arc::new(move || manager_for_closing.closing.load(Ordering::SeqCst));
    let supervisor = ProcessSupervisor::new(manager.clone(), supervisor_options);
    let _ = manager.supervisor.set(supervisor);

    let bootstrap_result: Result<(), BootstrapError> = async {
        *manager.state.lock().unwrap() = manager.state_store.load();
        manager.supervisor().reconcile().await;

        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .map_err(|e| BootstrapError::Message(e.to_string()))?;
        let port = listener
            .local_addr()
            .map_err(|e| BootstrapError::Message(e.to_string()))?
            .port();
        let app = router(manager.clone());
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let final_metadata = ManagerMetadata {
            port,
            ..bootstrap_metadata.clone()
        };
        *manager.metadata.lock().unwrap() = Some(final_metadata.clone());
        let key = read_lock_ownership_key(io.as_ref(), &runtime_directory)
            .ok_or_else(|| BootstrapError::Message("Missing ownership key".to_string()))?;
        io.write_file(
            &lock.metadata_path,
            &serde_json::to_string(&final_metadata).unwrap(),
        )
        .map_err(|e| BootstrapError::Message(e.to_string()))?;
        let proof = super::lock::create_lock_ownership_proof(&key, &final_metadata, &token);
        io.write_file(&lock.proof_path, &serde_json::to_string(&proof).unwrap())
            .map_err(|e| BootstrapError::Message(e.to_string()))?;
        manager.events.publish(
            "manager.started",
            json!({ "instanceId": manager.instance_id })
                .as_object()
                .unwrap()
                .clone(),
        );

        // The adoption loop runs unconditionally: a catalog with no `ownership: external` service
        // makes each tick a no-op, and the first such service may only arrive later by reload.
        manager.supervisor().sync_external_services().await;
        let sup = manager.supervisor().clone();
        let handle = tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                sup.sync_external_services().await;
            }
        });
        *manager.external_sync_task.lock().unwrap() = Some(handle);
        Ok(())
    }
    .await;

    if let Err(error) = bootstrap_result {
        let _ = prepare_owned_lock_release(io.as_ref(), &lock, &token).await;
        release_owned_lock(io.as_ref(), &lock, &token).await;
        return Err(error);
    }
    Ok(manager)
}

// ---------------------------------------------------------------------------------------------
// Router + middleware
// ---------------------------------------------------------------------------------------------

// =============================================================================================
// Real end-to-end test: a real bootstrap()'ed manager, a real spawned TCP-readiness service, real
// HTTP calls via reqwest against the real bound port — proof that every store (lock, state,
// events, operations, logs) and the supervisor are wired together behind the HTTP+SSE surface
// the clients (CLI, TUI, MCP) use.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::{
        CommandSpec, ReadinessSpec, ServiceCommand, ServiceDefinition, ServiceKind,
        ServiceProfiles, ServiceRunProfile, StartFailurePolicy,
    };
    use std::time::Duration;

    fn free_port() -> u16 {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        port
    }

    fn tcp_service(id: &str, port: u16) -> ServiceDefinition {
        ServiceDefinition {
            id: id.to_string(),
            label: None,
            kind: Some(ServiceKind::Application),
            ownership: None,
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
        }
    }

    async fn wait_for_operation(
        client: &reqwest::Client,
        base: &str,
        token: &str,
        id: &str,
    ) -> Value {
        for _ in 0..100 {
            let resp: Value = client
                .get(format!("{base}/v1/operations/{id}"))
                .bearer_auth(token)
                .header("x-hearth-protocol", PROTOCOL_VERSION.to_string())
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            let status = resp["operation"]["status"].as_str().unwrap_or("");
            if status == "succeeded" || status == "failed" {
                return resp;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("operation {id} did not settle in time");
    }

    #[tokio::test]
    async fn real_manager_full_lifecycle_over_real_http() {
        let port = free_port();
        let catalog = ServiceCatalog {
            services: vec![tcp_service("api", port)],
            groups: HashMap::new(),
            group_tree: Vec::new(),
            compose_file: None,
            runtime_directory: None,
            start_failure_policy: StartFailurePolicy::StopOnFirstFailureKeepStarted,
            private_file_guard: Some(false),
        };
        let dir = tempfile::tempdir().unwrap();
        let manager = bootstrap(HearthManagerOptions {
            runtime_directory: Some(dir.path().to_path_buf()),
            root: Some(PathBuf::from("/tmp")),
            catalog,
            event_capacity: None,
            log_tail_bytes: None,
            log_max_bytes: None,
            log_rotation_count: None,
            supervisor: None,
            shared: None,
        })
        .await
        .unwrap();

        let base = manager.base_url();
        let token = manager.bearer_token().to_string();
        let client = reqwest::Client::new();

        // GET /healthz needs no auth, and omits instanceId when unauthorized.
        let health: Value = client
            .get(format!("{base}/healthz"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(health["status"], "ok");
        assert!(health.get("instanceId").is_none());
        let health_authed: Value = client
            .get(format!("{base}/healthz"))
            .bearer_auth(&token)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(health_authed["instanceId"], json!(manager.instance_id));

        // Every /v1 route requires auth + the protocol header.
        let unauthorized = client
            .get(format!("{base}/v1/services"))
            .send()
            .await
            .unwrap();
        assert_eq!(unauthorized.status(), 401);
        let no_protocol = client
            .get(format!("{base}/v1/services"))
            .bearer_auth(&token)
            .send()
            .await
            .unwrap();
        assert_eq!(no_protocol.status(), 426);

        let get = |path: String| {
            let client = client.clone();
            let base = base.clone();
            let token = token.clone();
            async move {
                client
                    .get(format!("{base}{path}"))
                    .bearer_auth(&token)
                    .header("x-hearth-protocol", PROTOCOL_VERSION.to_string())
                    .send()
                    .await
                    .unwrap()
            }
        };

        let manager_info: Value = get("/v1/manager".to_string()).await.json().await.unwrap();
        assert_eq!(
            manager_info["protocolVersion"].as_u64(),
            Some(u64::from(PROTOCOL_VERSION))
        );
        assert_eq!(manager_info["instanceId"], json!(manager.instance_id));

        let catalog_resp: Value = get("/v1/catalog".to_string()).await.json().await.unwrap();
        assert_eq!(catalog_resp["catalog"]["services"][0]["id"], "api");

        let services: Value = get("/v1/services".to_string()).await.json().await.unwrap();
        assert_eq!(services["services"][0]["actualState"], "stopped");

        // Start the service via POST /v1/operations, wait for the operation to settle, verify ready.
        let start_response: Value = client
            .post(format!("{base}/v1/operations"))
            .bearer_auth(&token)
            .header("x-hearth-protocol", PROTOCOL_VERSION.to_string())
            .json(&json!({"requestId": "req-start-1", "serviceId": "api", "action": "start"}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let operation_id = start_response["operation"]["id"]
            .as_str()
            .unwrap()
            .to_string();
        let settled = wait_for_operation(&client, &base, &token, &operation_id).await;
        assert_eq!(settled["operation"]["status"], "succeeded");

        let services_after_start: Value =
            get("/v1/services".to_string()).await.json().await.unwrap();
        assert_eq!(services_after_start["services"][0]["actualState"], "ready");
        let pid = services_after_start["services"][0]["identity"]["pid"]
            .as_i64()
            .unwrap();
        assert!(
            nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid as i32), None).is_ok(),
            "the real spawned process should be alive"
        );

        // GET /v1/logs/:id — service_not_found for an unknown id, ok (possibly empty) for a real one.
        let missing_log = get("/v1/logs/does-not-exist".to_string()).await;
        assert_eq!(missing_log.status(), 404);
        let log_slice: Value = get("/v1/logs/api".to_string()).await.json().await.unwrap();
        assert_eq!(log_slice["serviceId"], "api");

        // GET /v1/daemon/log — empty slice while no daemon.log exists, then a byte-bounded tail.
        let empty_daemon: Value = get("/v1/daemon/log".to_string())
            .await
            .json()
            .await
            .unwrap();
        assert_eq!(empty_daemon["serviceId"], "daemon");
        assert_eq!(empty_daemon["data"], "");
        assert_eq!(empty_daemon["reset"], true);
        std::fs::write(dir.path().join("daemon.log"), "aa bb\ncc dd\n").unwrap();
        let daemon_log: Value = get("/v1/daemon/log?bytes=6".to_string())
            .await
            .json()
            .await
            .unwrap();
        assert_eq!(daemon_log["data"], "cc dd\n");
        assert_eq!(daemon_log["truncated"], true);
        let bad_bytes = get("/v1/daemon/log?bytes=0".to_string()).await;
        assert_eq!(bad_bytes.status(), 400);

        // GET /v1/events — at least manager.started and some service.lifecycle events should exist.
        let events: Value = get("/v1/events".to_string()).await.json().await.unwrap();
        assert!(events["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["type"] == "manager.started"));
        assert!(events["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["type"] == "service.lifecycle"));

        // Duplicate requestId returns the same operation instead of starting a second one.
        let duplicate: Value = client
            .post(format!("{base}/v1/operations"))
            .bearer_auth(&token)
            .header("x-hearth-protocol", PROTOCOL_VERSION.to_string())
            .json(&json!({"requestId": "req-start-1", "serviceId": "api", "action": "start"}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(duplicate["operation"]["id"], operation_id);

        // Stop the service, verify the real OS process is actually gone.
        let stop_response: Value = client
            .post(format!("{base}/v1/operations"))
            .bearer_auth(&token)
            .header("x-hearth-protocol", PROTOCOL_VERSION.to_string())
            .json(&json!({"requestId": "req-stop-1", "serviceId": "api", "action": "stop"}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        wait_for_operation(
            &client,
            &base,
            &token,
            stop_response["operation"]["id"].as_str().unwrap(),
        )
        .await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(
            nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid as i32), None).is_err(),
            "the real process must be gone after stop"
        );
        let services_after_stop: Value =
            get("/v1/services".to_string()).await.json().await.unwrap();
        assert_eq!(services_after_stop["services"][0]["actualState"], "stopped");

        // Shut down the manager itself; the server should stop accepting new connections afterward.
        let shutdown_response = client
            .post(format!("{base}/v1/manager/shutdown"))
            .bearer_auth(&token)
            .header("x-hearth-protocol", PROTOCOL_VERSION.to_string())
            .json(&json!({"requestId": "req-shutdown-1", "mode": "stop-services"}))
            .send()
            .await
            .unwrap();
        assert_eq!(shutdown_response.status(), 202);
        let mut completion = manager.shutdown_completion();
        tokio::time::timeout(Duration::from_secs(5), async {
            while !*completion.borrow() {
                completion.changed().await.ok();
            }
        })
        .await
        .expect("manager should finish shutting down");
    }

    /// `hearth manager restart` shuts the daemon down with `leave-services`: the daemon goes away
    /// but a running daemon-owned service must survive it (it is detached, and the next daemon
    /// re-adopts it from its persisted identity). The `refuse-if-active` guard must stay exactly as
    /// strict as it was — it is the mode that exists to refuse.
    #[tokio::test]
    async fn leave_services_shutdown_keeps_a_running_service_alive() {
        let port = free_port();
        let dir = tempfile::tempdir().unwrap();
        let catalog = ServiceCatalog {
            services: vec![tcp_service("api", port)],
            groups: HashMap::new(),
            group_tree: Vec::new(),
            compose_file: None,
            runtime_directory: None,
            start_failure_policy: StartFailurePolicy::StopOnFirstFailureKeepStarted,
            private_file_guard: Some(false),
        };
        let manager = bootstrap(HearthManagerOptions {
            runtime_directory: Some(dir.path().to_path_buf()),
            root: Some(PathBuf::from("/tmp")),
            catalog,
            event_capacity: None,
            log_tail_bytes: None,
            log_max_bytes: None,
            log_rotation_count: None,
            supervisor: None,
            shared: None,
        })
        .await
        .unwrap();
        manager
            .supervisor()
            .start(&"api".to_string(), None)
            .await
            .unwrap();
        let pid = match manager.service_states()[0].identity.clone().unwrap() {
            crate::state::ProcessIdentity::Posix(posix) => posix.pid,
            other => panic!("expected a posix identity, got {other:?}"),
        };
        let alive = || nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid as i32), None).is_ok();
        assert!(
            alive(),
            "the spawned service should be running before the shutdown"
        );

        let client = reqwest::Client::new();
        let base = manager.base_url();
        let token = manager.bearer_token().to_string();
        let shutdown = |mode: &'static str| {
            let client = client.clone();
            let base = base.clone();
            let token = token.clone();
            async move {
                client
                    .post(format!("{base}/v1/manager/shutdown"))
                    .bearer_auth(&token)
                    .header("x-hearth-protocol", PROTOCOL_VERSION.to_string())
                    .json(&json!({ "requestId": format!("req-shutdown-{mode}"), "mode": mode }))
                    .send()
                    .await
                    .unwrap()
            }
        };

        // Subscribed BEFORE the shutdown is triggered: `shutdown_completion` is a `watch` value, and
        // `watch::Sender::send` is a no-op when no receiver exists yet — a `leave-services` shutdown
        // has nothing to stop, so it can finish before a later subscriber ever looks.
        let mut completion = manager.shutdown_completion();
        assert_eq!(
            shutdown("refuse-if-active").await.status(),
            409,
            "an active service must still refuse a plain shutdown"
        );
        assert_eq!(shutdown("leave-services").await.status(), 202);

        tokio::time::timeout(Duration::from_secs(5), async {
            while !*completion.borrow() {
                completion.changed().await.ok();
            }
        })
        .await
        .expect("manager should finish shutting down");

        assert!(
            alive(),
            "leave-services must not stop the service's process"
        );
        assert_eq!(
            manager.service_states()[0].actual_state,
            ActualServiceState::Ready,
            "the service must still be recorded as ready for the next daemon to re-adopt"
        );

        // This test owns the process it spawned — nothing else will reap it.
        let _ = nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(pid as i32),
            nix::sys::signal::Signal::SIGTERM,
        );
    }

    #[tokio::test]
    async fn unknown_route_is_404() {
        let dir = tempfile::tempdir().unwrap();
        let catalog = ServiceCatalog {
            services: vec![],
            groups: HashMap::new(),
            group_tree: Vec::new(),
            compose_file: None,
            runtime_directory: None,
            start_failure_policy: StartFailurePolicy::StopOnFirstFailureKeepStarted,
            private_file_guard: Some(false),
        };
        let manager = bootstrap(HearthManagerOptions {
            runtime_directory: Some(dir.path().to_path_buf()),
            root: Some(PathBuf::from("/tmp")),
            catalog,
            event_capacity: None,
            log_tail_bytes: None,
            log_max_bytes: None,
            log_rotation_count: None,
            supervisor: None,
            shared: None,
        })
        .await
        .unwrap();
        let client = reqwest::Client::new();
        let response = client
            .get(format!("{}/v1/does-not-exist", manager.base_url()))
            .bearer_auth(manager.bearer_token())
            .header("x-hearth-protocol", PROTOCOL_VERSION.to_string())
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 404);
        manager.close().await;
    }

    #[tokio::test]
    async fn reload_catalog_stops_a_removed_active_service() {
        let port = free_port();
        let dir = tempfile::tempdir().unwrap();
        let catalog = ServiceCatalog {
            services: vec![tcp_service("api", port)],
            groups: HashMap::new(),
            group_tree: Vec::new(),
            compose_file: None,
            runtime_directory: None,
            start_failure_policy: StartFailurePolicy::StopOnFirstFailureKeepStarted,
            private_file_guard: Some(false),
        };
        let manager = bootstrap(HearthManagerOptions {
            runtime_directory: Some(dir.path().to_path_buf()),
            root: Some(PathBuf::from("/tmp")),
            catalog,
            event_capacity: None,
            log_tail_bytes: None,
            log_max_bytes: None,
            log_rotation_count: None,
            supervisor: None,
            shared: None,
        })
        .await
        .unwrap();
        manager
            .supervisor()
            .start(&"api".to_string(), None)
            .await
            .unwrap();
        assert_eq!(
            manager.service_states()[0].actual_state,
            ActualServiceState::Ready
        );

        let empty_catalog = ServiceCatalog {
            services: vec![],
            groups: HashMap::new(),
            group_tree: Vec::new(),
            compose_file: None,
            runtime_directory: None,
            start_failure_policy: StartFailurePolicy::StopOnFirstFailureKeepStarted,
            private_file_guard: Some(false),
        };
        let outcome = manager.reload_catalog(empty_catalog).await.unwrap();
        assert_eq!(outcome.stopped, vec!["api".to_string()]);
        manager.close().await;
    }

    /// A removed service whose stop fails must not vanish from the catalog while it may still run.
    #[tokio::test]
    async fn reload_catalog_keeps_the_old_catalog_when_a_stop_fails() {
        let dir = tempfile::tempdir().unwrap();
        let catalog = ServiceCatalog {
            services: vec![tcp_service("api", free_port())],
            groups: HashMap::new(),
            group_tree: Vec::new(),
            compose_file: None,
            runtime_directory: None,
            start_failure_policy: StartFailurePolicy::StopOnFirstFailureKeepStarted,
            private_file_guard: Some(false),
        };
        let manager = bootstrap(HearthManagerOptions {
            runtime_directory: Some(dir.path().to_path_buf()),
            root: Some(PathBuf::from("/tmp")),
            catalog,
            event_capacity: None,
            log_tail_bytes: None,
            log_max_bytes: None,
            log_rotation_count: None,
            supervisor: None,
            shared: None,
        })
        .await
        .unwrap();
        // "Ready" with no process identity and no catalog `stop:` — there is nothing the daemon can
        // stop, so the stop fails.
        let timestamp = now();
        let ready = ServiceLifecycleState {
            actual_state: ActualServiceState::Ready,
            desired_state: crate::state::DesiredServiceState::Running,
            readiness: crate::state::ServiceReadiness::Ready,
            generation: 1,
            ..default_state_or(None, "api", &timestamp)
        };
        manager.set_service_state(ready).await;

        let empty_catalog = ServiceCatalog {
            services: vec![],
            groups: HashMap::new(),
            group_tree: Vec::new(),
            compose_file: None,
            runtime_directory: None,
            start_failure_policy: StartFailurePolicy::StopOnFirstFailureKeepStarted,
            private_file_guard: Some(false),
        };
        match manager.reload_catalog(empty_catalog).await {
            Err(ReloadError::StopFailed { failures, .. }) => assert_eq!(
                failures
                    .iter()
                    .map(|(id, _)| id.as_str())
                    .collect::<Vec<_>>(),
                vec!["api"]
            ),
            Err(other) => panic!("expected StopFailed, got {other}"),
            Ok(outcome) => panic!("reload must fail, stopped {:?}", outcome.stopped),
        }
        assert_eq!(manager.catalog().services.len(), 1, "the old catalog stays");
        manager.close().await;
    }

    #[tokio::test]
    async fn reload_request_ids_replay_and_conflict() {
        let dir = tempfile::tempdir().unwrap();
        let catalog = ServiceCatalog {
            services: vec![tcp_service("api", free_port())],
            groups: HashMap::new(),
            group_tree: Vec::new(),
            compose_file: None,
            runtime_directory: None,
            start_failure_policy: StartFailurePolicy::StopOnFirstFailureKeepStarted,
            private_file_guard: Some(false),
        };
        let manager = bootstrap(HearthManagerOptions {
            runtime_directory: Some(dir.path().to_path_buf()),
            root: Some(PathBuf::from("/tmp")),
            catalog,
            event_capacity: None,
            log_tail_bytes: None,
            log_max_bytes: None,
            log_rotation_count: None,
            supervisor: None,
            shared: None,
        })
        .await
        .unwrap();
        let client = reqwest::Client::new();
        let post = |body: Value| {
            client
                .post(format!("{}/v1/manager/reload", manager.base_url()))
                .bearer_auth(manager.bearer_token())
                .header("x-hearth-protocol", PROTOCOL_VERSION.to_string())
                .json(&body)
                .send()
        };

        // `startFailurePolicy` is optional.
        let next = json!({ "services": [], "groups": {} });
        let first = post(json!({ "requestId": "r1", "catalog": next }))
            .await
            .unwrap();
        assert_eq!(first.status(), 200);
        let first: Value = first.json().await.unwrap();
        assert_eq!(first["stopped"], json!([]));
        let replay: Value = post(json!({ "requestId": "r1", "catalog": next }))
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(replay, first);
        let conflict = post(
            json!({ "requestId": "r1", "catalog": { "services": [], "groups": { "g": [] } } }),
        )
        .await
        .unwrap();
        assert_eq!(conflict.status(), 409);
        manager.close().await;
    }

    /// Regression for a deadlock reachable from an ordinary restart: a service that is genuinely
    /// ready but carries a stale `desired_state: stopped` could never be started again — the
    /// operation still reported success, but the service stayed skipped forever.
    ///
    /// How it is reached: `manager stop` sets `desired_state: stopped` for everything, then the
    /// next daemon adopts an externally-owned docker/tailnet unit back as `ready` with that stale
    /// intent still attached. `start_selected_dag` checks `desired_state` before it checks whether
    /// the node is already ready, so it reported `Skipped: db (start cancelled)`.
    /// `queue_stopped_services_for_start` could not fix the intent either, because it only ever
    /// touched services whose actual state was `stopped`.
    #[tokio::test]
    async fn a_ready_service_with_a_stale_desired_state_is_not_skipped() {
        let db_port = free_port();
        let catalog = ServiceCatalog {
            services: vec![tcp_service("db", db_port)],
            groups: HashMap::new(),
            group_tree: Vec::new(),
            compose_file: None,
            runtime_directory: None,
            start_failure_policy: StartFailurePolicy::StopOnFirstFailureKeepStarted,
            private_file_guard: Some(false),
        };
        let dir = tempfile::tempdir().unwrap();
        let manager = bootstrap(HearthManagerOptions {
            runtime_directory: Some(dir.path().to_path_buf()),
            root: Some(PathBuf::from("/tmp")),
            catalog,
            event_capacity: None,
            log_tail_bytes: None,
            log_max_bytes: None,
            log_rotation_count: None,
            supervisor: None,
            shared: None,
        })
        .await
        .unwrap();

        let base = manager.base_url();
        let token = manager.bearer_token().to_string();
        let client = reqwest::Client::new();

        // Bring `db` up, then forge exactly the state a restart leaves behind: actually ready,
        // but with the intent still recorded as stopped.
        manager
            .supervisor()
            .start(&"db".to_string(), None)
            .await
            .unwrap();
        {
            let mut state = manager.state.lock().unwrap();
            let db = state.services.get_mut("db").expect("db present");
            assert_eq!(db.actual_state, ActualServiceState::Ready);
            db.desired_state = crate::state::DesiredServiceState::Stopped;
        }

        let accepted: Value = client
            .post(format!("{base}/v1/operations"))
            .bearer_auth(&token)
            .header("x-hearth-protocol", PROTOCOL_VERSION.to_string())
            .json(&json!({ "requestId": "req-stale-intent", "serviceId": "db", "action": "start" }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let operation_id = accepted["operation"]["id"].as_str().unwrap().to_string();
        let settled = wait_for_operation(&client, &base, &token, &operation_id).await;
        assert_eq!(settled["operation"]["status"], "succeeded", "{settled:?}");

        let states = manager.service_states();
        let state_of = |id: &str| {
            states
                .iter()
                .find(|s| s.service_id == id)
                .cloned()
                .expect("service present")
        };
        assert_eq!(
            state_of("db").actual_state,
            ActualServiceState::Ready,
            "a ready service with a stale desired_state must not be skipped"
        );

        manager.close().await;
    }

    /// `disabled: true` rejects direct lifecycle operations and is skipped by bulk-start.
    #[tokio::test]
    async fn disabled_services_reject_operations_and_skip_bulk_start() {
        let dir = tempfile::tempdir().unwrap();
        let mut off = tcp_service("off", free_port());
        off.disabled = true;
        let api = tcp_service("api", free_port());
        let catalog = ServiceCatalog {
            services: vec![off, api],
            groups: HashMap::new(),
            group_tree: Vec::new(),
            compose_file: None,
            runtime_directory: None,
            start_failure_policy: StartFailurePolicy::StopOnFirstFailureKeepStarted,
            private_file_guard: Some(false),
        };
        let manager = bootstrap(HearthManagerOptions {
            runtime_directory: Some(dir.path().to_path_buf()),
            root: Some(PathBuf::from("/tmp")),
            catalog,
            event_capacity: None,
            log_tail_bytes: None,
            log_max_bytes: None,
            log_rotation_count: None,
            supervisor: None,
            shared: None,
        })
        .await
        .unwrap();
        let base = manager.base_url();
        let token = manager.bearer_token().to_string();
        let client = reqwest::Client::new();
        let post = |path: String, body: Value| {
            let (base, token, client) = (base.clone(), token.clone(), client.clone());
            async move {
                client
                    .post(format!("{base}{path}"))
                    .bearer_auth(&token)
                    .header("x-hearth-protocol", PROTOCOL_VERSION.to_string())
                    .json(&body)
                    .send()
                    .await
                    .unwrap()
            }
        };

        let rejected = post(
            "/v1/operations".to_string(),
            json!({"requestId": "req-off-1", "serviceId": "off", "action": "start"}),
        )
        .await;
        assert_eq!(rejected.status(), 409);
        assert_eq!(
            rejected.json::<Value>().await.unwrap()["error"]["code"],
            "service_disabled"
        );

        for action in ["stop", "restart"] {
            let rejected = post("/v1/operations".to_string(), json!({"requestId": format!("req-off-{action}"), "serviceId": "off", "action": action})).await;
            assert_eq!(rejected.status(), 409, "{action}");
        }

        let empty = post(
            "/v1/operations/bulk-start".to_string(),
            json!({"requestId": "req-bulk-off", "targets": ["off"]}),
        )
        .await;
        assert_eq!(empty.status(), 400);
        assert_eq!(
            empty.json::<Value>().await.unwrap()["error"]["code"],
            "invalid_targets"
        );

        // Mixed targets: the disabled member is dropped, the rest start.
        let accepted = post(
            "/v1/operations/bulk-start".to_string(),
            json!({"requestId": "req-bulk-mixed", "targets": ["off", "api"]}),
        )
        .await;
        assert_eq!(accepted.status(), 202);
        let operation_id = accepted.json::<Value>().await.unwrap()["operation"]["id"]
            .as_str()
            .unwrap()
            .to_string();
        let settled = wait_for_operation(&client, &base, &token, &operation_id).await;
        assert_eq!(settled["operation"]["status"], "succeeded");
        assert_eq!(
            manager
                .service_states()
                .iter()
                .find(|s| s.service_id == "api")
                .unwrap()
                .actual_state,
            ActualServiceState::Ready
        );

        manager.close().await;
    }

    #[tokio::test]
    async fn serves_resolved_service_urls_over_http() {
        let mut api = tcp_service("api", free_port());
        api.urls = Some(vec![crate::catalog::ServiceUrl {
            url: "http://127.0.0.1:18080/app".into(),
            label: Some("app".into()),
            requires_running: Some(false),
        }]);
        let catalog = ServiceCatalog {
            services: vec![api],
            groups: HashMap::new(),
            group_tree: Vec::new(),
            compose_file: None,
            runtime_directory: None,
            start_failure_policy: StartFailurePolicy::StopOnFirstFailureKeepStarted,
            private_file_guard: Some(false),
        };
        let dir = tempfile::tempdir().unwrap();
        let manager = bootstrap(HearthManagerOptions {
            runtime_directory: Some(dir.path().to_path_buf()),
            root: Some(PathBuf::from("/tmp")),
            catalog,
            event_capacity: None,
            log_tail_bytes: None,
            log_max_bytes: None,
            log_rotation_count: None,
            supervisor: None,
            shared: None,
        })
        .await
        .unwrap();

        let body: Value = reqwest::Client::new()
            .get(format!("{}/v1/urls", manager.base_url()))
            .bearer_auth(manager.bearer_token())
            .header("x-hearth-protocol", PROTOCOL_VERSION.to_string())
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(
            body["urls"],
            json!([{ "serviceId": "api", "label": "app", "url": "http://127.0.0.1:18080/app", "requiresRunning": false }])
        );
        assert_eq!(body["unresolved"], json!([]));

        // Behind auth like every other /v1 route.
        let status = reqwest::Client::new()
            .get(format!("{}/v1/urls", manager.base_url()))
            .send()
            .await
            .unwrap()
            .status();
        assert_eq!(status, 401);
        manager.close().await;
    }
}
