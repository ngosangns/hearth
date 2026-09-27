//! `HearthManager` and its HTTP+SSE route table — ties together the event store
//! (`ManagerEventStore`), `OperationScheduler`, `CursorLogStore`, `AtomicStateStore`, the
//! lock-claim protocol and a `ProcessSupervisor`, behind an `axum` server on a loopback,
//! OS-assigned port.
use std::collections::{HashMap, VecDeque};
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};

use axum::body::Bytes;
use axum::extract::{Path as AxumPath, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::middleware::{self, Next};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures::future::{BoxFuture, FutureExt};
use serde_json::{json, Map, Value};
use tokio::net::TcpListener;
use tokio::sync::watch;
use uuid::Uuid;

use crate::catalog::{validate_catalog, ServiceCatalog, ServiceId, ServiceOwnership};
use crate::file_io::{create_file_io, FileIo};
use crate::platform::{is_supported_hearth_platform, unsupported_platform_message};
use crate::state::{
    ActualServiceState, LogSlice, ManagerInfo, ManagerMetadata, OperationError, OperationKind, PersistedManagerState, ServiceLifecycleState, ServiceOperationKind, PROTOCOL_VERSION, STATE_VERSION,
};
use crate::supervisor::engine::ACTIVE_STATES;
use crate::supervisor::types::{format_iso8601_millis, Host, SupervisorOptions};
use crate::supervisor::ProcessSupervisor;

use super::event_store::ManagerEventStore;
use super::lock::{claim_lock, prepare_owned_lock_release, read_lock_ownership_key, release_owned_lock, ClaimLockError, LockHandle};
use super::log_store::CursorLogStore;
use super::operations::{OperationInput, OperationScheduler, RequestIdConflict};
use super::state_store::AtomicStateStore;

const MANAGER_METADATA_VERSION: u32 = 1;

pub(crate) fn now() -> String {
    let millis = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as i64;
    format_iso8601_millis(millis)
}

/// The one `service.lifecycle` payload shape, published once per transition by
/// `HearthManager::set_service_state` and by the queued-start bookkeeping below.
fn lifecycle_event(state: &ServiceLifecycleState) -> Map<String, Value> {
    let mut data = Map::new();
    data.insert("serviceId".to_string(), json!(state.service_id));
    data.insert("actualState".to_string(), json!(state.actual_state.as_wire_str()));
    data.insert("readiness".to_string(), json!(state.readiness));
    data.insert("generation".to_string(), json!(state.generation));
    data.insert("operationId".to_string(), json!(state.current_operation_id));
    data
}

fn default_daemon_owned(catalog: &ServiceCatalog, service_id: &str) -> bool {
    !matches!(catalog.services.iter().find(|s| s.id == service_id).and_then(|s| s.ownership), Some(ServiceOwnership::External))
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
        Self { status, code: code.to_string(), message: message.into() }
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
        Self::new(StatusCode::CONFLICT, "request_id_conflict", "requestId is already used by a different operation")
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
    /// Set only for the smp daemon (`hearthd smp`) — enables the `/v1/shared/*` route surface.
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
    StopFailed { failures: Vec<(ServiceId, String)>, stopped: Vec<ServiceId> },
}

impl std::fmt::Display for ReloadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Closing => f.write_str("manager is shutting down"),
            Self::Invalid(errors) => f.write_str(&errors.join("; ")),
            Self::StopFailed { failures, stopped } => {
                let detail = failures.iter().map(|(id, error)| format!("{id}: {error}")).collect::<Vec<_>>().join("; ");
                let stopped_note = if stopped.is_empty() { String::new() } else { format!("; stopped before the failure: {}", stopped.join(", ")) };
                write!(f, "catalog not reloaded; removed services failed to stop ({detail}){stopped_note}")
            }
        }
    }
}

impl From<ReloadError> for ManagerHttpError {
    fn from(error: ReloadError) -> Self {
        match &error {
            ReloadError::Closing => Self::new(StatusCode::CONFLICT, "manager_closing", "Manager is shutting down"),
            ReloadError::Invalid(_) => Self::new(StatusCode::UNPROCESSABLE_ENTITY, "invalid_catalog", error.to_string()),
            ReloadError::StopFailed { .. } => Self::new(StatusCode::CONFLICT, "stop_failed", error.to_string()),
        }
    }
}

impl HearthManager {
    pub(crate) fn supervisor(&self) -> &Arc<ProcessSupervisor> {
        self.supervisor.get().expect("supervisor is set immediately after construction, before any other method can run")
    }

    pub fn info(&self) -> ManagerInfo {
        let metadata = self.metadata.lock().unwrap().clone().expect("manager has not started");
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
        let metadata = self.metadata.lock().unwrap().clone().expect("manager has not started");
        format!("http://127.0.0.1:{}", metadata.port)
    }

    pub fn catalog(&self) -> Arc<ServiceCatalog> {
        self.catalog.read().unwrap().clone()
    }

