//! HTTP+SSE route table for `HearthManager`.
//! Child of `manager::http` so handlers can use private manager fields.
use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::sync::atomic::Ordering;
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Path as AxumPath, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::middleware::{self, Next};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use futures::future::{BoxFuture, FutureExt};
use serde_json::{json, Map, Value};

use crate::catalog::{ServiceCatalog, ServiceId};
use crate::state::{
    ActualServiceState, LogSlice, OperationError, OperationKind, ServiceLifecycleState,
    ServiceOperationKind, PROTOCOL_VERSION,
};

use crate::manager::operations::{OperationInput, RequestIdConflict};
use crate::supervisor::types::Host;

use super::{
    default_daemon_owned, default_state_or, json_response, lifecycle_event, now, HearthManager,
    HttpResult, ManagerHttpError,
};
use crate::manager::ShutdownMode;

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
        authorized_routes.merge(crate::manager::shared::shared_routes())
    } else {
        authorized_routes
    };
    let authorized_routes = authorized_routes.route_layer(middleware::from_fn_with_state(
        manager.clone(),
        require_authorized_and_protocol,
    ));

    // Unmatched paths and wrong methods get the same `{error:{code,message}}` envelope every other
    // failure uses. Axum's own 404/405 have an empty body, so a client parsing the error shape got
    // nothing to parse — the TS source returns a proper `not_found` for both.
    Router::new()
        .route("/healthz", get(get_healthz))
        .merge(authorized_routes)
        .fallback(|| async {
            ManagerHttpError::new(StatusCode::NOT_FOUND, "not_found", "Not found").into_response()
        })
        .method_not_allowed_fallback(|| async {
            ManagerHttpError::new(StatusCode::NOT_FOUND, "not_found", "Not found").into_response()
        })
        .with_state(manager)
}

async fn require_authorized_and_protocol(
    State(manager): State<Arc<HearthManager>>,
    headers: HeaderMap,
    request: axum::extract::Request,
    next: Next,
) -> Response {
    let expected = format!("Bearer {}", manager.token);
    if !headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|got| crate::manager::lock::constant_time_eq(got, &expected))
    {
        return ManagerHttpError::new(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "Bearer authentication is required",
        )
        .into_response();
    }
    let protocol_ok = headers
        .get("x-hearth-protocol")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u32>().ok())
        == Some(PROTOCOL_VERSION);
    if !protocol_ok {
        return ManagerHttpError::new(
            StatusCode::from_u16(426).unwrap(),
            "incompatible_protocol",
            format!("Expected protocol {PROTOCOL_VERSION}"),
        )
        .into_response();
    }
    next.run(request).await
}

fn is_authorized(manager: &HearthManager, headers: &HeaderMap) -> bool {
    let expected = format!("Bearer {}", manager.token);
    headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|got| crate::manager::lock::constant_time_eq(got, &expected))
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
    json_response(
        json!({ "catalog": manager.catalog().as_ref() }),
        StatusCode::OK,
    )
}

/// Every service URL in the current catalog with its placeholders resolved. Resolution may spawn
/// `tailscale` (cached), so it runs on the blocking pool rather than on a runtime worker.
async fn get_urls(State(manager): State<Arc<HearthManager>>) -> Response {
    let catalog = manager.catalog();
    let resolved = tokio::task::spawn_blocking(move || {
        crate::catalog::resolve_service_urls(
            &catalog,
            crate::manager::service_urls::lookup_placeholder,
        )
    })
    .await;
    match resolved {
        Ok((urls, unresolved)) => json_response(
            json!({ "urls": urls, "unresolved": unresolved }),
            StatusCode::OK,
        ),
        Err(error) => ManagerHttpError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            error.to_string(),
        )
        .into_response(),
    }
}

async fn get_services(State(manager): State<Arc<HearthManager>>) -> Response {
    json_response(
        json!({ "services": manager.service_states() }),
        StatusCode::OK,
    )
}

pub(crate) fn strict_body(
    bytes: &Bytes,
    allowed: &[&str],
    required: &[&str],
) -> HttpResult<Map<String, Value>> {
    let value: Value = serde_json::from_slice(bytes).map_err(|_| {
        ManagerHttpError::new(
            StatusCode::BAD_REQUEST,
            "invalid_json",
            "Request body must be JSON",
        )
    })?;
    let Some(obj) = value.as_object() else {
        return Err(ManagerHttpError::new(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "Request schema is invalid",
        ));
    };
    if obj.keys().any(|k| !allowed.contains(&k.as_str()))
        || required.iter().any(|k| !obj.contains_key(*k))
    {
        return Err(ManagerHttpError::new(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "Request schema is invalid",
        ));
    }
    Ok(obj.clone())
}

