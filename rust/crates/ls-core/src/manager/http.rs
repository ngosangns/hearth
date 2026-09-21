//! Port of `LocalServicesManager` and its HTTP+SSE route table (`src/core/manager.ts`) — ties
//! together every store built so far (`ManagerEventStore`, `OperationScheduler`, `CursorLogStore`,
//! `AtomicStateStore`, the lock-claim protocol) plus a `ProcessSupervisor`, behind an `axum` server
//! on a loopback, OS-assigned port.
use std::collections::HashMap;
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
use futures::future::{BoxFuture, FutureExt, Shared};
use serde_json::{json, Map, Value};
use tokio::net::TcpListener;
use tokio::sync::watch;
use uuid::Uuid;

use crate::catalog::{dependency_levels, validate_catalog, ServiceCatalog, ServiceId, ServiceOwnership};
use crate::file_io::{create_file_io, FileIo};
use crate::platform::{is_supported_local_services_platform, unsupported_platform_message};
use crate::state::{
    ManagerInfo, ManagerMetadata, OperationError, OperationKind, PersistedManagerState, ServiceLifecycleState, ServiceOperationKind, PROTOCOL_VERSION, STATE_VERSION,
};
use crate::supervisor::engine::ACTIVE_STATES;
use crate::supervisor::types::{format_iso8601_millis, Host, SupervisorOptions};
use crate::supervisor::ProcessSupervisor;

use super::event_store::ManagerEventStore;
use super::lock::{claim_lock, prepare_owned_lock_release, read_lock_ownership_key, release_owned_lock, ClaimLockError, LockHandle};
use super::log_store::CursorLogStore;
use super::operations::{OperationInput, OperationScheduler};
use super::state_store::AtomicStateStore;

const MANAGER_METADATA_VERSION: u32 = 1;

fn now() -> String {
    let millis = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as i64;
    format_iso8601_millis(millis)
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
    fn new(status: StatusCode, code: &str, message: impl Into<String>) -> Self {
        Self { status, code: code.to_string(), message: message.into() }
    }
}
impl IntoResponse for ManagerHttpError {
    fn into_response(self) -> Response {
        let body = json!({ "error": { "code": self.code, "message": self.message } });
        (self.status, [("cache-control", "no-store")], Json(body)).into_response()
    }
}
type HttpResult<T> = Result<T, ManagerHttpError>;

fn json_response(body: impl serde::Serialize, status: StatusCode) -> Response {
    (status, [("cache-control", "no-store")], Json(body)).into_response()
}

// ---------------------------------------------------------------------------------------------
// LocalServicesManager
// ---------------------------------------------------------------------------------------------

pub struct LocalServicesManagerOptions {
    pub runtime_directory: Option<PathBuf>,
    pub root: Option<PathBuf>,
    pub catalog: ServiceCatalog,
    pub event_capacity: Option<usize>,
    pub log_tail_bytes: Option<u64>,
    pub log_max_bytes: Option<u64>,
    pub log_rotation_count: Option<usize>,
    pub supervisor: Option<SupervisorOptions>,
}

pub struct LocalServicesManager {
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
    closing: AtomicBool,
    lifecycle: tokio::sync::Mutex<()>,
    catalog_reload_serial: tokio::sync::Mutex<()>,
    supervisor: OnceLock<Arc<ProcessSupervisor>>,
    external_sync_task: Mutex<Option<tokio::task::JoinHandle<()>>>,
    shutdown_done: watch::Sender<bool>,
}

pub struct ReloadOutcome {
    pub stopped: Vec<ServiceId>,
    pub changed: Vec<ServiceId>,
}