    fn lifecycle_generation(&self, service_id: &str) -> u64 {
        self.state.lock().unwrap().services.get(service_id).map(|s| s.generation).unwrap_or(0)
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
    pub async fn reload_catalog(&self, next_catalog: ServiceCatalog) -> Result<ReloadOutcome, ReloadError> {
        let _guard = self.catalog_reload_serial.lock().await;
        if self.closing.load(Ordering::SeqCst) {
            return Err(ReloadError::Closing);
        }
        let validation = validate_catalog(&next_catalog);
        if !validation.errors.is_empty() {
            return Err(ReloadError::Invalid(validation.errors));
        }
        let previous = self.catalog.read().unwrap().clone();
        let next_ids: std::collections::HashSet<&ServiceId> = next_catalog.services.iter().map(|s| &s.id).collect();
        let removed_ids: Vec<ServiceId> = previous.services.iter().filter(|s| !next_ids.contains(&s.id)).map(|s| s.id.clone()).collect();
        let changed: Vec<ServiceId> = previous
            .services
            .iter()
            .filter(|s| next_catalog.services.iter().find(|n| n.id == s.id).map(|n| n != *s).unwrap_or(false))
            .map(|s| s.id.clone())
            .collect();

        let mut stopped = Vec::new();
        let mut failed = Vec::new();
        for service_id in &removed_ids {
            if !default_daemon_owned(&previous, service_id) {
                continue;
            }
            let is_active = self.state.lock().unwrap().services.get(service_id).map(|s| ACTIVE_STATES.contains(&s.actual_state)).unwrap_or(false);
            if !is_active {
                continue;
            }
            match self.supervisor().stop(service_id, None).await {
                Ok(()) => stopped.push(service_id.clone()),
                Err(error) => failed.push((service_id.clone(), error.0)),
            }
        }
        if !failed.is_empty() {
            return Err(ReloadError::StopFailed { failures: failed, stopped });
        }
        let has_external = next_catalog.services.iter().any(|s| matches!(s.ownership, Some(ServiceOwnership::External)));
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

    /// Alias for `shutdown("refuse-if-active")`.
    pub async fn close(self: &Arc<Self>) {
        self.shutdown(false).await;
    }

    /// `stop_services = true` mirrors the TS `"stop-services"` mode; `false` covers both
    /// `"refuse-if-active"` (the caller is expected to have already checked for active services
    /// before calling this, same as the TS `shutdownRequest` handler does) and `"leave-services"`
    /// (shut down now, leave running services for the next daemon to re-adopt).
    pub async fn shutdown(self: &Arc<Self>, stop_services: bool) {
        self.closing.store(true, Ordering::SeqCst);
        self.operations.close_mutations();
        self.supervisor().begin_shutdown();
        self.operations.drain_services().await;
        if stop_services {
            self.supervisor().shutdown().await;
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
        let release_prepared = prepare_owned_lock_release(self.io.as_ref(), &self.lock, &self.token).await;
        self.events.publish("manager.stopped", json!({ "instanceId": self.instance_id }).as_object().unwrap().clone());
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
        self.catalog.read().unwrap().services.iter().map(|s| default_state_or(state.services.get(&s.id), &s.id, &timestamp)).collect()
    }
    fn service_state(&self, service_id: &ServiceId) -> Option<ServiceLifecycleState> {
        let catalog = self.catalog.read().unwrap();
        let definition = catalog.services.iter().find(|s| &s.id == service_id)?;
        let timestamp = now();
        let state = self.state.lock().unwrap();
        Some(default_state_or(state.services.get(&definition.id), &definition.id, &timestamp))
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

fn default_state_or(existing: Option<&ServiceLifecycleState>, service_id: &str, timestamp: &str) -> ServiceLifecycleState {
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

pub async fn bootstrap(options: HearthManagerOptions) -> Result<Arc<HearthManager>, BootstrapError> {
    let platform = crate::platform::current_platform();
    if !is_supported_hearth_platform(platform) {
        return Err(BootstrapError::Message(unsupported_platform_message(platform)));
    }
    let validation = validate_catalog(&options.catalog);
    if !validation.errors.is_empty() {
        return Err(BootstrapError::Message(format!("Invalid service catalog: {}", validation.errors.join("; "))));
    }
    let root = match options.root.clone() {
        Some(root) => root,
        None => std::env::current_dir().map_err(|e| BootstrapError::Message(format!("cannot resolve the current directory: {e}")))?,
    };
    let runtime_directory = options.runtime_directory.clone().unwrap_or_else(|| crate::paths::resolve_runtime_directory(&root, options.catalog.runtime_directory.as_deref()));
    let io: Arc<dyn FileIo> = Arc::from(create_file_io(options.catalog.private_file_guard != Some(false)));
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
    let lock = claim_lock(io.as_ref(), &runtime_directory, &bootstrap_metadata, &token, &http_client).await?;

    let events = ManagerEventStore::new(options.event_capacity, Some(bootstrap_metadata.instance_id.clone()));
    let operations = OperationScheduler::new(events.clone());
    let logs = CursorLogStore::new(io.clone(), runtime_directory.join("logs"), options.log_tail_bytes, options.log_max_bytes, options.log_rotation_count);
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
        state: Mutex::new(PersistedManagerState { version: STATE_VERSION, services: HashMap::new() }),
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

    let mut supervisor_options = options.supervisor.unwrap_or_else(|| crate::supervisor::default_adapters::default_supervisor_options(root.clone(), Some(runtime_directory.clone()), None));
    let manager_for_closing = manager.clone();
    supervisor_options.is_closing = Arc::new(move || manager_for_closing.closing.load(Ordering::SeqCst));
    let supervisor = ProcessSupervisor::new(manager.clone(), supervisor_options);
    let _ = manager.supervisor.set(supervisor);

    let bootstrap_result: Result<(), BootstrapError> = async {
        *manager.state.lock().unwrap() = manager.state_store.load();
        manager.supervisor().reconcile().await;

        let listener = TcpListener::bind("127.0.0.1:0").await.map_err(|e| BootstrapError::Message(e.to_string()))?;
        let port = listener.local_addr().map_err(|e| BootstrapError::Message(e.to_string()))?.port();
        let app = router(manager.clone());
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let final_metadata = ManagerMetadata { port, ..bootstrap_metadata.clone() };
        *manager.metadata.lock().unwrap() = Some(final_metadata.clone());
        let key = read_lock_ownership_key(io.as_ref(), &runtime_directory).ok_or_else(|| BootstrapError::Message("Missing ownership key".to_string()))?;
        io.write_file(&lock.metadata_path, &serde_json::to_string(&final_metadata).unwrap()).map_err(|e| BootstrapError::Message(e.to_string()))?;
        let proof = super::lock::create_lock_ownership_proof(&key, &final_metadata, &token);
        io.write_file(&lock.proof_path, &serde_json::to_string(&proof).unwrap()).map_err(|e| BootstrapError::Message(e.to_string()))?;
        manager.events.publish("manager.started", json!({ "instanceId": manager.instance_id }).as_object().unwrap().clone());

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

pub fn router(manager: Arc<HearthManager>) -> Router {
    let authorized_routes = Router::new()
        .route("/v1/manager", get(get_manager_info))
        .route("/v1/catalog", get(get_catalog))
        .route("/v1/urls", get(get_urls))
        .route("/v1/manager/reload", post(post_reload))
        .route("/v1/services", get(get_services))
        .route("/v1/operations", post(post_operation))
        .route("/v1/operations/bulk-start", post(post_bulk_start))
        .route("/v1/operations/:id", get(get_operation))
        .route("/v1/events", get(get_events))
        .route("/v1/events/stream", get(get_events_stream))
        .route("/v1/logs/:id", get(get_logs))
        .route("/v1/daemon/log", get(get_daemon_log))
        .route("/v1/manager/shutdown", post(post_shutdown));
    let authorized_routes = if manager.shared.is_some() {
        authorized_routes.merge(super::shared::shared_routes())
    } else {
        authorized_routes
    };
    let authorized_routes = authorized_routes.route_layer(middleware::from_fn_with_state(manager.clone(), require_authorized_and_protocol));

    // Unmatched paths and wrong methods get the same `{error:{code,message}}` envelope every other
    // failure uses. Axum's own 404/405 have an empty body, so a client parsing the error shape got
    // nothing to parse — the TS source returns a proper `not_found` for both.
    Router::new()
        .route("/healthz", get(get_healthz))
        .merge(authorized_routes)
        .fallback(|| async { ManagerHttpError::new(StatusCode::NOT_FOUND, "not_found", "Not found").into_response() })
        .method_not_allowed_fallback(|| async { ManagerHttpError::new(StatusCode::NOT_FOUND, "not_found", "Not found").into_response() })
        .with_state(manager)
}

async fn require_authorized_and_protocol(State(manager): State<Arc<HearthManager>>, headers: HeaderMap, request: axum::extract::Request, next: Next) -> Response {
    let expected = format!("Bearer {}", manager.token);
    if !headers.get("authorization").and_then(|v| v.to_str().ok()).is_some_and(|got| super::lock::constant_time_eq(got, &expected)) {
        return ManagerHttpError::new(StatusCode::UNAUTHORIZED, "unauthorized", "Bearer authentication is required").into_response();
    }
    let protocol_ok = headers.get("x-hearth-protocol").and_then(|v| v.to_str().ok()).and_then(|s| s.parse::<u32>().ok()) == Some(PROTOCOL_VERSION);
    if !protocol_ok {
        return ManagerHttpError::new(StatusCode::from_u16(426).unwrap(), "incompatible_protocol", format!("Expected protocol {PROTOCOL_VERSION}")).into_response();
    }
    next.run(request).await
}

fn is_authorized(manager: &HearthManager, headers: &HeaderMap) -> bool {
    let expected = format!("Bearer {}", manager.token);
    headers.get("authorization").and_then(|v| v.to_str().ok()).is_some_and(|got| super::lock::constant_time_eq(got, &expected))
}

async fn get_healthz(State(manager): State<Arc<HearthManager>>, headers: HeaderMap) -> Response {
    let mut body = json!({ "status": "ok", "protocolVersion": PROTOCOL_VERSION });
    if is_authorized(&manager, &headers) {
        body["instanceId"] = json!(manager.instance_id);
    }
    json_response(body, StatusCode::OK)
}

async fn get_manager_info(State(manager): State<Arc<HearthManager>>) -> Response {
    json_response(manager.info(), StatusCode::OK)
}

async fn get_catalog(State(manager): State<Arc<HearthManager>>) -> Response {
    json_response(json!({ "catalog": manager.catalog().as_ref() }), StatusCode::OK)
}

/// Every service URL in the current catalog with its placeholders resolved. Resolution may spawn
/// `tailscale` (cached), so it runs on the blocking pool rather than on a runtime worker.
async fn get_urls(State(manager): State<Arc<HearthManager>>) -> Response {
    let catalog = manager.catalog();
    let resolved = tokio::task::spawn_blocking(move || crate::catalog::resolve_service_urls(&catalog, super::service_urls::lookup_placeholder)).await;
    match resolved {
        Ok((urls, unresolved)) => json_response(json!({ "urls": urls, "unresolved": unresolved }), StatusCode::OK),
        Err(error) => ManagerHttpError::new(StatusCode::INTERNAL_SERVER_ERROR, "internal_error", error.to_string()).into_response(),
    }
}

async fn get_services(State(manager): State<Arc<HearthManager>>) -> Response {
    json_response(json!({ "services": manager.service_states() }), StatusCode::OK)
}

pub(crate) fn strict_body(bytes: &Bytes, allowed: &[&str], required: &[&str]) -> HttpResult<Map<String, Value>> {
    let value: Value = serde_json::from_slice(bytes).map_err(|_| ManagerHttpError::new(StatusCode::BAD_REQUEST, "invalid_json", "Request body must be JSON"))?;
    let Some(obj) = value.as_object() else {
        return Err(ManagerHttpError::new(StatusCode::BAD_REQUEST, "invalid_request", "Request schema is invalid"));
    };
    if obj.keys().any(|k| !allowed.contains(&k.as_str())) || required.iter().any(|k| !obj.contains_key(*k)) {
        return Err(ManagerHttpError::new(StatusCode::BAD_REQUEST, "invalid_request", "Request schema is invalid"));
    }
    Ok(obj.clone())
}

/// Every mutation refuses once shutdown has begun.
pub(crate) fn ensure_not_closing(manager: &HearthManager) -> HttpResult<()> {
    if manager.closing.load(Ordering::SeqCst) {
        return Err(ManagerHttpError::new(StatusCode::CONFLICT, "manager_closing", "Manager is shutting down"));
    }
    Ok(())
}

/// `requestId`: a non-empty string of at most 128 bytes, the same bound on every endpoint.
pub(crate) fn require_request_id(body: &Map<String, Value>) -> HttpResult<String> {
    body.get("requestId")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty() && s.len() <= 128)
        .map(str::to_string)
        .ok_or_else(|| ManagerHttpError::new(StatusCode::BAD_REQUEST, "invalid_request", "requestId must be a non-empty string of at most 128 bytes"))
}

/// An optional boolean flag (`killUnowned`, `force`): absent means `false`, anything but a bool is
/// a 400.
pub(crate) fn parse_bool_flag(body: &Map<String, Value>, key: &str) -> HttpResult<bool> {
    match body.get(key) {
        None | Some(Value::Bool(false)) => Ok(false),
        Some(Value::Bool(true)) => Ok(true),
        _ => Err(ManagerHttpError::new(StatusCode::BAD_REQUEST, "invalid_request", format!("{key} must be a boolean"))),
    }
}

fn is_service_id(catalog: &ServiceCatalog, value: &Value) -> Option<ServiceId> {
    let s = value.as_str()?;
    catalog.services.iter().any(|svc| svc.id == s).then(|| s.to_string())
}

async fn post_operation(State(manager): State<Arc<HearthManager>>, bytes: Bytes) -> HttpResult<Response> {
    ensure_not_closing(&manager)?;
    let body = strict_body(&bytes, &["requestId", "serviceId", "action", "killUnowned"], &["requestId", "serviceId", "action"])?;
    let request_id = require_request_id(&body)?;
    let catalog = manager.catalog();
    let Some(service_id) = body.get("serviceId").and_then(|v| is_service_id(&catalog, v)) else {
        return Err(ManagerHttpError::new(StatusCode::BAD_REQUEST, "invalid_service", "serviceId must be a catalog service"));
    };
    let action = match body.get("action").and_then(Value::as_str) {
        Some("start") => ServiceOperationKind::Start,
        Some("stop") => ServiceOperationKind::Stop,
        Some("restart") => ServiceOperationKind::Restart,
        Some("status") => ServiceOperationKind::Status,
        _ => return Err(ManagerHttpError::new(StatusCode::BAD_REQUEST, "invalid_action", "action must be start, stop, restart, or status")),
    };
    // A disabled service accepts reads (`status`) but no lifecycle action — `groups:` expands past
    // it too, so the only way to land here is an explicit serviceId.
    if action != ServiceOperationKind::Status && catalog.services.iter().find(|s| s.id == service_id).map(|s| s.disabled).unwrap_or(false) {
        return Err(ManagerHttpError::new(StatusCode::CONFLICT, "service_disabled", "service is disabled"));
    }
    let kill_unowned = parse_bool_flag(&body, "killUnowned")?;
    if kill_unowned && action != ServiceOperationKind::Start {
        return Err(ManagerHttpError::new(StatusCode::BAD_REQUEST, "invalid_request", "killUnowned only applies to a start action"));
    }
    let start_services: Vec<ServiceId> = if action == ServiceOperationKind::Start { vec![service_id.clone()] } else { Vec::new() };

    let input = OperationInput { request_id, kind: OperationKind::Service, service_id: Some(service_id.clone()), target_service_ids: None, action: Some(action) };
    if let Some(existing) = manager.operations.resolve_request(&input)? {
        return Ok(json_response(json!({ "operation": existing }), StatusCode::ACCEPTED));
    }

    let manager_for_execute = manager.clone();
    let start_services_for_execute = start_services.clone();
    let execute: super::operations::OperationExecute = Box::new(move |handle| {
        run_service_operation(manager_for_execute, handle, service_id.clone(), action, start_services_for_execute, kill_unowned)
    });
    let manager_for_rejected = manager.clone();
    let start_services_for_rejected = start_services.clone();
    let rejected: super::operations::OperationRejected = Box::new(move |handle| {
        let manager = manager_for_rejected;
        let start_services = start_services_for_rejected;
        async move {
            if !start_services.is_empty() {
                let operation_id = handle.lock().unwrap().id.clone();
                clear_queued_starts(&manager, &start_services, &operation_id).await;
            }
        }
        .boxed()
    });
    let operation = manager.operations.schedule(input, execute, Some(rejected))?;
    if action == ServiceOperationKind::Start {
        queue_stopped_services_for_start(&manager, &start_services, &operation.id).await;
    }
    Ok(json_response(json!({ "operation": operation }), StatusCode::ACCEPTED))
}

fn run_service_operation(manager: Arc<HearthManager>, handle: super::operations::OperationHandle, service_id: ServiceId, action: ServiceOperationKind, start_services: Vec<ServiceId>, kill_unowned: bool) -> BoxFuture<'static, Result<(), OperationError>> {
    async move {
        // `start_services` is `[service_id]` for a `start` action — routed through the same
        // `start_selected_dag` as bulk-start so `hearthd start <id>`, the TUI's start key and MCP
        // `manage start` all land on one code path.
        let result = match action {
            ServiceOperationKind::Start => start_selected_dag(&manager, &start_services, &handle, kill_unowned).await.map_err(|e| crate::supervisor::types::SupervisorError(e.message)),
            ServiceOperationKind::Stop => manager.supervisor().stop(&service_id, None).await,
            ServiceOperationKind::Restart => manager.supervisor().restart(&service_id, None).await,
            ServiceOperationKind::Status => manager.supervisor().status(&service_id).await,
        };
        if action == ServiceOperationKind::Start {
            // The real operation id, never an empty string: `clear_queued_starts` skips its
            // ownership check when the id is empty, which would let this operation reset
            // `queued-start` services belonging to a concurrent bulk-start and cancel it.
            let operation_id = handle.lock().unwrap().id.clone();
            clear_queued_starts(&manager, &start_services, &operation_id).await;
        }
        result.map_err(|e| OperationError { code: "operation_failed".to_string(), message: e.0 })
    }
    .boxed()
}

async fn queue_stopped_services_for_start(manager: &Arc<HearthManager>, service_ids: &[ServiceId], operation_id: &str) {
    let _guard = manager.lifecycle.lock().await;
    if manager.closed.load(Ordering::SeqCst) {
        return;
    }
    let timestamp = now();
    let mut state = manager.state.lock().unwrap();
    let mut changed = false;
    for service_id in service_ids {
        let previous = state.services.get(service_id).cloned().unwrap_or_else(|| default_state_or(None, service_id, &timestamp));
        if previous.actual_state != ActualServiceState::Stopped {
            // Already up, or mid-flight. Its actual state must not be disturbed — but the INTENT
            // still has to be recorded, because `start_selected_dag` skips any node whose
            // `desired_state` is not `running` ("start cancelled") before it ever checks whether
            // the node is already ready.
            //
            // Skipping this left a service that is genuinely ready but carries a stale
            // `desired_state: stopped` in a state it could never leave: `start_selected_dag`
            // skipped it and the operation still reported success — so nothing surfaced the
            // problem. Reached easily in practice, because `manager stop`
            // sets `desired_state: stopped` for everything and an adopted docker/tailnet unit then
            // comes back as `ready` on the next daemon start with that stale intent attached.
            if previous.desired_state != crate::state::DesiredServiceState::Running {
                let next = ServiceLifecycleState { desired_state: crate::state::DesiredServiceState::Running, updated_at: timestamp.clone(), ..previous };
                state.services.insert(service_id.clone(), next);
                changed = true;
            }
            continue;
        }
        let next = ServiceLifecycleState {
            desired_state: crate::state::DesiredServiceState::Running,
            actual_state: ActualServiceState::QueuedStart,
            readiness: crate::state::ServiceReadiness::Unknown,
            updated_at: timestamp.clone(),
            current_operation_id: Some(operation_id.to_string()),
            ..previous
        };
        manager.events.publish("service.lifecycle", lifecycle_event(&next));
        state.services.insert(service_id.clone(), next);
        changed = true;
    }
    if changed {
        drop(state);
        manager.persist();
    }
}

async fn clear_queued_starts(manager: &Arc<HearthManager>, service_ids: &[ServiceId], operation_id: &str) {
    let _guard = manager.lifecycle.lock().await;
    if manager.closed.load(Ordering::SeqCst) {
        return;
    }
    let timestamp = now();
    let mut state = manager.state.lock().unwrap();
    let mut changed = false;
    for service_id in service_ids {
        let Some(previous) = state.services.get(service_id).cloned() else { continue };
        if previous.actual_state != ActualServiceState::QueuedStart {
            continue;
        }
        // Always ownership-checked: only the operation that queued a service may un-queue it.
        // There is deliberately no "clear regardless" escape hatch — one used to exist for the
        // single-service start path and let it cancel a concurrent bulk-start's queued services.
        if previous.current_operation_id.as_deref() != Some(operation_id) {
            continue;
        }
        let next = ServiceLifecycleState {
            desired_state: crate::state::DesiredServiceState::Stopped,
            actual_state: ActualServiceState::Stopped,
            readiness: crate::state::ServiceReadiness::Unknown,
            updated_at: timestamp.clone(),
            exited_at: Some(timestamp.clone()),
            current_operation_id: None,
            ..previous
        };
        manager.events.publish("service.lifecycle", lifecycle_event(&next));
        state.services.insert(service_id.clone(), next);
        changed = true;
    }
    if changed {
        drop(state);
        manager.persist();
    }
}

async fn post_bulk_start(State(manager): State<Arc<HearthManager>>, bytes: Bytes) -> HttpResult<Response> {
    ensure_not_closing(&manager)?;
    let body = strict_body(&bytes, &["requestId", "targets", "killUnowned"], &["requestId", "targets"])?;
    let request_id = require_request_id(&body)?;
    let catalog = manager.catalog();
    let targets_value = body.get("targets").and_then(Value::as_array).cloned().unwrap_or_default();
    let mut targets = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut ok = !targets_value.is_empty();
    for t in &targets_value {
        match is_service_id(&catalog, t) {
            Some(id) if seen.insert(id.clone()) => targets.push(id),
            _ => {
                ok = false;
                break;
            }
        }
    }
    if !ok {
        return Err(ManagerHttpError::new(StatusCode::BAD_REQUEST, "invalid_targets", "targets must be a non-empty set of catalog services"));
    }
    // Disabled services are silent skips in a bulk start — the same way `groups:` expansion already
    // drops them. When nothing runnable remains, the request itself is what's wrong.
    targets.retain(|id| !catalog.services.iter().find(|s| &s.id == id).map(|s| s.disabled).unwrap_or(false));
    if targets.is_empty() {
        return Err(ManagerHttpError::new(StatusCode::BAD_REQUEST, "invalid_targets", "targets must include at least one enabled catalog service"));
    }
    let kill_unowned = parse_bool_flag(&body, "killUnowned")?;
    if targets.iter().any(|t| catalog.services.iter().find(|s| &s.id == t).map(|s| !s.profiles.run.is_verified()).unwrap_or(true)) {
        return Err(ManagerHttpError::new(StatusCode::CONFLICT, "unsupported_service", "The requested catalog service is not executable"));
    }
    let selected: Vec<ServiceId> = targets.clone();

    let input = OperationInput { request_id, kind: OperationKind::BulkStart, service_id: None, target_service_ids: Some(targets.clone()), action: None };
    if let Some(existing) = manager.operations.resolve_request(&input)? {
        return Ok(json_response(json!({ "operation": existing }), StatusCode::ACCEPTED));
    }
    let manager_for_execute = manager.clone();
    let selected_for_execute = selected.clone();
    let execute: super::operations::OperationExecute = Box::new(move |handle| {
        async move {
            let operation_id = handle.lock().unwrap().id.clone();
            let result = start_selected_dag(&manager_for_execute, &selected_for_execute, &handle, kill_unowned).await;
            clear_queued_starts(&manager_for_execute, &selected_for_execute, &operation_id).await;
            result
        }
        .boxed()
    });
    let manager_for_rejected = manager.clone();
    let selected_for_rejected = selected.clone();
    let rejected: super::operations::OperationRejected = Box::new(move |handle| {
        async move {
            let operation_id = handle.lock().unwrap().id.clone();
            clear_queued_starts(&manager_for_rejected, &selected_for_rejected, &operation_id).await;
        }
        .boxed()
    });
    let operation = manager.operations.schedule(input, execute, Some(rejected))?;
    queue_stopped_services_for_start(&manager, &selected, &operation.id).await;
    Ok(json_response(json!({ "operation": operation }), StatusCode::ACCEPTED))
}

/// Port of `startSelectedDag`: starts every selected service concurrently and independently — no
/// service waits on another.
async fn start_selected_dag(manager: &Arc<HearthManager>, selected: &[ServiceId], operation: &super::operations::OperationHandle, kill_unowned: bool) -> Result<(), OperationError> {
    /// Two distinct non-ready outcomes, not one `ready: bool`. Only `Failed` makes the operation
    /// fail: `Skipped` because a start was cancelled mid-flight is a normal result the TS source
    /// reports as success. Collapsing both into "not ready" made a cancelled start report the
    /// whole bulk start as failed.
    #[derive(Clone, PartialEq)]
    enum StartOutcome {
        Ready,
        Skipped,
        Failed(String),
    }

    struct StartResult {
        service_id: ServiceId,
        outcome: StartOutcome,
    }

    async fn start(manager: Arc<HearthManager>, operation: super::operations::OperationHandle, service_id_owned: ServiceId, kill_unowned: bool) -> StartResult {
        let desired_running = manager.state.lock().unwrap().services.get(&service_id_owned).map(|s| s.desired_state == crate::state::DesiredServiceState::Running).unwrap_or(false);
        if !desired_running {
            manager.operations.trace(&operation, &format!("Skipped: {service_id_owned} (start cancelled)"));
            return StartResult { service_id: service_id_owned, outcome: StartOutcome::Skipped };
        }
        let already_ready = manager
            .state
            .lock()
            .unwrap()
            .services
            .get(&service_id_owned)
            .map(|s| s.actual_state == ActualServiceState::Ready && s.readiness == crate::state::ServiceReadiness::Ready)
            .unwrap_or(false);
        if already_ready {
            manager.operations.trace(&operation, &format!("Ready: {service_id_owned} (already ready)"));
            return StartResult { service_id: service_id_owned, outcome: StartOutcome::Ready };
        }
        manager.operations.trace(&operation, &format!("Starting: {service_id_owned}"));
        let operation_id = operation.lock().unwrap().id.clone();
        match manager.supervisor().start_with_options(&service_id_owned, Some(operation_id), crate::supervisor::types::StartOptions { kill_unowned }).await {
            Ok(()) => {
                let settled = manager.state.lock().unwrap().services.get(&service_id_owned).map(|s| {
                    s.actual_state == ActualServiceState::Ready || (s.actual_state == ActualServiceState::RunningUnready && s.readiness_kind == Some(crate::state::ReadinessKind::Process))
                }).unwrap_or(false);
                if settled {
                    manager.operations.trace(&operation, &format!("Ready: {service_id_owned}"));
                    StartResult { service_id: service_id_owned, outcome: StartOutcome::Ready }
                } else {
                    manager.operations.trace(&operation, &format!("Failed: {service_id_owned} (did not become ready)"));
                    StartResult { service_id: service_id_owned, outcome: StartOutcome::Failed("did not become ready".to_string()) }
                }
            }
            Err(error) => {
                manager.operations.trace(&operation, &format!("Failed: {service_id_owned} ({})", error.0));
                StartResult { service_id: service_id_owned, outcome: StartOutcome::Failed(error.0) }
            }
        }
    }

    let results = futures::future::join_all(selected.iter().map(|service_id| start(manager.clone(), operation.clone(), service_id.clone(), kill_unowned))).await;
    let failures: Vec<&StartResult> = results.iter().filter(|r| matches!(r.outcome, StartOutcome::Failed(_))).collect();
    if !failures.is_empty() {
        let summary = failures.iter().map(|r| r.service_id.clone()).collect::<Vec<_>>().join(", ");
        // The first failure's own cause is appended, as the TS source does — without it the caller
        // only learns *which* service failed, never why.
        let cause = match &failures[0].outcome {
            StartOutcome::Failed(message) if !message.is_empty() => format!(" ({message})"),
            _ => String::new(),
        };
        return Err(OperationError { code: "operation_failed".to_string(), message: format!("Service startup failed: {summary}{cause}") });
    }
    Ok(())
}

async fn get_operation(State(manager): State<Arc<HearthManager>>, AxumPath(id): AxumPath<String>) -> Response {
    match manager.operations.get(&id) {
        Some(operation) => json_response(json!({ "operation": operation }), StatusCode::OK),
        None => ManagerHttpError::new(StatusCode::NOT_FOUND, "operation_not_found", "Operation not found").into_response(),
    }
}

fn parse_after(params: &HashMap<String, String>) -> HttpResult<Option<u64>> {
    match params.get("after") {
        None => Ok(None),
        Some(v) => v.parse::<u64>().map(Some).map_err(|_| ManagerHttpError::new(StatusCode::BAD_REQUEST, "invalid_cursor", "after must be a non-negative integer")),
    }
}

async fn get_events(State(manager): State<Arc<HearthManager>>, Query(params): Query<HashMap<String, String>>) -> Response {
    match parse_after(&params) {
        Ok(after) => {
            let replay = manager.events.replay(after, params.get("epoch").map(|s| s.as_str()));
            json_response(json!({ "epoch": replay.epoch, "reset": replay.reset, "events": replay.events, "latestSequence": replay.latest_sequence }), StatusCode::OK)
        }
        Err(error) => error.into_response(),
    }
}

async fn get_events_stream(State(manager): State<Arc<HearthManager>>, Query(params): Query<HashMap<String, String>>) -> Response {
    let after = match parse_after(&params) {
        Ok(a) => a,
        Err(error) => return error.into_response(),
    };
    let replay = manager.events.replay(after, params.get("epoch").map(|s| s.as_str()));

    const MAX_QUEUE_FRAMES: usize = 64;
    type SseItem = Result<Event, std::convert::Infallible>;
    let (tx, rx) = tokio::sync::mpsc::channel::<SseItem>(MAX_QUEUE_FRAMES);

    if replay.reset || replay.events.len() + 1 > MAX_QUEUE_FRAMES {
        let _ = tx.try_send(Ok(replay_event(&replay.epoch, true, replay.latest_sequence)));
        // Reset case is terminal — no live subscription, matching the TS "force a REST refetch"
        // behavior for a replay window that itself would overflow the queue.
        let stream = tokio_stream::wrappers::ReceiverStream::new(rx);
        return Sse::new(stream).keep_alive(KeepAlive::default()).into_response();
    }
    let _ = tx.try_send(Ok(replay_event(&replay.epoch, false, replay.latest_sequence)));
    for event in &replay.events {
        if tx.try_send(Ok(event_to_sse(event))).is_err() {
            break;
        }
    }

    // On overflow the stream is CLOSED, not silently thinned. A client that merely stops receiving
    // some frames has no way to know it missed them: it keeps applying deltas to state that is now
    // permanently wrong (the TUI's service list, the app's status). Closing makes it reconnect with
    // its cursor and take a `reset` replay, which is the whole point of having a cursor. This
    // mirrors the TS source's `stop()` on a full queue.
    //
    // Closing means dropping every `Sender`: the one the listener holds (taken out of the slot
    // below) and the one the watchdog holds. `Notify` is what lets the listener, which is a sync
    // closure, wake the async watchdog to do its half.
    let overflowed = Arc::new(tokio::sync::Notify::new());
    let overflowed_for_listener = overflowed.clone();
    let watchdog_tx = tx.clone();
    let sender_slot = Arc::new(std::sync::Mutex::new(Some(tx)));
    let listener_slot = sender_slot.clone();
    let unsubscribe = manager.events.subscribe(Arc::new(move |event| {
        let mut guard = listener_slot.lock().unwrap();
        let full = match guard.as_ref() {
            Some(sender) => sender.try_send(Ok(event_to_sse(event))).is_err(),
            None => return,
        };
        if full {
            guard.take();
            overflowed_for_listener.notify_one();
        }
    }));
    tokio::spawn(async move {
        tokio::select! {
            // The client hung up.
            _ = watchdog_tx.closed() => {}
            // We overflowed and are hanging up on the client.
            _ = overflowed.notified() => {}
        }
        unsubscribe();
        sender_slot.lock().unwrap().take();
        drop(watchdog_tx);
    });
    let stream = tokio_stream::wrappers::ReceiverStream::new(rx);
    Sse::new(stream).keep_alive(KeepAlive::default()).into_response()
}

fn event_to_sse(event: &crate::state::ManagerEvent) -> Event {
    Event::default().id(event.sequence.to_string()).event(event.event_type.clone()).data(serde_json::to_string(event).unwrap())
}

fn replay_event(epoch: &str, reset: bool, latest_sequence: u64) -> Event {
    Event::default().event("replay").data(json!({ "epoch": epoch, "reset": reset, "latestSequence": latest_sequence }).to_string())
}

async fn get_logs(State(manager): State<Arc<HearthManager>>, AxumPath(raw_service_id): AxumPath<String>, Query(params): Query<HashMap<String, String>>) -> Response {
    let catalog = manager.catalog();
    if !catalog.services.iter().any(|s| s.id == raw_service_id) {
        return ManagerHttpError::new(StatusCode::NOT_FOUND, "service_not_found", "Service is not in the catalog").into_response();
    }
    let cursor = match params.get("cursor") {
        None => None,
        Some(v) => match v.parse::<u64>() {
            Ok(n) => Some(n),
            Err(_) => return ManagerHttpError::new(StatusCode::BAD_REQUEST, "invalid_cursor", "cursor must be a non-negative integer").into_response(),
        },
    };
    let limit = match params.get("limit") {
        None => None,
        Some(v) => match v.parse::<u64>() {
            Ok(n) if n >= 1 => Some(n),
            _ => return ManagerHttpError::new(StatusCode::BAD_REQUEST, "invalid_limit", "limit must be a positive integer").into_response(),
        },
    };
    let generation = match params.get("generation") {
        None => None,
        Some(v) => match v.parse::<u64>() {
            Ok(n) => Some(n),
            Err(_) => return ManagerHttpError::new(StatusCode::BAD_REQUEST, "invalid_generation", "generation must be a non-negative integer").into_response(),
        },
    };
    let lifecycle_generation = manager.lifecycle_generation(&raw_service_id);
    let slice = manager.logs.read(&raw_service_id, cursor, limit, lifecycle_generation, generation).await;
    json_response(slice, StatusCode::OK)
}

/// `GET /v1/daemon/log` — the daemon's own `daemon.log` is a plain rotating file, not the
/// per-service `CursorLogStore`, so it gets its own route. Returns a `LogSlice`-shaped tail:
/// `reset` is always true (the whole tail is returned each poll — a diagnostic pane, not an
/// incremental cursor) and `nextCursor` carries the byte length so callers can detect rotation.
async fn get_daemon_log(State(manager): State<Arc<HearthManager>>, Query(params): Query<HashMap<String, String>>) -> HttpResult<Response> {
    let bytes = match params.get("bytes") {
        None => 131_072_u64,
        Some(v) => match v.parse::<u64>() {
            Ok(n) if (1..=1_048_576).contains(&n) => n,
            _ => return Err(ManagerHttpError::new(StatusCode::BAD_REQUEST, "invalid_bytes", "bytes must be an integer between 1 and 1048576")),
        },
    };
    let path = manager.runtime_directory.join(crate::daemon::DAEMON_LOG_NAME);
    let read = tokio::task::spawn_blocking(move || read_log_tail(&path, bytes))
        .await
        .map_err(|e| ManagerHttpError::new(StatusCode::INTERNAL_SERVER_ERROR, "internal_error", e.to_string()))?;
    let (size, tail, truncated) = read.map_err(|e| ManagerHttpError::new(StatusCode::INTERNAL_SERVER_ERROR, "daemon_log_unreadable", format!("could not read daemon.log: {e}")))?;
    let slice = LogSlice {
        service_id: "daemon".to_string(),
        generation: 0,
        cursor: 0,
        next_cursor: size,
        data: tail,
        reset: true,
        truncated,
    };
    Ok(json_response(slice, StatusCode::OK))
}

/// The last `bytes` of the file at `path` as `(file size, text, truncated)` — seeks to the tail
/// instead of reading the whole file on every poll. A missing file reads as empty.
fn read_log_tail(path: &std::path::Path, bytes: u64) -> std::io::Result<(u64, String, bool)> {
    let mut file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((0, String::new(), false)),
        Err(e) => return Err(e),
    };
    let size = file.metadata()?.len();
    let start = size.saturating_sub(bytes);
    file.seek(SeekFrom::Start(start))?;
    let mut raw = Vec::with_capacity((size - start) as usize);
    file.take(bytes).read_to_end(&mut raw)?;
    // Snap the tail start to a UTF-8 boundary — same rule as the app's LogController trim.
    let skip = if start > 0 { raw.iter().take_while(|b| (**b & 0b1100_0000) == 0b1000_0000).count() } else { 0 };
    Ok((size, String::from_utf8_lossy(&raw[skip..]).into_owned(), start > 0 || skip > 0))
}

async fn post_reload(State(manager): State<Arc<HearthManager>>, bytes: Bytes) -> HttpResult<Response> {
    ensure_not_closing(&manager)?;
    let body = strict_body(&bytes, &["requestId", "catalog"], &["requestId", "catalog"])?;
    let request_id = require_request_id(&body)?;
    let catalog_value = body.get("catalog").cloned().unwrap_or(Value::Null);
    let valid_shape = catalog_value.get("services").map(Value::is_array).unwrap_or(false)
        && catalog_value.get("groups").map(Value::is_object).unwrap_or(false)
        && catalog_value.get("startFailurePolicy").map(Value::is_string).unwrap_or(true);
    if !valid_shape {
        return Err(ManagerHttpError::new(StatusCode::BAD_REQUEST, "invalid_catalog", "catalog must be a ServiceCatalog: { services: [...], groups: {...}, startFailurePolicy? }"));
    }
    let catalog: ServiceCatalog = serde_json::from_value(catalog_value.clone()).map_err(|e| ManagerHttpError::new(StatusCode::BAD_REQUEST, "invalid_catalog", e.to_string()))?;
    // Serialized with the reload itself so a retry arriving mid-reload waits for, then replays, the
    // original answer.
    let _serial = manager.reload_requests_serial.lock().await;
    if let Some(replayed) = replay_reload(&manager, &request_id, &catalog_value)? {
        return Ok(json_response(replayed, StatusCode::OK));
    }
    let outcome = manager.reload_catalog(catalog).await?;
    let response = json!({ "stopped": outcome.stopped, "changed": outcome.changed });
    let mut recent = manager.reload_requests.lock().unwrap();
    if recent.len() >= RELOAD_REQUESTS_KEPT {
        recent.pop_front();
    }
    recent.push_back((request_id, catalog_value, response.clone()));
    Ok(json_response(response, StatusCode::OK))
}

/// How many recent reload `requestId`s are remembered for replay.
const RELOAD_REQUESTS_KEPT: usize = 32;

/// A reload `requestId` seen before: the same catalog replays the original response, a different
/// one is a conflict.
fn replay_reload(manager: &HearthManager, request_id: &str, catalog: &Value) -> HttpResult<Option<Value>> {
    let recent = manager.reload_requests.lock().unwrap();
    match recent.iter().find(|(id, _, _)| id == request_id) {
        Some((_, seen, response)) if seen == catalog => Ok(Some(response.clone())),
        Some(_) => Err(RequestIdConflict.into()),
        None => Ok(None),
    }
}

async fn post_shutdown(State(manager): State<Arc<HearthManager>>, bytes: Bytes) -> HttpResult<Response> {
    let body = strict_body(&bytes, &["requestId", "mode"], &["requestId"])?;
    let request_id = require_request_id(&body)?;
    let mode = match body.get("mode") {
        None => "refuse-if-active",
        Some(Value::String(s)) if matches!(s.as_str(), "refuse-if-active" | "stop-services" | "leave-services") => s.as_str(),
        _ => return Err(ManagerHttpError::new(StatusCode::BAD_REQUEST, "invalid_shutdown_mode", "mode must be refuse-if-active, stop-services, or leave-services")),
    };
    let stop_services = mode == "stop-services";
    let schedule_input = OperationInput { request_id, kind: OperationKind::ManagerShutdown, service_id: None, target_service_ids: None, action: None };
    if let Some(existing) = manager.operations.resolve_request(&schedule_input)? {
        return Ok(json_response(json!({ "operation": existing }), StatusCode::ACCEPTED));
    }
    ensure_not_closing(&manager)?;
    let non_terminal = [ActualServiceState::Stopped, ActualServiceState::QueuedStart, ActualServiceState::Failed, ActualServiceState::Orphaned, ActualServiceState::ExternallyOwned];
    let catalog = manager.catalog();
    let active = manager.service_states().into_iter().any(|s| default_daemon_owned(&catalog, &s.service_id) && !non_terminal.contains(&s.actual_state));
    // Only `refuse-if-active` guards. `leave-services` is the deliberate "restart the daemon, keep
    // the services" path (`hearthd manager restart`): daemon-owned processes are detached and
    // outlive this daemon, and the next one re-adopts them from their persisted identities — the
    // same thing a SIGTERM shutdown already does.
    if active && mode == "refuse-if-active" {
        return Err(ManagerHttpError::new(StatusCode::CONFLICT, "active_services", "Manager shutdown is refused while managed services are active"));
    }
    let manager_for_shutdown = manager.clone();
    tokio::spawn(async move {
        manager_for_shutdown.shutdown(stop_services).await;
    });
    let operation = manager.operations.schedule(schedule_input, Box::new(|_h| async { Ok(()) }.boxed()), None)?;
    Ok(json_response(json!({ "operation": operation }), StatusCode::ACCEPTED))
}

// =============================================================================================
// Real end-to-end test: a real bootstrap()'ed manager, a real spawned TCP-readiness service, real
// HTTP calls via reqwest against the real bound port — proof that every store (lock, state,
// events, operations, logs) and the supervisor are wired together behind the HTTP+SSE surface
// the clients (macOS app, CLI, TUI, MCP) use.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::{CommandSpec, ReadinessSpec, ServiceCommand, ServiceDefinition, ServiceKind, ServiceProfiles, ServiceRunProfile, StartFailurePolicy};
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
        }
    }

    async fn wait_for_operation(client: &reqwest::Client, base: &str, token: &str, id: &str) -> Value {
        for _ in 0..100 {
            let resp: Value = client.get(format!("{base}/v1/operations/{id}")).bearer_auth(token).header("x-hearth-protocol", "1").send().await.unwrap().json().await.unwrap();
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
        let health: Value = client.get(format!("{base}/healthz")).send().await.unwrap().json().await.unwrap();
        assert_eq!(health["status"], "ok");
        assert!(health.get("instanceId").is_none());
        let health_authed: Value = client.get(format!("{base}/healthz")).bearer_auth(&token).send().await.unwrap().json().await.unwrap();
        assert_eq!(health_authed["instanceId"], json!(manager.instance_id));

        // Every /v1 route requires auth + the protocol header.
        let unauthorized = client.get(format!("{base}/v1/services")).send().await.unwrap();
        assert_eq!(unauthorized.status(), 401);
        let no_protocol = client.get(format!("{base}/v1/services")).bearer_auth(&token).send().await.unwrap();
        assert_eq!(no_protocol.status(), 426);

        let get = |path: String| {
            let client = client.clone();
            let base = base.clone();
            let token = token.clone();
            async move { client.get(format!("{base}{path}")).bearer_auth(&token).header("x-hearth-protocol", "1").send().await.unwrap() }
        };

        let manager_info: Value = get("/v1/manager".to_string()).await.json().await.unwrap();
        assert_eq!(manager_info["protocolVersion"], 1);
        assert_eq!(manager_info["instanceId"], json!(manager.instance_id));

        let catalog_resp: Value = get("/v1/catalog".to_string()).await.json().await.unwrap();
        assert_eq!(catalog_resp["catalog"]["services"][0]["id"], "api");

        let services: Value = get("/v1/services".to_string()).await.json().await.unwrap();
        assert_eq!(services["services"][0]["actualState"], "stopped");

        // Start the service via POST /v1/operations, wait for the operation to settle, verify ready.
        let start_response: Value = client
            .post(format!("{base}/v1/operations"))
            .bearer_auth(&token)
            .header("x-hearth-protocol", "1")
            .json(&json!({"requestId": "req-start-1", "serviceId": "api", "action": "start"}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let operation_id = start_response["operation"]["id"].as_str().unwrap().to_string();
        let settled = wait_for_operation(&client, &base, &token, &operation_id).await;
        assert_eq!(settled["operation"]["status"], "succeeded");

        let services_after_start: Value = get("/v1/services".to_string()).await.json().await.unwrap();
        assert_eq!(services_after_start["services"][0]["actualState"], "ready");
        let pid = services_after_start["services"][0]["identity"]["pid"].as_i64().unwrap();
        assert!(nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid as i32), None).is_ok(), "the real spawned process should be alive");

        // GET /v1/logs/:id — service_not_found for an unknown id, ok (possibly empty) for a real one.
        let missing_log = get("/v1/logs/does-not-exist".to_string()).await;
        assert_eq!(missing_log.status(), 404);
        let log_slice: Value = get("/v1/logs/api".to_string()).await.json().await.unwrap();
        assert_eq!(log_slice["serviceId"], "api");

        // GET /v1/daemon/log — empty slice while no daemon.log exists, then a byte-bounded tail.
        let empty_daemon: Value = get("/v1/daemon/log".to_string()).await.json().await.unwrap();
        assert_eq!(empty_daemon["serviceId"], "daemon");
        assert_eq!(empty_daemon["data"], "");
        assert_eq!(empty_daemon["reset"], true);
        std::fs::write(dir.path().join("daemon.log"), "aa bb\ncc dd\n").unwrap();
        let daemon_log: Value = get("/v1/daemon/log?bytes=6".to_string()).await.json().await.unwrap();
        assert_eq!(daemon_log["data"], "cc dd\n");
        assert_eq!(daemon_log["truncated"], true);
        let bad_bytes = get("/v1/daemon/log?bytes=0".to_string()).await;
        assert_eq!(bad_bytes.status(), 400);

        // GET /v1/events — at least manager.started and some service.lifecycle events should exist.
        let events: Value = get("/v1/events".to_string()).await.json().await.unwrap();
        assert!(events["events"].as_array().unwrap().iter().any(|e| e["type"] == "manager.started"));
        assert!(events["events"].as_array().unwrap().iter().any(|e| e["type"] == "service.lifecycle"));

        // Duplicate requestId returns the same operation instead of starting a second one.
        let duplicate: Value = client
            .post(format!("{base}/v1/operations"))
            .bearer_auth(&token)
            .header("x-hearth-protocol", "1")
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
            .header("x-hearth-protocol", "1")
            .json(&json!({"requestId": "req-stop-1", "serviceId": "api", "action": "stop"}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        wait_for_operation(&client, &base, &token, stop_response["operation"]["id"].as_str().unwrap()).await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid as i32), None).is_err(), "the real process must be gone after stop");
        let services_after_stop: Value = get("/v1/services".to_string()).await.json().await.unwrap();
        assert_eq!(services_after_stop["services"][0]["actualState"], "stopped");

        // Shut down the manager itself; the server should stop accepting new connections afterward.
        let shutdown_response = client
            .post(format!("{base}/v1/manager/shutdown"))
            .bearer_auth(&token)
            .header("x-hearth-protocol", "1")
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

    /// `hearthd manager restart` shuts the daemon down with `leave-services`: the daemon goes away
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
        let manager = bootstrap(HearthManagerOptions { runtime_directory: Some(dir.path().to_path_buf()), root: Some(PathBuf::from("/tmp")), catalog, event_capacity: None, log_tail_bytes: None, log_max_bytes: None, log_rotation_count: None, supervisor: None, shared: None }).await.unwrap();
        manager.supervisor().start(&"api".to_string(), None).await.unwrap();
        let pid = match manager.service_states()[0].identity.clone().unwrap() {
            crate::state::ProcessIdentity::Posix(posix) => posix.pid,
            other => panic!("expected a posix identity, got {other:?}"),
        };
        let alive = || nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid as i32), None).is_ok();
        assert!(alive(), "the spawned service should be running before the shutdown");

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
                    .header("x-hearth-protocol", "1")
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
        assert_eq!(shutdown("refuse-if-active").await.status(), 409, "an active service must still refuse a plain shutdown");
        assert_eq!(shutdown("leave-services").await.status(), 202);

        tokio::time::timeout(Duration::from_secs(5), async {
            while !*completion.borrow() {
                completion.changed().await.ok();
            }
        })
        .await
        .expect("manager should finish shutting down");

        assert!(alive(), "leave-services must not stop the service's process");
        assert_eq!(manager.service_states()[0].actual_state, ActualServiceState::Ready, "the service must still be recorded as ready for the next daemon to re-adopt");

        // This test owns the process it spawned — nothing else will reap it.
        let _ = nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid as i32), nix::sys::signal::Signal::SIGTERM);
    }

    #[tokio::test]
    async fn unknown_route_is_404() {
        let dir = tempfile::tempdir().unwrap();
        let catalog = ServiceCatalog { services: vec![], groups: HashMap::new(), group_tree: Vec::new(), compose_file: None, runtime_directory: None, start_failure_policy: StartFailurePolicy::StopOnFirstFailureKeepStarted, private_file_guard: Some(false) };
        let manager = bootstrap(HearthManagerOptions { runtime_directory: Some(dir.path().to_path_buf()), root: Some(PathBuf::from("/tmp")), catalog, event_capacity: None, log_tail_bytes: None, log_max_bytes: None, log_rotation_count: None, supervisor: None, shared: None }).await.unwrap();
        let client = reqwest::Client::new();
        let response = client.get(format!("{}/v1/does-not-exist", manager.base_url())).bearer_auth(manager.bearer_token()).header("x-hearth-protocol", "1").send().await.unwrap();
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
        let manager = bootstrap(HearthManagerOptions { runtime_directory: Some(dir.path().to_path_buf()), root: Some(PathBuf::from("/tmp")), catalog, event_capacity: None, log_tail_bytes: None, log_max_bytes: None, log_rotation_count: None, supervisor: None, shared: None }).await.unwrap();
        manager.supervisor().start(&"api".to_string(), None).await.unwrap();
        assert_eq!(manager.service_states()[0].actual_state, ActualServiceState::Ready);

        let empty_catalog = ServiceCatalog { services: vec![], groups: HashMap::new(), group_tree: Vec::new(), compose_file: None, runtime_directory: None, start_failure_policy: StartFailurePolicy::StopOnFirstFailureKeepStarted, private_file_guard: Some(false) };
        let outcome = manager.reload_catalog(empty_catalog).await.unwrap();
        assert_eq!(outcome.stopped, vec!["api".to_string()]);
        manager.close().await;
    }

    /// A removed service whose stop fails must not vanish from the catalog while it may still run.
    #[tokio::test]
    async fn reload_catalog_keeps_the_old_catalog_when_a_stop_fails() {
        let dir = tempfile::tempdir().unwrap();
        let catalog = ServiceCatalog { services: vec![tcp_service("api", free_port())], groups: HashMap::new(), group_tree: Vec::new(), compose_file: None, runtime_directory: None, start_failure_policy: StartFailurePolicy::StopOnFirstFailureKeepStarted, private_file_guard: Some(false) };
        let manager = bootstrap(HearthManagerOptions { runtime_directory: Some(dir.path().to_path_buf()), root: Some(PathBuf::from("/tmp")), catalog, event_capacity: None, log_tail_bytes: None, log_max_bytes: None, log_rotation_count: None, supervisor: None, shared: None }).await.unwrap();
        // "Ready" with no process identity and no catalog `stop:` — there is nothing the daemon can
        // stop, so the stop fails.
        let timestamp = now();
        let ready = ServiceLifecycleState { actual_state: ActualServiceState::Ready, desired_state: crate::state::DesiredServiceState::Running, readiness: crate::state::ServiceReadiness::Ready, generation: 1, ..default_state_or(None, "api", &timestamp) };
        manager.set_service_state(ready).await;

        let empty_catalog = ServiceCatalog { services: vec![], groups: HashMap::new(), group_tree: Vec::new(), compose_file: None, runtime_directory: None, start_failure_policy: StartFailurePolicy::StopOnFirstFailureKeepStarted, private_file_guard: Some(false) };
        match manager.reload_catalog(empty_catalog).await {
            Err(ReloadError::StopFailed { failures, .. }) => assert_eq!(failures.iter().map(|(id, _)| id.as_str()).collect::<Vec<_>>(), vec!["api"]),
            Err(other) => panic!("expected StopFailed, got {other}"),
            Ok(outcome) => panic!("reload must fail, stopped {:?}", outcome.stopped),
        }
        assert_eq!(manager.catalog().services.len(), 1, "the old catalog stays");
        manager.close().await;
    }

    #[tokio::test]
    async fn reload_request_ids_replay_and_conflict() {
        let dir = tempfile::tempdir().unwrap();
        let catalog = ServiceCatalog { services: vec![tcp_service("api", free_port())], groups: HashMap::new(), group_tree: Vec::new(), compose_file: None, runtime_directory: None, start_failure_policy: StartFailurePolicy::StopOnFirstFailureKeepStarted, private_file_guard: Some(false) };
        let manager = bootstrap(HearthManagerOptions { runtime_directory: Some(dir.path().to_path_buf()), root: Some(PathBuf::from("/tmp")), catalog, event_capacity: None, log_tail_bytes: None, log_max_bytes: None, log_rotation_count: None, supervisor: None, shared: None }).await.unwrap();
        let client = reqwest::Client::new();
        let post = |body: Value| client.post(format!("{}/v1/manager/reload", manager.base_url())).bearer_auth(manager.bearer_token()).header("x-hearth-protocol", "1").json(&body).send();

        // `startFailurePolicy` is optional.
        let next = json!({ "services": [], "groups": {} });
        let first = post(json!({ "requestId": "r1", "catalog": next })).await.unwrap();
        assert_eq!(first.status(), 200);
        let first: Value = first.json().await.unwrap();
        assert_eq!(first["stopped"], json!([]));
        let replay: Value = post(json!({ "requestId": "r1", "catalog": next })).await.unwrap().json().await.unwrap();
        assert_eq!(replay, first);
        let conflict = post(json!({ "requestId": "r1", "catalog": { "services": [], "groups": { "g": [] } } })).await.unwrap();
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
        manager.supervisor().start(&"db".to_string(), None).await.unwrap();
        {
            let mut state = manager.state.lock().unwrap();
            let db = state.services.get_mut("db").expect("db present");
            assert_eq!(db.actual_state, ActualServiceState::Ready);
            db.desired_state = crate::state::DesiredServiceState::Stopped;
        }

        let accepted: Value = client
            .post(format!("{base}/v1/operations"))
            .bearer_auth(&token)
            .header("x-hearth-protocol", "1")
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
        let state_of = |id: &str| states.iter().find(|s| s.service_id == id).cloned().expect("service present");
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
        let catalog = ServiceCatalog { services: vec![off, api], groups: HashMap::new(), group_tree: Vec::new(), compose_file: None, runtime_directory: None, start_failure_policy: StartFailurePolicy::StopOnFirstFailureKeepStarted, private_file_guard: Some(false) };
        let manager = bootstrap(HearthManagerOptions { runtime_directory: Some(dir.path().to_path_buf()), root: Some(PathBuf::from("/tmp")), catalog, event_capacity: None, log_tail_bytes: None, log_max_bytes: None, log_rotation_count: None, supervisor: None, shared: None }).await.unwrap();
        let base = manager.base_url();
        let token = manager.bearer_token().to_string();
        let client = reqwest::Client::new();
        let post = |path: String, body: Value| {
            let (base, token, client) = (base.clone(), token.clone(), client.clone());
            async move {
                client.post(format!("{base}{path}")).bearer_auth(&token).header("x-hearth-protocol", "1").json(&body).send().await.unwrap()
            }
        };

        let rejected = post("/v1/operations".to_string(), json!({"requestId": "req-off-1", "serviceId": "off", "action": "start"})).await;
        assert_eq!(rejected.status(), 409);
        assert_eq!(rejected.json::<Value>().await.unwrap()["error"]["code"], "service_disabled");

        for action in ["stop", "restart"] {
            let rejected = post("/v1/operations".to_string(), json!({"requestId": format!("req-off-{action}"), "serviceId": "off", "action": action})).await;
            assert_eq!(rejected.status(), 409, "{action}");
        }

        let empty = post("/v1/operations/bulk-start".to_string(), json!({"requestId": "req-bulk-off", "targets": ["off"]})).await;
        assert_eq!(empty.status(), 400);
        assert_eq!(empty.json::<Value>().await.unwrap()["error"]["code"], "invalid_targets");

        // Mixed targets: the disabled member is dropped, the rest start.
        let accepted = post("/v1/operations/bulk-start".to_string(), json!({"requestId": "req-bulk-mixed", "targets": ["off", "api"]})).await;
        assert_eq!(accepted.status(), 202);
        let operation_id = accepted.json::<Value>().await.unwrap()["operation"]["id"].as_str().unwrap().to_string();
        let settled = wait_for_operation(&client, &base, &token, &operation_id).await;
        assert_eq!(settled["operation"]["status"], "succeeded");
        assert_eq!(manager.service_states().iter().find(|s| s.service_id == "api").unwrap().actual_state, ActualServiceState::Ready);

        manager.close().await;
    }

    #[tokio::test]
    async fn serves_resolved_service_urls_over_http() {
        let mut api = tcp_service("api", free_port());
        api.urls = Some(vec![crate::catalog::ServiceUrl { url: "http://127.0.0.1:18080/app".into(), label: Some("app".into()), requires_running: Some(false) }]);
        let catalog = ServiceCatalog { services: vec![api], groups: HashMap::new(), group_tree: Vec::new(), compose_file: None, runtime_directory: None, start_failure_policy: StartFailurePolicy::StopOnFirstFailureKeepStarted, private_file_guard: Some(false) };
        let dir = tempfile::tempdir().unwrap();
        let manager = bootstrap(HearthManagerOptions { runtime_directory: Some(dir.path().to_path_buf()), root: Some(PathBuf::from("/tmp")), catalog, event_capacity: None, log_tail_bytes: None, log_max_bytes: None, log_rotation_count: None, supervisor: None, shared: None }).await.unwrap();

        let body: Value = reqwest::Client::new()
            .get(format!("{}/v1/urls", manager.base_url()))
            .bearer_auth(manager.bearer_token())
            .header("x-hearth-protocol", "1")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(body["urls"], json!([{ "serviceId": "api", "label": "app", "url": "http://127.0.0.1:18080/app", "requiresRunning": false }]));
        assert_eq!(body["unresolved"], json!([]));

        // Behind auth like every other /v1 route.
        let status = reqwest::Client::new().get(format!("{}/v1/urls", manager.base_url())).send().await.unwrap().status();
        assert_eq!(status, 401);
        manager.close().await;
    }
}