/// Every mutation refuses once shutdown has begun.
pub(crate) fn ensure_not_closing(manager: &HearthManager) -> HttpResult<()> {
    if manager.closing.load(Ordering::SeqCst) {
        return Err(ManagerHttpError::new(
            StatusCode::CONFLICT,
            "manager_closing",
            "Manager is shutting down",
        ));
    }
    Ok(())
}

/// `requestId`: a non-empty string of at most 128 bytes, the same bound on every endpoint.
pub(crate) fn require_request_id(body: &Map<String, Value>) -> HttpResult<String> {
    body.get("requestId")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty() && s.len() <= 128)
        .map(str::to_string)
        .ok_or_else(|| {
            ManagerHttpError::new(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                "requestId must be a non-empty string of at most 128 bytes",
            )
        })
}

/// An optional boolean flag (`killUnowned`, `force`): absent means `false`, anything but a bool is
/// a 400.
pub(crate) fn parse_bool_flag(body: &Map<String, Value>, key: &str) -> HttpResult<bool> {
    match body.get(key) {
        None | Some(Value::Bool(false)) => Ok(false),
        Some(Value::Bool(true)) => Ok(true),
        _ => Err(ManagerHttpError::new(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            format!("{key} must be a boolean"),
        )),
    }
}

fn is_service_id(catalog: &ServiceCatalog, value: &Value) -> Option<ServiceId> {
    let s = value.as_str()?;
    catalog
        .services
        .iter()
        .any(|svc| svc.id == s)
        .then(|| s.to_string())
}