impl LocalServicesManager {
    fn supervisor(&self) -> &Arc<ProcessSupervisor> {
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
    /// Serialized against itself (not `self.lifecycle`, which `supervisor.stop`'s own state writes
    /// run through — nesting into that from here would deadlock).
    pub async fn reload_catalog(&self, next_catalog: ServiceCatalog) -> Result<ReloadOutcome, Vec<String>> {
        let _guard = self.catalog_reload_serial.lock().await;
        if self.closing.load(Ordering::SeqCst) {
            return Err(vec!["manager is shutting down".to_string()]);
        }
        let validation = validate_catalog(&next_catalog);
        if !validation.errors.is_empty() {
            return Err(validation.errors);
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
        for service_id in &removed_ids {
            if !default_daemon_owned(&previous, service_id) {
                continue;
            }
            let is_active = self.state.lock().unwrap().services.get(service_id).map(|s| ACTIVE_STATES.contains(&s.actual_state)).unwrap_or(false);
            if !is_active {
                continue;
            }
            let _ = self.supervisor().stop(service_id, None).await;
            stopped.push(service_id.clone());
        }
        *self.catalog.write().unwrap() = Arc::new(next_catalog);
        let mut data = Map::new();
        data.insert("removed".to_string(), json!(removed_ids));
        data.insert("changed".to_string(), json!(changed));
        data.insert("stopped".to_string(), json!(stopped));
        self.events.publish("manager.catalog-reloaded", data);
        Ok(ReloadOutcome { stopped, changed })
    }

    pub fn shutdown_completion(&self) -> watch::Receiver<bool> {
        self.shutdown_done.subscribe()
    }

    /// Alias for `shutdown("refuse-if-active")`.
    pub async fn close(self: &Arc<Self>) {
        self.shutdown(false).await;
    }

    /// `stop_services = true` mirrors the TS `"stop-services"` mode; `false` is `"refuse-if-active"`
    /// (the caller is expected to have already checked for active services before calling this, same
    /// as the TS `shutdownRequest` handler does).
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
impl Host for LocalServicesManager {
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
        let mut state = self.state.lock().unwrap();
        state.services.insert(next.service_id.clone(), next.clone());
        let snapshot = PersistedManagerState { version: STATE_VERSION, services: state.services.clone() };
        drop(state);
        let _ = self.state_store.save(&snapshot);
        let mut data = Map::new();
        data.insert("serviceId".to_string(), json!(next.service_id));
        data.insert("actualState".to_string(), serde_json::to_value(next.actual_state).unwrap());
        data.insert("generation".to_string(), json!(next.generation));
        data.insert("operationId".to_string(), next.current_operation_id.map(Value::String).unwrap_or(Value::Null));
        self.events.publish("service.lifecycle", data);
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
        actual_state: crate::state::ActualServiceState::Stopped,
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

pub async fn bootstrap(options: LocalServicesManagerOptions) -> Result<Arc<LocalServicesManager>, BootstrapError> {
    let platform = crate::platform::current_platform();
    if !is_supported_local_services_platform(platform) {
        return Err(BootstrapError::Message(unsupported_platform_message(platform)));
    }
    let validation = validate_catalog(&options.catalog);
    if !validation.errors.is_empty() {
        return Err(BootstrapError::Message(format!("Invalid service catalog: {}", validation.errors.join("; "))));
    }
    let root = options.root.clone().unwrap_or_else(|| std::env::current_dir().unwrap());
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
    let state_store = AtomicStateStore::new(io.clone(), &runtime_directory);
    let (shutdown_done, _rx) = watch::channel(false);

    let manager = Arc::new(LocalServicesManager {
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
        catalog_reload_serial: tokio::sync::Mutex::new(()),
        supervisor: OnceLock::new(),
        external_sync_task: Mutex::new(None),
        shutdown_done,
    });

    let supervisor_options = match options.supervisor {
        Some(mut opts) => {
            let manager_for_closing = manager.clone();
            opts.is_closing = Arc::new(move || manager_for_closing.closing.load(Ordering::SeqCst));
            opts
        }
        None => {
            let mut opts = crate::supervisor::default_adapters::default_supervisor_options(root.clone(), Some(runtime_directory.clone()), None);
            let manager_for_closing = manager.clone();
            opts.is_closing = Arc::new(move || manager_for_closing.closing.load(Ordering::SeqCst));
            opts
        }
    };
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

        if manager.catalog().services.iter().any(|s| matches!(s.ownership, Some(ServiceOwnership::External))) {
            manager.supervisor().sync_external_services().await;
            let sup = manager.supervisor().clone();
            let handle = tokio::spawn(async move {
                loop {
                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                    sup.sync_external_services().await;
                }
            });
            *manager.external_sync_task.lock().unwrap() = Some(handle);
        }
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

pub fn router(manager: Arc<LocalServicesManager>) -> Router {
    let authorized_routes = Router::new()
        .route("/v1/manager", get(get_manager_info))
        .route("/v1/catalog", get(get_catalog))
        .route("/v1/manager/reload", post(post_reload))
        .route("/v1/services", get(get_services))
        .route("/v1/operations", post(post_operation))
        .route("/v1/operations/bulk-start", post(post_bulk_start))
        .route("/v1/operations/:id", get(get_operation))
        .route("/v1/events", get(get_events))
        .route("/v1/events/stream", get(get_events_stream))
        .route("/v1/logs/:id", get(get_logs))
        .route("/v1/manager/shutdown", post(post_shutdown))
        .route_layer(middleware::from_fn_with_state(manager.clone(), require_authorized_and_protocol));

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

async fn require_authorized_and_protocol(State(manager): State<Arc<LocalServicesManager>>, headers: HeaderMap, request: axum::extract::Request, next: Next) -> Response {
    let expected = format!("Bearer {}", manager.token);
    if !headers.get("authorization").and_then(|v| v.to_str().ok()).is_some_and(|got| super::lock::constant_time_eq(got, &expected)) {
        return ManagerHttpError::new(StatusCode::UNAUTHORIZED, "unauthorized", "Bearer authentication is required").into_response();
    }
    let protocol_ok = headers.get("x-local-services-protocol").and_then(|v| v.to_str().ok()).and_then(|s| s.parse::<u32>().ok()) == Some(PROTOCOL_VERSION);
    if !protocol_ok {
        return ManagerHttpError::new(StatusCode::from_u16(426).unwrap(), "incompatible_protocol", format!("Expected protocol {PROTOCOL_VERSION}")).into_response();
    }
    next.run(request).await
}

fn is_authorized(manager: &LocalServicesManager, headers: &HeaderMap) -> bool {
    let expected = format!("Bearer {}", manager.token);
    headers.get("authorization").and_then(|v| v.to_str().ok()).is_some_and(|got| super::lock::constant_time_eq(got, &expected))
}

async fn get_healthz(State(manager): State<Arc<LocalServicesManager>>, headers: HeaderMap) -> Response {
    let mut body = json!({ "status": "ok", "protocolVersion": PROTOCOL_VERSION });
    if is_authorized(&manager, &headers) {
        body["instanceId"] = json!(manager.instance_id);
    }
    json_response(body, StatusCode::OK)
}

async fn get_manager_info(State(manager): State<Arc<LocalServicesManager>>) -> Response {
    json_response(manager.info(), StatusCode::OK)
}

async fn get_catalog(State(manager): State<Arc<LocalServicesManager>>) -> Response {
    json_response(json!({ "catalog": manager.catalog().as_ref() }), StatusCode::OK)
}

async fn get_services(State(manager): State<Arc<LocalServicesManager>>) -> Response {
    json_response(json!({ "services": manager.service_states() }), StatusCode::OK)
}

fn strict_body(bytes: &Bytes, allowed: &[&str], required: &[&str]) -> HttpResult<Map<String, Value>> {
    let value: Value = serde_json::from_slice(bytes).map_err(|_| ManagerHttpError::new(StatusCode::BAD_REQUEST, "invalid_json", "Request body must be JSON"))?;
    let Some(obj) = value.as_object() else {
        return Err(ManagerHttpError::new(StatusCode::BAD_REQUEST, "invalid_request", "Request schema is invalid"));
    };
    if obj.keys().any(|k| !allowed.contains(&k.as_str())) || required.iter().any(|k| !obj.contains_key(*k)) {
        return Err(ManagerHttpError::new(StatusCode::BAD_REQUEST, "invalid_request", "Request schema is invalid"));
    }
    Ok(obj.clone())
}

fn is_service_id(catalog: &ServiceCatalog, value: &Value) -> Option<ServiceId> {
    let s = value.as_str()?;
    catalog.services.iter().any(|svc| svc.id == s).then(|| s.to_string())
}

async fn post_operation(State(manager): State<Arc<LocalServicesManager>>, bytes: Bytes) -> Response {
    match post_operation_inner(manager, bytes).await {
        Ok(response) => response,
        Err(error) => error.into_response(),
    }
}

async fn post_operation_inner(manager: Arc<LocalServicesManager>, bytes: Bytes) -> HttpResult<Response> {
    if manager.closing.load(Ordering::SeqCst) {
        return Err(ManagerHttpError::new(StatusCode::CONFLICT, "manager_closing", "Manager is shutting down"));
    }
    let body = strict_body(&bytes, &["requestId", "serviceId", "action"], &["requestId", "serviceId", "action"])?;
    let request_id = body.get("requestId").and_then(Value::as_str).filter(|s| !s.is_empty() && s.len() <= 128);
    let Some(request_id) = request_id else {
        return Err(ManagerHttpError::new(StatusCode::BAD_REQUEST, "invalid_request", "requestId must be a non-empty string"));
    };
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
    let start_services: Vec<ServiceId> = if action == ServiceOperationKind::Start {
        dependency_levels(&catalog, std::slice::from_ref(&service_id)).map_err(|e| ManagerHttpError::new(StatusCode::INTERNAL_SERVER_ERROR, "internal_error", e.to_string()))?.into_iter().flatten().collect()
    } else {
        Vec::new()
    };

    let input = OperationInput { request_id: request_id.to_string(), kind: OperationKind::Service, service_id: Some(service_id.clone()), target_service_ids: None, action: Some(action) };
    if let Some(existing) = manager.operations.resolve_request(&input).map_err(|_| ManagerHttpError::new(StatusCode::CONFLICT, "request_id_conflict", "requestId is already used by a different operation"))? {
        return Ok(json_response(json!({ "operation": existing }), StatusCode::ACCEPTED));
    }

    let manager_for_execute = manager.clone();
    let start_services_for_execute = start_services.clone();
    let execute: super::operations::OperationExecute = Box::new(move |handle| {
        run_service_operation(manager_for_execute, handle, service_id.clone(), action, start_services_for_execute)
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
    let operation = manager.operations.schedule(input, execute, Some(rejected)).map_err(|_| ManagerHttpError::new(StatusCode::CONFLICT, "request_id_conflict", "requestId is already used by a different operation"))?;
    if action == ServiceOperationKind::Start {
        queue_stopped_services_for_start(&manager, &start_services, &operation.id).await;
    }
    Ok(json_response(json!({ "operation": operation }), StatusCode::ACCEPTED))
}

fn run_service_operation(manager: Arc<LocalServicesManager>, handle: super::operations::OperationHandle, service_id: ServiceId, action: ServiceOperationKind, start_services: Vec<ServiceId>) -> BoxFuture<'static, Result<(), OperationError>> {
    async move {
        // A single-service `start` brings up that service's whole dependency closure, in dependency
        // order, exactly like bulk-start — `start_services` is that closure (computed by
        // `dependency_levels` at schedule time), not just `service_id`. Calling
        // `supervisor().start(&service_id)` directly here would spawn the service with its
        // dependencies still stopped: they would be marked `queued-start` by
        // `queue_stopped_services_for_start`, never started, then reset to `stopped` by the clear
        // pass below, and the service itself would fail readiness against a dependency that was
        // never brought up. `lsd start <id>`, the TUI's start key and MCP `manage start` all land
        // here, so that path has to match `bulk-start`'s.
        let result = match action {
            ServiceOperationKind::Start => start_selected_dag(&manager, &start_services, &handle).await.map_err(|e| crate::supervisor::types::SupervisorError(e.message)),
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

async fn queue_stopped_services_for_start(manager: &Arc<LocalServicesManager>, service_ids: &[ServiceId], operation_id: &str) {
    let _guard = manager.lifecycle.lock().await;
    if manager.closed.load(Ordering::SeqCst) {
        return;
    }
    let timestamp = now();
    let mut state = manager.state.lock().unwrap();
    let mut changed = false;
    for service_id in service_ids {
        let previous = state.services.get(service_id).cloned().unwrap_or_else(|| default_state_or(None, service_id, &timestamp));
        if previous.actual_state != crate::state::ActualServiceState::Stopped {
            // Already up, or mid-flight. Its actual state must not be disturbed — but the INTENT
            // still has to be recorded, because `start_selected_dag` skips any node whose
            // `desired_state` is not `running` ("start cancelled") before it ever checks whether
            // the node is already ready.
            //
            // Skipping this left a service that is genuinely ready but carries a stale
            // `desired_state: stopped` in a state it could never leave: the DAG skipped it, every
            // dependent was reported `Blocked`, and the operation still reported success — so
            // nothing surfaced the problem. Reached easily in practice, because `manager stop`
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
            actual_state: crate::state::ActualServiceState::QueuedStart,
            readiness: crate::state::ServiceReadiness::Unknown,
            updated_at: timestamp.clone(),
            current_operation_id: Some(operation_id.to_string()),
            ..previous
        };
        manager.events.publish(
            "service.lifecycle",
            json!({"serviceId": service_id, "actualState": "queued-start", "generation": next.generation, "operationId": operation_id}).as_object().unwrap().clone(),
        );
        state.services.insert(service_id.clone(), next);
        changed = true;
    }
    if changed {
        let snapshot = PersistedManagerState { version: STATE_VERSION, services: state.services.clone() };
        drop(state);
        let _ = manager.state_store.save(&snapshot);
    }
}

async fn clear_queued_starts(manager: &Arc<LocalServicesManager>, service_ids: &[ServiceId], operation_id: &str) {
    let _guard = manager.lifecycle.lock().await;
    if manager.closed.load(Ordering::SeqCst) {
        return;
    }
    let timestamp = now();
    let mut state = manager.state.lock().unwrap();
    let mut changed = false;
    for service_id in service_ids {
        let Some(previous) = state.services.get(service_id).cloned() else { continue };
        if previous.actual_state != crate::state::ActualServiceState::QueuedStart {
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
            actual_state: crate::state::ActualServiceState::Stopped,
            readiness: crate::state::ServiceReadiness::Unknown,
            updated_at: timestamp.clone(),
            exited_at: Some(timestamp.clone()),
            current_operation_id: None,
            ..previous
        };
        manager.events.publish("service.lifecycle", json!({"serviceId": service_id, "actualState": "stopped", "generation": next.generation}).as_object().unwrap().clone());
        state.services.insert(service_id.clone(), next);
        changed = true;
    }
    if changed {
        let snapshot = PersistedManagerState { version: STATE_VERSION, services: state.services.clone() };
        drop(state);
        let _ = manager.state_store.save(&snapshot);
    }
}

async fn post_bulk_start(State(manager): State<Arc<LocalServicesManager>>, bytes: Bytes) -> Response {
    match post_bulk_start_inner(manager, bytes).await {
        Ok(response) => response,
        Err(error) => error.into_response(),
    }
}

async fn post_bulk_start_inner(manager: Arc<LocalServicesManager>, bytes: Bytes) -> HttpResult<Response> {
    if manager.closing.load(Ordering::SeqCst) {
        return Err(ManagerHttpError::new(StatusCode::CONFLICT, "manager_closing", "Manager is shutting down"));
    }
    let body = strict_body(&bytes, &["requestId", "targets"], &["requestId", "targets"])?;
    let request_id = body.get("requestId").and_then(Value::as_str).filter(|s| !s.is_empty() && s.len() <= 128);
    let Some(request_id) = request_id else {
        return Err(ManagerHttpError::new(StatusCode::BAD_REQUEST, "invalid_request", "requestId must be a non-empty string"));
    };
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
    if targets.iter().any(|t| catalog.services.iter().find(|s| &s.id == t).map(|s| !s.profiles.run.is_verified()).unwrap_or(true)) {
        return Err(ManagerHttpError::new(StatusCode::CONFLICT, "unsupported_service", "The requested catalog service is not executable"));
    }
    let selected: Vec<ServiceId> = dependency_levels(&catalog, &targets).map_err(|e| ManagerHttpError::new(StatusCode::INTERNAL_SERVER_ERROR, "internal_error", e.to_string()))?.into_iter().flatten().collect();

    let input = OperationInput { request_id: request_id.to_string(), kind: OperationKind::BulkStart, service_id: None, target_service_ids: Some(targets.clone()), action: None };
    if let Some(existing) = manager.operations.resolve_request(&input).map_err(|_| ManagerHttpError::new(StatusCode::CONFLICT, "request_id_conflict", "requestId is already used by a different operation"))? {
        return Ok(json_response(json!({ "operation": existing }), StatusCode::ACCEPTED));
    }
    let manager_for_execute = manager.clone();
    let selected_for_execute = selected.clone();
    let execute: super::operations::OperationExecute = Box::new(move |handle| {
        async move {
            let operation_id = handle.lock().unwrap().id.clone();
            let result = start_selected_dag(&manager_for_execute, &selected_for_execute, &handle).await;
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
    let operation = manager.operations.schedule(input, execute, Some(rejected)).map_err(|_| ManagerHttpError::new(StatusCode::CONFLICT, "request_id_conflict", "requestId is already used by a different operation"))?;
    queue_stopped_services_for_start(&manager, &selected, &operation.id).await;
    Ok(json_response(json!({ "operation": operation }), StatusCode::ACCEPTED))
}

/// Port of `startSelectedDag`: brings up every selected service respecting dependency order, each
/// node's own future awaiting only its direct (also-selected) dependencies — so independent branches
/// of the graph proceed concurrently rather than waiting for an entire dependency "level" to finish.
async fn start_selected_dag(manager: &Arc<LocalServicesManager>, selected: &[ServiceId], operation: &super::operations::OperationHandle) -> Result<(), OperationError> {
    /// Three distinct non-ready outcomes, not one `ready: bool`. Only `Failed` makes the operation
    /// fail: a node `Blocked` behind a dependency that didn't come up, or `Skipped` because its
    /// start was cancelled mid-flight, is a normal result the TS source reports as success.
    /// Collapsing all three into "not ready" made a cancelled start report the whole bulk start as
    /// failed.
    #[derive(Clone, PartialEq)]
    enum StartOutcome {
        Ready,
        Blocked,
        Skipped,
        Failed(String),
    }

    #[derive(Clone)]
    struct StartResult {
        service_id: ServiceId,
        outcome: StartOutcome,
    }

    impl StartResult {
        fn is_ready(&self) -> bool {
            self.outcome == StartOutcome::Ready
        }
    }
    let selected_set: std::collections::HashSet<ServiceId> = selected.iter().cloned().collect();
    let catalog = manager.catalog();
    let mut tasks: HashMap<ServiceId, Shared<BoxFuture<'static, StartResult>>> = HashMap::new();
    for service_id in selected {
        let dependencies: Vec<ServiceId> = catalog.services.iter().find(|s| &s.id == service_id).and_then(|s| s.dependencies.clone()).unwrap_or_default().into_iter().filter(|d| selected_set.contains(d)).collect();
        let dependency_futures: Vec<Shared<BoxFuture<'static, StartResult>>> = dependencies.iter().map(|d| tasks[d].clone()).collect();
        let manager = manager.clone();
        let operation = operation.clone();
        let service_id_owned = service_id.clone();
        let fut: BoxFuture<'static, StartResult> = async move {
            let dependency_results = futures::future::join_all(dependency_futures).await;
            let unavailable: Vec<ServiceId> = dependency_results.iter().filter(|r| !r.is_ready()).map(|r| r.service_id.clone()).collect();
            if !unavailable.is_empty() {
                manager.operations.trace(&operation, &format!("Blocked: {service_id_owned} (dependencies not ready: {})", unavailable.join(", ")));
                return StartResult { service_id: service_id_owned, outcome: StartOutcome::Blocked };
            }
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
                .map(|s| s.actual_state == crate::state::ActualServiceState::Ready && s.readiness == crate::state::ServiceReadiness::Ready)
                .unwrap_or(false);
            if already_ready {
                manager.operations.trace(&operation, &format!("Ready: {service_id_owned} (already ready)"));
                return StartResult { service_id: service_id_owned, outcome: StartOutcome::Ready };
            }
            manager.operations.trace(&operation, &format!("Starting: {service_id_owned}"));
            let operation_id = operation.lock().unwrap().id.clone();
            match manager.supervisor().start(&service_id_owned, Some(operation_id)).await {
                Ok(()) => {
                    let settled = manager.state.lock().unwrap().services.get(&service_id_owned).map(|s| {
                        s.actual_state == crate::state::ActualServiceState::Ready || (s.actual_state == crate::state::ActualServiceState::RunningUnready && s.readiness_kind == Some(crate::state::ReadinessKind::Process))
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
        .boxed();
        tasks.insert(service_id.clone(), fut.shared());
    }
    let results = futures::future::join_all(selected.iter().map(|id| tasks[id].clone())).await;
    let failures: Vec<&StartResult> = results.iter().filter(|r| matches!(r.outcome, StartOutcome::Failed(_))).collect();
    if !failures.is_empty() {
        let summary = failures.iter().map(|r| r.service_id.clone()).collect::<Vec<_>>().join(", ");
        // The first failure's own cause is appended, as the TS source does — without it the caller
        // only learns *which* service failed, never why.
        let cause = match &failures[0].outcome {
            StartOutcome::Failed(message) if !message.is_empty() => format!(" ({message})"),
            _ => String::new(),
        };
        return Err(OperationError { code: "operation_failed".to_string(), message: format!("Dependency startup failed: {summary}{cause}") });
    }
    Ok(())
}

async fn get_operation(State(manager): State<Arc<LocalServicesManager>>, AxumPath(id): AxumPath<String>) -> Response {
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

async fn get_events(State(manager): State<Arc<LocalServicesManager>>, Query(params): Query<HashMap<String, String>>) -> Response {
    match parse_after(&params) {
        Ok(after) => {
            let replay = manager.events.replay(after, params.get("epoch").map(|s| s.as_str()));
            json_response(json!({ "epoch": replay.epoch, "reset": replay.reset, "events": replay.events, "latestSequence": replay.latest_sequence }), StatusCode::OK)
        }
        Err(error) => error.into_response(),
    }
}

async fn get_events_stream(State(manager): State<Arc<LocalServicesManager>>, Query(params): Query<HashMap<String, String>>) -> Response {
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

async fn get_logs(State(manager): State<Arc<LocalServicesManager>>, AxumPath(raw_service_id): AxumPath<String>, Query(params): Query<HashMap<String, String>>) -> Response {
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

async fn post_reload(State(manager): State<Arc<LocalServicesManager>>, bytes: Bytes) -> Response {
    match post_reload_inner(manager, bytes).await {
        Ok(response) => response,
        Err(error) => error.into_response(),
    }
}

async fn post_reload_inner(manager: Arc<LocalServicesManager>, bytes: Bytes) -> HttpResult<Response> {
    if manager.closing.load(Ordering::SeqCst) {
        return Err(ManagerHttpError::new(StatusCode::CONFLICT, "manager_closing", "Manager is shutting down"));
    }
    let body = strict_body(&bytes, &["requestId", "catalog"], &["requestId", "catalog"])?;
    let request_id = body.get("requestId").and_then(Value::as_str).filter(|s| !s.is_empty());
    if request_id.is_none() {
        return Err(ManagerHttpError::new(StatusCode::BAD_REQUEST, "invalid_request", "requestId must be a non-empty string"));
    }
    let catalog_value = body.get("catalog").cloned().unwrap_or(Value::Null);
    let valid_shape = catalog_value.get("services").map(Value::is_array).unwrap_or(false) && catalog_value.get("groups").map(Value::is_object).unwrap_or(false) && catalog_value.get("startFailurePolicy").map(Value::is_string).unwrap_or(false);
    if !valid_shape {
        return Err(ManagerHttpError::new(StatusCode::BAD_REQUEST, "invalid_catalog", "catalog must be a ServiceCatalog: { services: [...], groups: {...}, startFailurePolicy }"));
    }
    let catalog: ServiceCatalog = serde_json::from_value(catalog_value).map_err(|e| ManagerHttpError::new(StatusCode::BAD_REQUEST, "invalid_catalog", e.to_string()))?;
    match manager.reload_catalog(catalog).await {
        Ok(outcome) => Ok(json_response(json!({ "stopped": outcome.stopped, "changed": outcome.changed }), StatusCode::OK)),
        Err(errors) => Err(ManagerHttpError::new(StatusCode::UNPROCESSABLE_ENTITY, "invalid_catalog", errors.join("; "))),
    }
}

async fn post_shutdown(State(manager): State<Arc<LocalServicesManager>>, bytes: Bytes) -> Response {
    match post_shutdown_inner(manager, bytes).await {
        Ok(response) => response,
        Err(error) => error.into_response(),
    }
}

async fn post_shutdown_inner(manager: Arc<LocalServicesManager>, bytes: Bytes) -> HttpResult<Response> {
    let body = strict_body(&bytes, &["requestId", "mode"], &["requestId"])?;
    let request_id = body.get("requestId").and_then(Value::as_str).filter(|s| !s.is_empty());
    let Some(request_id) = request_id else {
        return Err(ManagerHttpError::new(StatusCode::BAD_REQUEST, "invalid_request", "requestId must be a non-empty string"));
    };
    let mode = match body.get("mode") {
        None => "refuse-if-active",
        Some(Value::String(s)) if s == "refuse-if-active" || s == "stop-services" => s.as_str(),
        _ => return Err(ManagerHttpError::new(StatusCode::BAD_REQUEST, "invalid_shutdown_mode", "mode must be refuse-if-active or stop-services")),
    };
    let stop_services = mode == "stop-services";
    let schedule_input = OperationInput { request_id: request_id.to_string(), kind: OperationKind::ManagerShutdown, service_id: None, target_service_ids: None, action: None };
    if let Some(existing) = manager.operations.resolve_request(&schedule_input).map_err(|_| ManagerHttpError::new(StatusCode::CONFLICT, "request_id_conflict", "requestId is already used by a different operation"))? {
        return Ok(json_response(json!({ "operation": existing }), StatusCode::ACCEPTED));
    }
    if manager.closing.load(Ordering::SeqCst) {
        return Err(ManagerHttpError::new(StatusCode::CONFLICT, "manager_closing", "Manager is shutting down"));
    }
    let non_terminal = ["stopped", "queued-start", "failed", "orphaned", "externally-owned"];
    let active = manager.service_states().into_iter().any(|s| default_daemon_owned(&manager.catalog(), &s.service_id) && !non_terminal.contains(&s.actual_state.as_wire_str()));
    if active && !stop_services {
        return Err(ManagerHttpError::new(StatusCode::CONFLICT, "active_services", "Manager shutdown is refused while managed services are active"));
    }
    let manager_for_shutdown = manager.clone();
    tokio::spawn(async move {
        manager_for_shutdown.shutdown(stop_services).await;
    });
    let operation = manager.operations.schedule(schedule_input, Box::new(|_h| async { Ok(()) }.boxed()), None).map_err(|_| ManagerHttpError::new(StatusCode::CONFLICT, "request_id_conflict", "requestId is already used by a different operation"))?;
    Ok(json_response(json!({ "operation": operation }), StatusCode::ACCEPTED))
}

// =============================================================================================
// Real end-to-end test: a real bootstrap()'ed manager, a real spawned TCP-readiness service, real
// HTTP calls via reqwest against the real bound port — the same category of test that closed out
// Phase 2 for the supervisor + its real adapters. This is the capstone proof that every store
// (lock, state, events, operations, logs) and the supervisor are correctly wired together behind
// the actual HTTP+SSE surface a client (the macOS app, a future Rust CLI/TUI/MCP) would use.
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
            dependencies: None,
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
        }
    }

    async fn wait_for_operation(client: &reqwest::Client, base: &str, token: &str, id: &str) -> Value {
        for _ in 0..100 {
            let resp: Value = client.get(format!("{base}/v1/operations/{id}")).bearer_auth(token).header("x-local-services-protocol", "1").send().await.unwrap().json().await.unwrap();
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
            compose_file: None,
            runtime_directory: None,
            start_failure_policy: StartFailurePolicy::StopOnFirstFailureKeepStarted,
            private_file_guard: Some(false),
        };
        let dir = tempfile::tempdir().unwrap();
        let manager = bootstrap(LocalServicesManagerOptions {
            runtime_directory: Some(dir.path().to_path_buf()),
            root: Some(PathBuf::from("/tmp")),
            catalog,
            event_capacity: None,
            log_tail_bytes: None,
            log_max_bytes: None,
            log_rotation_count: None,
            supervisor: None,
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
            async move { client.get(format!("{base}{path}")).bearer_auth(&token).header("x-local-services-protocol", "1").send().await.unwrap() }
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
            .header("x-local-services-protocol", "1")
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

        // GET /v1/events — at least manager.started and some service.lifecycle events should exist.
        let events: Value = get("/v1/events".to_string()).await.json().await.unwrap();
        assert!(events["events"].as_array().unwrap().iter().any(|e| e["type"] == "manager.started"));
        assert!(events["events"].as_array().unwrap().iter().any(|e| e["type"] == "service.lifecycle"));

        // Duplicate requestId returns the same operation instead of starting a second one.
        let duplicate: Value = client
            .post(format!("{base}/v1/operations"))
            .bearer_auth(&token)
            .header("x-local-services-protocol", "1")
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
            .header("x-local-services-protocol", "1")
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
            .header("x-local-services-protocol", "1")
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

    #[tokio::test]
    async fn unknown_route_is_404() {
        let dir = tempfile::tempdir().unwrap();
        let catalog = ServiceCatalog { services: vec![], groups: HashMap::new(), compose_file: None, runtime_directory: None, start_failure_policy: StartFailurePolicy::StopOnFirstFailureKeepStarted, private_file_guard: Some(false) };
        let manager = bootstrap(LocalServicesManagerOptions { runtime_directory: Some(dir.path().to_path_buf()), root: Some(PathBuf::from("/tmp")), catalog, event_capacity: None, log_tail_bytes: None, log_max_bytes: None, log_rotation_count: None, supervisor: None }).await.unwrap();
        let client = reqwest::Client::new();
        let response = client.get(format!("{}/v1/does-not-exist", manager.base_url())).bearer_auth(manager.bearer_token()).header("x-local-services-protocol", "1").send().await.unwrap();
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
            compose_file: None,
            runtime_directory: None,
            start_failure_policy: StartFailurePolicy::StopOnFirstFailureKeepStarted,
            private_file_guard: Some(false),
        };
        let manager = bootstrap(LocalServicesManagerOptions { runtime_directory: Some(dir.path().to_path_buf()), root: Some(PathBuf::from("/tmp")), catalog, event_capacity: None, log_tail_bytes: None, log_max_bytes: None, log_rotation_count: None, supervisor: None }).await.unwrap();
        manager.supervisor().start(&"api".to_string(), None).await.unwrap();
        assert_eq!(manager.service_states()[0].actual_state, crate::state::ActualServiceState::Ready);

        let empty_catalog = ServiceCatalog { services: vec![], groups: HashMap::new(), compose_file: None, runtime_directory: None, start_failure_policy: StartFailurePolicy::StopOnFirstFailureKeepStarted, private_file_guard: Some(false) };
        let outcome = manager.reload_catalog(empty_catalog).await.unwrap();
        assert_eq!(outcome.stopped, vec!["api".to_string()]);
        manager.close().await;
    }

    /// Regression test for a real port bug: `run_service_operation` used to call
    /// `supervisor().start(&service_id)` for the single requested service, while the TypeScript
    /// source runs the whole dependency closure through `startSelectedDag`. A single-service start
    /// therefore left every dependency stopped — visibly flipping them to `queued-start` and back
    /// to `stopped` — and the service itself failed readiness against a dependency that was never
    /// brought up. `lsd start <id>`, the TUI's start key and MCP `manage start` all post here, so
    /// this is the path almost every real start takes.
    #[tokio::test]
    async fn single_service_start_brings_up_its_dependencies() {
        let db_port = free_port();
        let api_port = free_port();
        let mut api = tcp_service("api", api_port);
        api.dependencies = Some(vec!["db".to_string()]);
        let catalog = ServiceCatalog {
            services: vec![tcp_service("db", db_port), api],
            groups: HashMap::new(),
            compose_file: None,
            runtime_directory: None,
            start_failure_policy: StartFailurePolicy::StopOnFirstFailureKeepStarted,
            private_file_guard: Some(false),
        };
        let dir = tempfile::tempdir().unwrap();
        let manager = bootstrap(LocalServicesManagerOptions {
            runtime_directory: Some(dir.path().to_path_buf()),
            root: Some(PathBuf::from("/tmp")),
            catalog,
            event_capacity: None,
            log_tail_bytes: None,
            log_max_bytes: None,
            log_rotation_count: None,
            supervisor: None,
        })
        .await
        .unwrap();

        let base = manager.base_url();
        let token = manager.bearer_token().to_string();
        let client = reqwest::Client::new();

        // Start ONLY `api`. Its dependency `db` is not named anywhere in the request.
        let accepted: Value = client
            .post(format!("{base}/v1/operations"))
            .bearer_auth(&token)
            .header("x-local-services-protocol", "1")
            .json(&json!({ "requestId": "req-dep-start", "serviceId": "api", "action": "start" }))
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
        assert_eq!(state_of("db").actual_state, crate::state::ActualServiceState::Ready, "the dependency must be started, not left stopped");
        assert_eq!(state_of("api").actual_state, crate::state::ActualServiceState::Ready);

        manager.close().await;
    }

    /// Regression for a deadlock reachable from an ordinary restart: a service that is genuinely
    /// ready but carries a stale `desired_state: stopped` could never be started, and permanently
    /// blocked every dependent — while the operation still reported success.
    ///
    /// How it is reached: `manager stop` sets `desired_state: stopped` for everything, then the
    /// next daemon adopts an externally-owned docker/tailnet unit back as `ready` with that stale
    /// intent still attached. `start_selected_dag` checks `desired_state` before it checks whether
    /// the node is already ready, so it reported `Skipped: <dep> (start cancelled)` and cascaded
    /// `Blocked` to everything downstream. `queue_stopped_services_for_start` could not fix the
    /// intent either, because it only ever touched services whose actual state was `stopped`.
    #[tokio::test]
    async fn a_ready_dependency_with_a_stale_desired_state_does_not_block_its_dependents() {
        let db_port = free_port();
        let api_port = free_port();
        let mut api = tcp_service("api", api_port);
        api.dependencies = Some(vec!["db".to_string()]);
        let catalog = ServiceCatalog {
            services: vec![tcp_service("db", db_port), api],
            groups: HashMap::new(),
            compose_file: None,
            runtime_directory: None,
            start_failure_policy: StartFailurePolicy::StopOnFirstFailureKeepStarted,
            private_file_guard: Some(false),
        };
        let dir = tempfile::tempdir().unwrap();
        let manager = bootstrap(LocalServicesManagerOptions {
            runtime_directory: Some(dir.path().to_path_buf()),
            root: Some(PathBuf::from("/tmp")),
            catalog,
            event_capacity: None,
            log_tail_bytes: None,
            log_max_bytes: None,
            log_rotation_count: None,
            supervisor: None,
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
            assert_eq!(db.actual_state, crate::state::ActualServiceState::Ready);
            db.desired_state = crate::state::DesiredServiceState::Stopped;
        }

        let accepted: Value = client
            .post(format!("{base}/v1/operations"))
            .bearer_auth(&token)
            .header("x-local-services-protocol", "1")
            .json(&json!({ "requestId": "req-stale-intent", "serviceId": "api", "action": "start" }))
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
        assert_eq!(state_of("db").actual_state, crate::state::ActualServiceState::Ready);
        assert_eq!(
            state_of("api").actual_state,
            crate::state::ActualServiceState::Ready,
            "a ready dependency with a stale desired_state must not block its dependent"
        );

        manager.close().await;
    }
}