async fn post_operation(
    State(manager): State<Arc<HearthManager>>,
    bytes: Bytes,
) -> HttpResult<Response> {
    ensure_not_closing(&manager)?;
    let body = strict_body(
        &bytes,
        &["requestId", "serviceId", "action", "killUnowned"],
        &["requestId", "serviceId", "action"],
    )?;
    let request_id = require_request_id(&body)?;
    let catalog = manager.catalog();
    let Some(service_id) = body
        .get("serviceId")
        .and_then(|v| is_service_id(&catalog, v))
    else {
        return Err(ManagerHttpError::new(
            StatusCode::BAD_REQUEST,
            "invalid_service",
            "serviceId must be a catalog service",
        ));
    };
    let action = match body.get("action").and_then(Value::as_str) {
        Some("start") => ServiceOperationKind::Start,
        Some("stop") => ServiceOperationKind::Stop,
        Some("restart") => ServiceOperationKind::Restart,
        Some("status") => ServiceOperationKind::Status,
        _ => {
            return Err(ManagerHttpError::new(
                StatusCode::BAD_REQUEST,
                "invalid_action",
                "action must be start, stop, restart, or status",
            ))
        }
    };
    // A disabled service accepts reads (`status`) but no lifecycle action — `groups:` expands past
    // it too, so the only way to land here is an explicit serviceId.
    if action != ServiceOperationKind::Status
        && catalog
            .services
            .iter()
            .find(|s| s.id == service_id)
            .map(|s| s.disabled)
            .unwrap_or(false)
    {
        return Err(ManagerHttpError::new(
            StatusCode::CONFLICT,
            "service_disabled",
            "service is disabled",
        ));
    }
    let kill_unowned = parse_bool_flag(&body, "killUnowned")?;
    if kill_unowned && action != ServiceOperationKind::Start {
        return Err(ManagerHttpError::new(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "killUnowned only applies to a start action",
        ));
    }
    let start_services: Vec<ServiceId> = if action == ServiceOperationKind::Start {
        vec![service_id.clone()]
    } else {
        Vec::new()
    };

    let input = OperationInput {
        request_id,
        kind: OperationKind::Service,
        service_id: Some(service_id.clone()),
        target_service_ids: None,
        action: Some(action),
    };
    if let Some(existing) = manager.operations.resolve_request(&input)? {
        return Ok(json_response(
            json!({ "operation": existing }),
            StatusCode::ACCEPTED,
        ));
    }

    let manager_for_execute = manager.clone();
    let start_services_for_execute = start_services.clone();
    let execute: crate::manager::operations::OperationExecute = Box::new(move |handle| {
        run_service_operation(
            manager_for_execute,
            handle,
            service_id.clone(),
            action,
            start_services_for_execute,
            kill_unowned,
        )
    });
    let manager_for_rejected = manager.clone();
    let start_services_for_rejected = start_services.clone();
    let rejected: crate::manager::operations::OperationRejected = Box::new(move |handle| {
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
    // The worker is spawned inside `schedule` and skips a start whose desired state is not yet
    // `running` ("start cancelled"), then reports success. Hold it until the desired state is
    // published, or a stopped service is left in `queued-start` after the operation already finished.
    let (operation, release) =
        manager
            .operations
            .schedule_when_released(input, execute, Some(rejected))?;
    if let Some(release) = release {
        if action == ServiceOperationKind::Start {
            queue_stopped_services_for_start(&manager, &start_services, &operation.id).await;
        }
        let _ = release.send(());
    } else if action == ServiceOperationKind::Start
        && matches!(
            operation.status,
            crate::state::OperationStatus::Succeeded | crate::state::OperationStatus::Failed
        )
    {
        // A duplicate request id that already settled between resolve and schedule. This call did
        // not queue, but clearing the finished operation's id drops a row it left in `queued-start`.
        // An in-flight duplicate (queued/running) keeps the row: that worker clears it on completion.
        clear_queued_starts(&manager, &start_services, &operation.id).await;
    }
    Ok(json_response(
        json!({ "operation": operation }),
        StatusCode::ACCEPTED,
    ))
}

fn run_service_operation(
    manager: Arc<HearthManager>,
    handle: crate::manager::operations::OperationHandle,
    service_id: ServiceId,
    action: ServiceOperationKind,
    start_services: Vec<ServiceId>,
    kill_unowned: bool,
) -> BoxFuture<'static, Result<(), OperationError>> {
    async move {
        // `start_services` is `[service_id]` for a `start` action — routed through the same
        // `start_selected_dag` as bulk-start so `hearth start <id>`, the TUI's start key and MCP
        // `manage start` all land on one code path.
        let result = match action {
            ServiceOperationKind::Start => {
                start_selected_dag(&manager, &start_services, &handle, kill_unowned)
                    .await
                    .map_err(|e| crate::supervisor::types::SupervisorError(e.message))
            }
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
        result.map_err(|e| OperationError {
            code: "operation_failed".to_string(),
            message: e.0,
        })
    }
    .boxed()
}

async fn queue_stopped_services_for_start(
    manager: &Arc<HearthManager>,
    service_ids: &[ServiceId],
    operation_id: &str,
) {
    let _guard = manager.lifecycle.lock().await;
    if manager.closed.load(Ordering::SeqCst) {
        return;
    }
    let timestamp = now();
    let mut state = manager.state.lock().unwrap();
    let mut changed = false;
    for service_id in service_ids {
        let previous = state
            .services
            .get(service_id)
            .cloned()
            .unwrap_or_else(|| default_state_or(None, service_id, &timestamp));
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
                let next = ServiceLifecycleState {
                    desired_state: crate::state::DesiredServiceState::Running,
                    updated_at: timestamp.clone(),
                    ..previous
                };
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
        manager
            .events
            .publish("service.lifecycle", lifecycle_event(&next));
        state.services.insert(service_id.clone(), next);
        changed = true;
    }
    if changed {
        drop(state);
        manager.persist();
    }
}

async fn clear_queued_starts(
    manager: &Arc<HearthManager>,
    service_ids: &[ServiceId],
    operation_id: &str,
) {
    let _guard = manager.lifecycle.lock().await;
    if manager.closed.load(Ordering::SeqCst) {
        return;
    }
    let timestamp = now();
    let mut state = manager.state.lock().unwrap();
    let mut changed = false;
    for service_id in service_ids {
        let Some(previous) = state.services.get(service_id).cloned() else {
            continue;
        };
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
        manager
            .events
            .publish("service.lifecycle", lifecycle_event(&next));
        state.services.insert(service_id.clone(), next);
        changed = true;
    }
    if changed {
        drop(state);
        manager.persist();
    }
}

async fn post_bulk_start(
    State(manager): State<Arc<HearthManager>>,
    bytes: Bytes,
) -> HttpResult<Response> {
    ensure_not_closing(&manager)?;
    let body = strict_body(
        &bytes,
        &["requestId", "targets", "killUnowned"],
        &["requestId", "targets"],
    )?;
    let request_id = require_request_id(&body)?;
    let catalog = manager.catalog();
    let targets_value = body
        .get("targets")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
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
        return Err(ManagerHttpError::new(
            StatusCode::BAD_REQUEST,
            "invalid_targets",
            "targets must be a non-empty set of catalog services",
        ));
    }
    // Disabled services are silent skips in a bulk start — the same way `groups:` expansion already
    // drops them. When nothing runnable remains, the request itself is what's wrong.
    targets.retain(|id| {
        !catalog
            .services
            .iter()
            .find(|s| &s.id == id)
            .map(|s| s.disabled)
            .unwrap_or(false)
    });
    if targets.is_empty() {
        return Err(ManagerHttpError::new(
            StatusCode::BAD_REQUEST,
            "invalid_targets",
            "targets must include at least one enabled catalog service",
        ));
    }
    let kill_unowned = parse_bool_flag(&body, "killUnowned")?;
    if targets.iter().any(|t| {
        catalog
            .services
            .iter()
            .find(|s| &s.id == t)
            .map(|s| !s.profiles.run.is_verified())
            .unwrap_or(true)
    }) {
        return Err(ManagerHttpError::new(
            StatusCode::CONFLICT,
            "unsupported_service",
            "The requested catalog service is not executable",
        ));
    }
    let selected: Vec<ServiceId> = targets.clone();

    let input = OperationInput {
        request_id,
        kind: OperationKind::BulkStart,
        service_id: None,
        target_service_ids: Some(targets.clone()),
        action: None,
    };
    if let Some(existing) = manager.operations.resolve_request(&input)? {
        return Ok(json_response(
            json!({ "operation": existing }),
            StatusCode::ACCEPTED,
        ));
    }
    let manager_for_execute = manager.clone();
    let selected_for_execute = selected.clone();
    let execute: crate::manager::operations::OperationExecute = Box::new(move |handle| {
        async move {
            let operation_id = handle.lock().unwrap().id.clone();
            let result = start_selected_dag(
                &manager_for_execute,
                &selected_for_execute,
                &handle,
                kill_unowned,
            )
            .await;
            clear_queued_starts(&manager_for_execute, &selected_for_execute, &operation_id).await;
            result
        }
        .boxed()
    });
    let manager_for_rejected = manager.clone();
    let selected_for_rejected = selected.clone();
    let rejected: crate::manager::operations::OperationRejected = Box::new(move |handle| {
        async move {
            let operation_id = handle.lock().unwrap().id.clone();
            clear_queued_starts(&manager_for_rejected, &selected_for_rejected, &operation_id).await;
        }
        .boxed()
    });
    let (operation, release) =
        manager
            .operations
            .schedule_when_released(input, execute, Some(rejected))?;
    if let Some(release) = release {
        queue_stopped_services_for_start(&manager, &selected, &operation.id).await;
        let _ = release.send(());
    } else if matches!(
        operation.status,
        crate::state::OperationStatus::Succeeded | crate::state::OperationStatus::Failed
    ) {
        clear_queued_starts(&manager, &selected, &operation.id).await;
    }
    Ok(json_response(
        json!({ "operation": operation }),
        StatusCode::ACCEPTED,
    ))
}

/// Port of `startSelectedDag`: starts every selected service concurrently and independently — no
/// service waits on another.
async fn start_selected_dag(
    manager: &Arc<HearthManager>,
    selected: &[ServiceId],
    operation: &crate::manager::operations::OperationHandle,
    kill_unowned: bool,
) -> Result<(), OperationError> {
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

    async fn start(
        manager: Arc<HearthManager>,
        operation: crate::manager::operations::OperationHandle,
        service_id_owned: ServiceId,
        kill_unowned: bool,
    ) -> StartResult {
        let desired_running = manager
            .state
            .lock()
            .unwrap()
            .services
            .get(&service_id_owned)
            .map(|s| s.desired_state == crate::state::DesiredServiceState::Running)
            .unwrap_or(false);
        if !desired_running {
            manager.operations.trace(
                &operation,
                &format!("Skipped: {service_id_owned} (start cancelled)"),
            );
            return StartResult {
                service_id: service_id_owned,
                outcome: StartOutcome::Skipped,
            };
        }
        let already_ready = manager
            .state
            .lock()
            .unwrap()
            .services
            .get(&service_id_owned)
            .map(|s| {
                s.actual_state == ActualServiceState::Ready
                    && s.readiness == crate::state::ServiceReadiness::Ready
            })
            .unwrap_or(false);
        if already_ready {
            manager.operations.trace(
                &operation,
                &format!("Ready: {service_id_owned} (already ready)"),
            );
            return StartResult {
                service_id: service_id_owned,
                outcome: StartOutcome::Ready,
            };
        }
        manager
            .operations
            .trace(&operation, &format!("Starting: {service_id_owned}"));
        let operation_id = operation.lock().unwrap().id.clone();
        match manager
            .supervisor()
            .start_with_options(
                &service_id_owned,
                Some(operation_id),
                crate::supervisor::types::StartOptions { kill_unowned },
            )
            .await
        {
            Ok(()) => {
                let settled = manager
                    .state
                    .lock()
                    .unwrap()
                    .services
                    .get(&service_id_owned)
                    .and_then(|s| {
                        // `succeeded` is the success state of `readiness: exit`. Treating only `ready`
                        // as settled made a finished job report "did not become ready". A one-time
                        // command never becomes `ready`; its trace says succeeded.
                        // `running-unready` is also settled: the process is up and the probe loop
                        // is running. Waiting for the first passing probe has no deadline.
                        if s.actual_state == ActualServiceState::Succeeded {
                            Some("Succeeded")
                        } else if s.actual_state == ActualServiceState::Ready {
                            Some("Ready")
                        } else if s.actual_state == ActualServiceState::RunningUnready {
                            Some("Running")
                        } else {
                            None
                        }
                    });
                if let Some(label) = settled {
                    manager
                        .operations
                        .trace(&operation, &format!("{label}: {service_id_owned}"));
                    StartResult {
                        service_id: service_id_owned,
                        outcome: StartOutcome::Ready,
                    }
                } else {
                    manager.operations.trace(
                        &operation,
                        &format!("Failed: {service_id_owned} (did not become ready)"),
                    );
                    StartResult {
                        service_id: service_id_owned,
                        outcome: StartOutcome::Failed("did not become ready".to_string()),
                    }
                }
            }
            Err(error) => {
                manager.operations.trace(
                    &operation,
                    &format!("Failed: {service_id_owned} ({})", error.0),
                );
                StartResult {
                    service_id: service_id_owned,
                    outcome: StartOutcome::Failed(error.0),
                }
            }
        }
    }

    let results = futures::future::join_all(selected.iter().map(|service_id| {
        start(
            manager.clone(),
            operation.clone(),
            service_id.clone(),
            kill_unowned,
        )
    }))
    .await;
    let failures: Vec<&StartResult> = results
        .iter()
        .filter(|r| matches!(r.outcome, StartOutcome::Failed(_)))
        .collect();
    if !failures.is_empty() {
        let summary = failures
            .iter()
            .map(|r| r.service_id.clone())
            .collect::<Vec<_>>()
            .join(", ");
        // The first failure's own cause is appended, as the TS source does — without it the caller
        // only learns *which* service failed, never why.
        let cause = match &failures[0].outcome {
            StartOutcome::Failed(message) if !message.is_empty() => format!(" ({message})"),
            _ => String::new(),
        };
        return Err(OperationError {
            code: "operation_failed".to_string(),
            message: format!("Service startup failed: {summary}{cause}"),
        });
    }
    Ok(())
}

async fn get_operation(
    State(manager): State<Arc<HearthManager>>,
    AxumPath(id): AxumPath<String>,
) -> Response {
    match manager.operations.get(&id) {
        Some(operation) => json_response(json!({ "operation": operation }), StatusCode::OK),
        None => ManagerHttpError::new(
            StatusCode::NOT_FOUND,
            "operation_not_found",
            "Operation not found",
        )
        .into_response(),
    }
}

fn parse_after(params: &HashMap<String, String>) -> HttpResult<Option<u64>> {
    match params.get("after") {
        None => Ok(None),
        Some(v) => v.parse::<u64>().map(Some).map_err(|_| {
            ManagerHttpError::new(
                StatusCode::BAD_REQUEST,
                "invalid_cursor",
                "after must be a non-negative integer",
            )
        }),
    }
}

async fn get_events(
    State(manager): State<Arc<HearthManager>>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    match parse_after(&params) {
        Ok(after) => {
            let replay = manager
                .events
                .replay(after, params.get("epoch").map(|s| s.as_str()));
            json_response(
                json!({ "epoch": replay.epoch, "reset": replay.reset, "events": replay.events, "latestSequence": replay.latest_sequence }),
                StatusCode::OK,
            )
        }
        Err(error) => error.into_response(),
    }
}

async fn get_events_stream(
    State(manager): State<Arc<HearthManager>>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let after = match parse_after(&params) {
        Ok(a) => a,
        Err(error) => return error.into_response(),
    };

    const MAX_QUEUE_FRAMES: usize = 64;
    type SseItem = Result<Event, std::convert::Infallible>;
    let (tx, rx) = tokio::sync::mpsc::channel::<SseItem>(MAX_QUEUE_FRAMES);

    // Live events buffer here until the snapshot frames are queued, then drain under this same
    // mutex as the gate flips. Otherwise a publish that lands after subscribe and before the
    // snapshot `try_send`s is delivered ahead of the replay the client resynchronizes from.
    struct PrefixGate {
        open: bool,
        pending: Vec<crate::state::ManagerEvent>,
    }
    let prefix = Arc::new(std::sync::Mutex::new(PrefixGate {
        open: false,
        pending: Vec::new(),
    }));
    let overflowed = Arc::new(tokio::sync::Notify::new());
    let watchdog_tx = tx.clone();
    let snapshot_tx = tx.clone();
    let sender_slot = Arc::new(std::sync::Mutex::new(Some(tx)));

    let prefix_for_listener = prefix.clone();
    let listener_slot = sender_slot.clone();
    let overflowed_for_listener = overflowed.clone();
    let deliver = |slot: &Arc<std::sync::Mutex<Option<tokio::sync::mpsc::Sender<SseItem>>>>,
                   overflowed: &Arc<tokio::sync::Notify>,
                   event: &crate::state::ManagerEvent| {
        let mut guard = slot.lock().unwrap();
        let full = match guard.as_ref() {
            Some(sender) => sender.try_send(Ok(event_to_sse(event))).is_err(),
            None => return,
        };
        if full {
            guard.take();
            overflowed.notify_one();
        }
    };
    let (replay, unsubscribe) = manager.events.subscribe_and_replay(
        after,
        params.get("epoch").map(|s| s.as_str()),
        Arc::new(move |event| {
            let mut gate = prefix_for_listener.lock().unwrap();
            if !gate.open {
                gate.pending.push(event.clone());
                return;
            }
            deliver(&listener_slot, &overflowed_for_listener, event);
        }),
        |replay| !replay.reset && replay.events.len() < MAX_QUEUE_FRAMES,
    );

    let Some(unsubscribe) = unsubscribe else {
        let _ = snapshot_tx.try_send(Ok(replay_event(
            &replay.epoch,
            true,
            replay.latest_sequence,
        )));
        // Reset, or a snapshot that would itself overflow the queue, is terminal — no live
        // subscription. The client refetches. Dropping every sender closes the stream after the
        // one reset frame.
        drop(snapshot_tx);
        sender_slot.lock().unwrap().take();
        drop(watchdog_tx);
        let stream = tokio_stream::wrappers::ReceiverStream::new(rx);
        return Sse::new(stream)
            .keep_alive(KeepAlive::default())
            .into_response();
    };

    let _ = snapshot_tx.try_send(Ok(replay_event(
        &replay.epoch,
        false,
        replay.latest_sequence,
    )));
    for event in &replay.events {
        if snapshot_tx.try_send(Ok(event_to_sse(event))).is_err() {
            break;
        }
    }
    drop(snapshot_tx);
    {
        let mut gate = prefix.lock().unwrap();
        let pending = std::mem::take(&mut gate.pending);
        // Open before draining so a listener that acquires this lock next sends *after* `pending`,
        // which is written while the lock is still held.
        gate.open = true;
        for event in &pending {
            deliver(&sender_slot, &overflowed, event);
        }
    }

    // On overflow the stream is CLOSED, not silently thinned. A client that merely stops receiving
    // some frames has no way to know it missed them: it keeps applying deltas to state that is now
    // permanently wrong (the TUI's service list). Closing makes it reconnect with
    // its cursor and take a `reset` replay, which is the whole point of having a cursor. This
    // mirrors the TS source's `stop()` on a full queue.
    //
    // Closing means dropping every `Sender`: the one the listener holds (taken out of the slot
    // below) and the one the watchdog holds. `Notify` is what lets the listener, which is a sync
    // closure, wake the async watchdog to do its half.
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
    Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response()
}

fn event_to_sse(event: &crate::state::ManagerEvent) -> Event {
    Event::default()
        .id(event.sequence.to_string())
        .event(event.event_type.clone())
        .data(serde_json::to_string(event).unwrap())
}

fn replay_event(epoch: &str, reset: bool, latest_sequence: u64) -> Event {
    Event::default().event("replay").data(
        json!({ "epoch": epoch, "reset": reset, "latestSequence": latest_sequence }).to_string(),
    )
}

async fn get_logs(
    State(manager): State<Arc<HearthManager>>,
    AxumPath(raw_service_id): AxumPath<String>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let catalog = manager.catalog();
    if !catalog.services.iter().any(|s| s.id == raw_service_id) {
        return ManagerHttpError::new(
            StatusCode::NOT_FOUND,
            "service_not_found",
            "Service is not in the catalog",
        )
        .into_response();
    }
    let cursor = match params.get("cursor") {
        None => None,
        Some(v) => match v.parse::<u64>() {
            Ok(n) => Some(n),
            Err(_) => {
                return ManagerHttpError::new(
                    StatusCode::BAD_REQUEST,
                    "invalid_cursor",
                    "cursor must be a non-negative integer",
                )
                .into_response()
            }
        },
    };
    let limit = match params.get("limit") {
        None => None,
        Some(v) => match v.parse::<u64>() {
            Ok(n) if n >= 1 => Some(n),
            _ => {
                return ManagerHttpError::new(
                    StatusCode::BAD_REQUEST,
                    "invalid_limit",
                    "limit must be a positive integer",
                )
                .into_response()
            }
        },
    };
    let generation = match params.get("generation") {
        None => None,
        Some(v) => match v.parse::<u64>() {
            Ok(n) => Some(n),
            Err(_) => {
                return ManagerHttpError::new(
                    StatusCode::BAD_REQUEST,
                    "invalid_generation",
                    "generation must be a non-negative integer",
                )
                .into_response()
            }
        },
    };
    let lifecycle_generation = manager.lifecycle_generation(&raw_service_id);
    let slice = manager
        .logs
        .read(
            &raw_service_id,
            cursor,
            limit,
            lifecycle_generation,
            generation,
        )
        .await;
    json_response(slice, StatusCode::OK)
}

/// `GET /v1/daemon/log` — the daemon's own `daemon.log` is a plain rotating file, not the
/// per-service `CursorLogStore`, so it gets its own route. Returns a `LogSlice`-shaped tail:
/// `reset` is always true (the whole tail is returned each poll — a diagnostic pane, not an
/// incremental cursor) and `nextCursor` carries the byte length so callers can detect rotation.
async fn get_daemon_log(
    State(manager): State<Arc<HearthManager>>,
    Query(params): Query<HashMap<String, String>>,
) -> HttpResult<Response> {
    let bytes = match params.get("bytes") {
        None => 131_072_u64,
        Some(v) => match v.parse::<u64>() {
            Ok(n) if (1..=1_048_576).contains(&n) => n,
            _ => {
                return Err(ManagerHttpError::new(
                    StatusCode::BAD_REQUEST,
                    "invalid_bytes",
                    "bytes must be an integer between 1 and 1048576",
                ))
            }
        },
    };
    let path = manager
        .runtime_directory
        .join(crate::daemon::DAEMON_LOG_NAME);
    let read = tokio::task::spawn_blocking(move || read_log_tail(&path, bytes))
        .await
        .map_err(|e| {
            ManagerHttpError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                e.to_string(),
            )
        })?;
    let (size, tail, truncated) = read.map_err(|e| {
        ManagerHttpError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "daemon_log_unreadable",
            format!("could not read daemon.log: {e}"),
        )
    })?;
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
    // Snap the tail start to a UTF-8 boundary so a split codepoint is not returned as text.
    let skip = if start > 0 {
        raw.iter()
            .take_while(|b| (**b & 0b1100_0000) == 0b1000_0000)
            .count()
    } else {
        0
    };
    Ok((
        size,
        String::from_utf8_lossy(&raw[skip..]).into_owned(),
        start > 0 || skip > 0,
    ))
}

async fn post_reload(
    State(manager): State<Arc<HearthManager>>,
    bytes: Bytes,
) -> HttpResult<Response> {
    ensure_not_closing(&manager)?;
    let body = strict_body(&bytes, &["requestId", "catalog"], &["requestId", "catalog"])?;
    let request_id = require_request_id(&body)?;
    let catalog_value = body.get("catalog").cloned().unwrap_or(Value::Null);
    let valid_shape = catalog_value
        .get("services")
        .map(Value::is_array)
        .unwrap_or(false)
        && catalog_value
            .get("groups")
            .map(Value::is_object)
            .unwrap_or(false)
        && catalog_value
            .get("startFailurePolicy")
            .map(Value::is_string)
            .unwrap_or(true);
    if !valid_shape {
        return Err(ManagerHttpError::new(StatusCode::BAD_REQUEST, "invalid_catalog", "catalog must be a ServiceCatalog: { services: [...], groups: {...}, startFailurePolicy? }"));
    }
    let catalog: ServiceCatalog = serde_json::from_value(catalog_value.clone()).map_err(|e| {
        ManagerHttpError::new(StatusCode::BAD_REQUEST, "invalid_catalog", e.to_string())
    })?;
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
fn replay_reload(
    manager: &HearthManager,
    request_id: &str,
    catalog: &Value,
) -> HttpResult<Option<Value>> {
    let recent = manager.reload_requests.lock().unwrap();
    match recent.iter().find(|(id, _, _)| id == request_id) {
        Some((_, seen, response)) if seen == catalog => Ok(Some(response.clone())),
        Some(_) => Err(RequestIdConflict.into()),
        None => Ok(None),
    }
}

async fn post_shutdown(
    State(manager): State<Arc<HearthManager>>,
    bytes: Bytes,
) -> HttpResult<Response> {
    let body = strict_body(&bytes, &["requestId", "mode"], &["requestId"])?;
    let request_id = require_request_id(&body)?;
    let mode = match body.get("mode") {
        None => "refuse-if-active",
        Some(Value::String(s))
            if matches!(
                s.as_str(),
                "refuse-if-active" | "stop-services" | "leave-services"
            ) =>
        {
            s.as_str()
        }
        _ => {
            return Err(ManagerHttpError::new(
                StatusCode::BAD_REQUEST,
                "invalid_shutdown_mode",
                "mode must be refuse-if-active, stop-services, or leave-services",
            ))
        }
    };
    let shutdown_mode = if mode == "stop-services" {
        ShutdownMode::StopServices
    } else {
        ShutdownMode::LeaveServices
    };
    let schedule_input = OperationInput {
        request_id,
        kind: OperationKind::ManagerShutdown,
        service_id: None,
        target_service_ids: None,
        action: None,
    };
    if let Some(existing) = manager.operations.resolve_request(&schedule_input)? {
        return Ok(json_response(
            json!({ "operation": existing }),
            StatusCode::ACCEPTED,
        ));
    }
    ensure_not_closing(&manager)?;
    let non_terminal = [
        ActualServiceState::Stopped,
        ActualServiceState::QueuedStart,
        ActualServiceState::Failed,
        ActualServiceState::Orphaned,
        ActualServiceState::ExternallyOwned,
    ];
    let catalog = manager.catalog();
    let active = manager.service_states().into_iter().any(|s| {
        default_daemon_owned(&catalog, &s.service_id) && !non_terminal.contains(&s.actual_state)
    });
    // Only `refuse-if-active` guards. `leave-services` is the deliberate "restart the daemon, keep
    // the services" path (`hearth manager restart`): daemon-owned processes are detached and
    // outlive this daemon, and the next one re-adopts them from their persisted identities — the
    // same thing a SIGTERM shutdown already does.
    if active && mode == "refuse-if-active" {
        return Err(ManagerHttpError::new(
            StatusCode::CONFLICT,
            "active_services",
            "Manager shutdown is refused while managed services are active",
        ));
    }
    let manager_for_shutdown = manager.clone();
    tokio::spawn(async move {
        manager_for_shutdown.shutdown(shutdown_mode).await;
    });
    let operation = manager.operations.schedule(
        schedule_input,
        Box::new(|_h| async { Ok(()) }.boxed()),
        None,
    )?;
    Ok(json_response(
        json!({ "operation": operation }),
        StatusCode::ACCEPTED,
    ))
}
