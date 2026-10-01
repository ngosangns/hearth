//! Loopback HTTP carrier for the browser GUI.
//!
//! Named routes are the workspace and shared-service API. Everything else the page needs is a
//! static file. A request is admitted only when its Host is this process's loopback address and,
//! for `/api`, it presents the session cookie. The process token is accepted once, on `GET /`,
//! and is traded for that cookie.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::{Path as AxumPath, Query, State};
use axum::http::{header, HeaderMap, Method as HttpMethod, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use hearth_cli::{
    encode_path_segment, ensure, request, request_with_timeout, require_client_for,
    restart_manager, stop_manager, LocalctlOptions, EXIT_UNAVAILABLE,
};
use hearth_core::config_file::load_catalog;
use reqwest::Method;
use serde::Deserialize;
use serde_json::{json, Value};
use subtle::ConstantTimeEq;
use tokio::sync::{oneshot, OwnedMutexGuard};
use tokio::task::JoinHandle;

use crate::store::{display_path, folder_name, WorkspaceRecord, WorkspaceStore};
use crate::SpawnHook;

use include_dir::{include_dir, Dir};

static DIST: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/ui/dist");
const COOKIE: &str = "hearth_session";
const FAILURE_COOLDOWN: Duration = Duration::from_secs(4);

pub struct ServeOptions {
    pub port: u16,
    pub workspace_file: std::path::PathBuf,
    pub adopt_root: Option<std::path::PathBuf>,
    pub spawn_daemon: SpawnHook,
    pub spawn_smp: SpawnHook,
}

pub struct Running {
    pub url: String,
    pub token: String,
    pub addr: SocketAddr,
    stop: oneshot::Sender<()>,
    task: JoinHandle<()>,
}

impl Running {
    pub async fn shutdown(self) {
        let _ = self.stop.send(());
        let _ = tokio::time::timeout(Duration::from_secs(2), self.task).await;
    }
}

struct App {
    token: String,
    port: u16,
    store: Mutex<WorkspaceStore>,
    spawn_daemon: SpawnHook,
    spawn_smp: SpawnHook,
    gates: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    failures: Mutex<HashMap<String, (Instant, String)>>,
    suggested: Option<String>,
}

pub async fn serve(options: ServeOptions) -> Result<Running, String> {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", options.port))
        .await
        .map_err(|error| format!("cannot listen on 127.0.0.1:{}: {error}", options.port))?;
    let addr = listener.local_addr().map_err(|error| error.to_string())?;
    let mut store = WorkspaceStore::open(options.workspace_file)?;
    let suggested = options
        .adopt_root
        .as_deref()
        .and_then(|root| store.adopt_project(root));
    let token = fresh_token()?;
    let state = Arc::new(App {
        token: token.clone(),
        port: addr.port(),
        store: Mutex::new(store),
        spawn_daemon: options.spawn_daemon,
        spawn_smp: options.spawn_smp,
        gates: Mutex::new(HashMap::new()),
        failures: Mutex::new(HashMap::new()),
        suggested,
    });
    let router = routes(state);
    let (stop, stop_rx) = oneshot::channel();
    let task = tokio::spawn(async move {
        let _ = axum::serve(listener, router)
            .with_graceful_shutdown(async move {
                let _ = stop_rx.await;
            })
            .await;
    });
    let url = format!("http://127.0.0.1:{}/?token={token}", addr.port());
    Ok(Running {
        url,
        token,
        addr,
        stop,
        task,
    })
}

fn routes(state: Arc<App>) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/assets/*file", get(asset))
        .route("/favicon.ico", get(favicon))
        .route("/api/workspaces", get(list_workspaces).post(add_workspace))
        .route(
            "/api/workspaces/:id",
            axum::routing::delete(remove_workspace),
        )
        .route("/api/workspaces/:id/trust", post(trust_workspace))
        .route("/api/workspaces/:id/reveal", post(reveal_workspace))
        .route("/api/workspaces/:id/snapshot", get(snapshot))
        .route("/api/workspaces/:id/operations", post(post_operation))
        .route("/api/workspaces/:id/operations/:op", get(get_operation))
        .route("/api/workspaces/:id/bulk-start", post(bulk_start))
        .route("/api/workspaces/:id/logs", get(service_logs))
        .route("/api/workspaces/:id/daemon-log", get(daemon_log))
        .route("/api/workspaces/:id/reload", post(reload_catalog))
        .route("/api/workspaces/:id/daemon/restart", post(restart_daemon))
        .route("/api/workspaces/:id/daemon/stop", post(stop_daemon))
        .route("/api/shared", get(shared_status))
        .route("/api/shared/catalog", get(shared_catalog))
        .route("/api/shared/operations", post(shared_operation))
        .route("/api/shared/operations/:op", get(shared_get_operation))
        .route("/api/shared/install", post(shared_install))
        .route("/api/shared/remove", post(shared_remove))
        .route("/api/shared/logs", get(shared_logs))
        .layer(middleware::from_fn_with_state(state.clone(), guard))
        .with_state(state)
}

async fn guard(
    State(state): State<Arc<App>>,
    request: axum::extract::Request,
    next: Next,
) -> Response {
    if !host_allowed(request.headers(), state.port) {
        return error_json(
            StatusCode::FORBIDDEN,
            "forbidden_host",
            "This GUI only accepts requests to 127.0.0.1.",
        );
    }
    let path = request.uri().path();
    if path.starts_with("/assets/") || path == "/favicon.ico" {
        return next.run(request).await;
    }
    if path != "/" && !path.starts_with("/api/") {
        return error_json(StatusCode::NOT_FOUND, "not_found", "not found");
    }
    let presented = query_value(request.uri().query(), "token");
    let query_ok = path == "/"
        && presented
            .as_deref()
            .is_some_and(|token| tokens_match(token, &state.token));
    let cookie_ok =
        cookie_token(request.headers()).is_some_and(|token| tokens_match(&token, &state.token));
    if path.starts_with("/api/") {
        if !cookie_ok {
            return error_json(
                StatusCode::UNAUTHORIZED,
                "unauthorized",
                "Open the URL printed by hearthd web.",
            );
        }
        match request
            .headers()
            .get(header::ORIGIN)
            .and_then(|value| value.to_str().ok())
        {
            Some(origin) if origin_allowed(origin, state.port) => {}
            Some(_) => {
                return error_json(
                    StatusCode::FORBIDDEN,
                    "forbidden_origin",
                    "Cross-origin requests are refused.",
                )
            }
            None if request.method() != HttpMethod::GET && request.method() != HttpMethod::HEAD => {
                return error_json(
                    StatusCode::FORBIDDEN,
                    "forbidden_origin",
                    "This request needs an Origin header.",
                );
            }
            None => {}
        }
        return next.run(request).await;
    }
    if query_ok || cookie_ok {
        return next.run(request).await;
    }
    html(StatusCode::UNAUTHORIZED, UNAUTHORIZED_HTML)
}

const UNAUTHORIZED_HTML: &str = r#"<!doctype html>
<html lang="en"><meta charset="utf-8"><title>Hearth</title>
<h1>Hearth</h1>
<p>This session is not signed in. Open the URL printed by <code>hearthd web</code>.</p>
"#;

async fn index(State(state): State<Arc<App>>, uri: axum::http::Uri) -> Response {
    if query_value(uri.query(), "token")
        .as_deref()
        .is_some_and(|token| tokens_match(token, &state.token))
    {
        let cookie = format!(
            "{COOKIE}={}; HttpOnly; SameSite=Strict; Path=/; Max-Age=43200",
            state.token
        );
        return (
            StatusCode::FOUND,
            [
                (header::LOCATION, "/"),
                (header::SET_COOKIE, cookie.as_str()),
                (header::CACHE_CONTROL, "no-store"),
                (header::REFERRER_POLICY, "no-referrer"),
            ],
        )
            .into_response();
    }
    html(StatusCode::OK, dist_index())
}

async fn asset(AxumPath(file): AxumPath<String>) -> Response {
    match DIST.get_file(format!("assets/{file}")) {
        Some(asset) => static_file(&format!("assets/{file}"), asset.contents()),
        None => error_json(StatusCode::NOT_FOUND, "not_found", "not found"),
    }
}

fn dist_index() -> &'static str {
    DIST.get_file("index.html")
        .and_then(|file| file.contents_utf8())
        .expect("the Solid web UI was not built")
}

fn static_file(path: &str, body: &'static [u8]) -> Response {
    let content_type = if path.ends_with(".js") {
        "text/javascript; charset=utf-8"
    } else if path.ends_with(".css") {
        "text/css; charset=utf-8"
    } else if path.ends_with(".svg") {
        "image/svg+xml"
    } else {
        "application/octet-stream"
    };
    ([(header::CONTENT_TYPE, content_type), (header::CACHE_CONTROL, "no-store")], body).into_response()
}

async fn favicon() -> StatusCode {
    StatusCode::NO_CONTENT
}

fn html(status: StatusCode, body: &'static str) -> Response {
    (
        status,
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (header::CACHE_CONTROL, "no-store"),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
            (header::REFERRER_POLICY, "no-referrer"),
            (header::CONTENT_SECURITY_POLICY, "default-src 'self'; script-src 'self'; style-src 'self'; connect-src 'self'; img-src 'self' data:; base-uri 'none'; form-action 'self'; frame-ancestors 'none'"),
        ],
        body,
    )
        .into_response()
}

async fn list_workspaces(State(state): State<Arc<App>>) -> Response {
    let store = state.store.lock().expect("workspace store");
    json_response(
        StatusCode::OK,
        json!({
            "workspaces": store.list().iter().map(workspace_view).collect::<Vec<_>>(),
            "loadError": store.load_error,
            "suggestedWorkspaceId": state.suggested,
        }),
    )
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AddBody {
    path: String,
}

async fn add_workspace(State(state): State<Arc<App>>, Json(body): Json<AddBody>) -> Response {
    let mut store = state.store.lock().expect("workspace store");
    match store.add(&body.path) {
        Ok(added) => {
            let status = if added.created {
                StatusCode::CREATED
            } else {
                StatusCode::OK
            };
            json_response(
                status,
                json!({ "workspace": workspace_view(&added.record), "created": added.created }),
            )
        }
        Err(error) => error_json(StatusCode::BAD_REQUEST, "invalid_path", error),
    }
}

async fn remove_workspace(
    State(state): State<Arc<App>>,
    AxumPath(id): AxumPath<String>,
) -> Response {
    if !valid_id(&id) {
        return error_json(
            StatusCode::BAD_REQUEST,
            "invalid_workspace",
            "workspace id is invalid",
        );
    }
    let mut store = state.store.lock().expect("workspace store");
    match store.remove(&id) {
        Ok(true) => json_response(StatusCode::OK, json!({ "removed": true })),
        Ok(false) => error_json(
            StatusCode::NOT_FOUND,
            "workspace_not_found",
            "workspace not found",
        ),
        Err(error) => error_json(StatusCode::INTERNAL_SERVER_ERROR, "store_failed", error),
    }
}

async fn trust_workspace(
    State(state): State<Arc<App>>,
    AxumPath(id): AxumPath<String>,
) -> Response {
    if !valid_id(&id) {
        return error_json(
            StatusCode::BAD_REQUEST,
            "invalid_workspace",
            "workspace id is invalid",
        );
    }
    let mut store = state.store.lock().expect("workspace store");
    match store.trust(&id) {
        Ok(record) => json_response(
            StatusCode::OK,
            json!({ "workspace": workspace_view(&record) }),
        ),
        Err(error) if error == "workspace not found" => {
            error_json(StatusCode::NOT_FOUND, "workspace_not_found", error)
        }
        Err(error) => error_json(StatusCode::INTERNAL_SERVER_ERROR, "store_failed", error),
    }
}

async fn snapshot(State(state): State<Arc<App>>, AxumPath(id): AxumPath<String>) -> Response {
    let Some(record) = workspace(&state, &id) else {
        return missing_workspace(&id);
    };
    let path = Path::new(&record.path);
    if !path.is_dir() {
        return json_response(
            StatusCode::OK,
            stamp(
                path,
                json!({ "workspace": workspace_view(&record), "missing": true, "trusted": record.trusted, "error": "This folder is missing.", "services": [], "urls": [] }),
            ),
        );
    }
    let disk = match load_catalog(path) {
        Ok(loaded) => serde_json::to_value(&loaded.catalog).unwrap_or(Value::Null),
        Err(error) => {
            return json_response(
                StatusCode::OK,
                stamp(
                    path,
                    json!({ "workspace": workspace_view(&record), "trusted": record.trusted, "catalogError": error.errors.join("; "), "services": [], "urls": [] }),
                ),
            );
        }
    };
    if !record.trusted {
        return json_response(
            StatusCode::OK,
            stamp(
                path,
                json!({ "workspace": workspace_view(&record), "trusted": false, "catalog": disk, "services": [], "urls": [] }),
            ),
        );
    }
    let client = match connect(&state, path).await {
        Ok(client) => client,
        Err(error) => {
            return json_response(
                StatusCode::OK,
                stamp(
                    path,
                    json!({ "workspace": workspace_view(&record), "trusted": true, "catalog": disk, "error": error, "services": [], "urls": [] }),
                ),
            )
        }
    };
    let (services, catalog, urls) = tokio::join!(
        request(&client, "/v1/services", Method::GET, None, None),
        request(&client, "/v1/catalog", Method::GET, None, None),
        request(&client, "/v1/urls", Method::GET, None, None),
    );
    let services = match services {
        Ok(body) => body.get("services").cloned().unwrap_or(Value::Null),
        Err(error) => {
            return json_response(
                StatusCode::OK,
                stamp(
                    path,
                    json!({ "workspace": workspace_view(&record), "trusted": true, "catalog": disk, "error": error, "services": [], "urls": [] }),
                ),
            )
        }
    };
    let catalog = catalog
        .ok()
        .and_then(|body| body.get("catalog").cloned())
        .unwrap_or(disk);
    let urls = urls
        .ok()
        .and_then(|body| body.get("urls").cloned())
        .unwrap_or_else(|| json!([]));
    json_response(
        StatusCode::OK,
        stamp(
            path,
            json!({
                "workspace": workspace_view(&record),
                "trusted": true,
                "catalog": catalog,
                "services": services,
                "urls": urls,
                "daemon": { "pid": client.metadata.pid, "port": client.metadata.port, "instanceId": client.metadata.instance_id },
            }),
        ),
    )
}

/// Shows the workspace folder in Finder. The path is one the operator already added.
async fn reveal_workspace(
    State(state): State<Arc<App>>,
    AxumPath(id): AxumPath<String>,
) -> Response {
    let Some(record) = workspace(&state, &id) else {
        return missing_workspace(&id);
    };
    if !Path::new(&record.path).exists() {
        return error_json(
            StatusCode::NOT_FOUND,
            "missing_folder",
            "This folder is missing.",
        );
    }
    match std::process::Command::new("open")
        .arg("-R")
        .arg(&record.path)
        .status()
    {
        Ok(status) if status.success() => json_response(StatusCode::OK, json!({ "ok": true })),
        Ok(status) => error_json(
            StatusCode::INTERNAL_SERVER_ERROR,
            "reveal_failed",
            format!("open exited {status}"),
        ),
        Err(error) => error_json(
            StatusCode::INTERNAL_SERVER_ERROR,
            "reveal_failed",
            error.to_string(),
        ),
    }
}

/// Seconds since the epoch of the catalog file, so the page can reload after an edit the way the
/// a directory watcher would.
fn config_revision(root: &Path) -> Option<u64> {
    let path = hearth_core::config_file::find_config_file(root)?;
    let modified = std::fs::metadata(path).ok()?.modified().ok()?;
    modified
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|age| age.as_secs())
}

fn stamp(root: &Path, mut body: Value) -> Value {
    if let Some(revision) = config_revision(root) {
        body["configRevision"] = json!(revision);
    }
    body
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct OperationBody {
    service_id: String,
    action: String,
    #[serde(default)]
    kill_unowned: bool,
}

async fn post_operation(
    State(state): State<Arc<App>>,
    AxumPath(id): AxumPath<String>,
    Json(body): Json<OperationBody>,
) -> Response {
    if body.kill_unowned && body.action != "start" {
        return error_json(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "killUnowned only applies to a start action",
        );
    }
    if !matches!(body.action.as_str(), "start" | "stop" | "restart") || !valid_id(&body.service_id)
    {
        return error_json(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "action must be start, stop, or restart",
        );
    }
    let Some(client) = trusted_client(&state, &id).await else {
        return missing_or_untrusted(&state, &id).await;
    };
    let payload = json!({ "requestId": new_request_id(), "serviceId": body.service_id, "action": body.action, "killUnowned": body.kill_unowned });
    proxy(&client, Method::POST, "/v1/operations", Some(&payload)).await
}

async fn get_operation(
    State(state): State<Arc<App>>,
    AxumPath((id, op)): AxumPath<(String, String)>,
) -> Response {
    if !valid_id(&op) {
        return error_json(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "operation id is invalid",
        );
    }
    let Some(client) = trusted_client(&state, &id).await else {
        return missing_or_untrusted(&state, &id).await;
    };
    let path = format!("/v1/operations/{}", encode_path_segment(&op));
    proxy(&client, Method::GET, &path, None).await
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BulkBody {
    targets: Vec<String>,
}

async fn bulk_start(
    State(state): State<Arc<App>>,
    AxumPath(id): AxumPath<String>,
    Json(body): Json<BulkBody>,
) -> Response {
    if body.targets.is_empty() || body.targets.iter().any(|target| !valid_id(target)) {
        return error_json(
            StatusCode::BAD_REQUEST,
            "invalid_targets",
            "targets must be catalog service ids",
        );
    }
    let Some(client) = trusted_client(&state, &id).await else {
        return missing_or_untrusted(&state, &id).await;
    };
    let payload = json!({ "requestId": new_request_id(), "targets": body.targets });
    proxy(
        &client,
        Method::POST,
        "/v1/operations/bulk-start",
        Some(&payload),
    )
    .await
}

#[derive(Deserialize)]
struct LogQuery {
    service: String,
    cursor: Option<u64>,
    generation: Option<u64>,
    limit: Option<u64>,
}

async fn service_logs(
    State(state): State<Arc<App>>,
    AxumPath(id): AxumPath<String>,
    Query(query): Query<LogQuery>,
) -> Response {
    if !valid_id(&query.service) {
        return error_json(
            StatusCode::BAD_REQUEST,
            "invalid_service",
            "service id is invalid",
        );
    }
    let Some(client) = trusted_client(&state, &id).await else {
        return missing_or_untrusted(&state, &id).await;
    };
    let limit = query.limit.unwrap_or(16_384).clamp(1, 1_048_576);
    let mut path = format!(
        "/v1/logs/{}?limit={limit}",
        encode_path_segment(&query.service)
    );
    if let Some(cursor) = query.cursor {
        path.push_str(&format!("&cursor={cursor}"));
    }
    if let Some(generation) = query.generation {
        path.push_str(&format!("&generation={generation}"));
    }
    proxy(&client, Method::GET, &path, None).await
}

async fn daemon_log(State(state): State<Arc<App>>, AxumPath(id): AxumPath<String>) -> Response {
    let Some(client) = trusted_client(&state, &id).await else {
        return missing_or_untrusted(&state, &id).await;
    };
    proxy(&client, Method::GET, "/v1/daemon/log?bytes=131072", None).await
}

async fn reload_catalog(State(state): State<Arc<App>>, AxumPath(id): AxumPath<String>) -> Response {
    let Some(record) = workspace(&state, &id) else {
        return missing_workspace(&id);
    };
    if !record.trusted {
        return error_json(
            StatusCode::CONFLICT,
            "untrusted",
            "Trust this workspace before reloading it.",
        );
    }
    let path = Path::new(&record.path);
    let loaded = match load_catalog(path) {
        Ok(loaded) => loaded,
        Err(error) => {
            return error_json(
                StatusCode::BAD_REQUEST,
                "invalid_catalog",
                error.errors.join("; "),
            )
        }
    };
    let client = match connect(&state, path).await {
        Ok(client) => client,
        Err(error) => return error_json(StatusCode::BAD_GATEWAY, "manager_unavailable", error),
    };
    let payload = json!({ "requestId": new_request_id(), "catalog": loaded.catalog });
    proxy(&client, Method::POST, "/v1/manager/reload", Some(&payload)).await
}

async fn restart_daemon(State(state): State<Arc<App>>, AxumPath(id): AxumPath<String>) -> Response {
    daemon_lifecycle(&state, &id, true).await
}

async fn stop_daemon(State(state): State<Arc<App>>, AxumPath(id): AxumPath<String>) -> Response {
    daemon_lifecycle(&state, &id, false).await
}

async fn daemon_lifecycle(state: &App, id: &str, restart: bool) -> Response {
    let Some(record) = workspace(state, id) else {
        return missing_workspace(id);
    };
    if !record.trusted {
        return error_json(
            StatusCode::CONFLICT,
            "untrusted",
            "Trust this workspace first.",
        );
    }
    let path = Path::new(&record.path);
    let _guard = project_guard(state, path).await;
    let loaded = match load_catalog(path) {
        Ok(loaded) => loaded,
        Err(error) => {
            return error_json(
                StatusCode::BAD_REQUEST,
                "invalid_catalog",
                error.errors.join("; "),
            )
        }
    };
    let spawn = state.spawn_daemon.clone();
    let options = LocalctlOptions {
        catalog: loaded.catalog,
        spawn_daemon: Box::new(move |root| spawn(root)),
    };
    let result = if restart {
        restart_manager(path, &options).await
    } else {
        stop_manager(path, &options).await
    };
    match result {
        Ok(_) => json_response(StatusCode::OK, json!({ "ok": true })),
        Err(error) => error_json(
            StatusCode::BAD_GATEWAY,
            "manager_unavailable",
            error.message,
        ),
    }
}

async fn shared_status(State(state): State<Arc<App>>) -> Response {
    let client = match connect_smp(&state).await {
        Ok(client) => client,
        Err(error) => {
            return json_response(StatusCode::OK, json!({ "instances": [], "error": error }))
        }
    };
    match request(&client, "/v1/shared", Method::GET, None, None).await {
        Ok(body) => json_response(StatusCode::OK, body),
        Err(error) => json_response(StatusCode::OK, json!({ "instances": [], "error": error })),
    }
}

async fn shared_catalog(State(state): State<Arc<App>>) -> Response {
    let client = match connect_smp(&state).await {
        Ok(client) => client,
        Err(error) => {
            return json_response(StatusCode::OK, json!({ "services": [], "error": error }))
        }
    };
    match request(&client, "/v1/shared/catalog", Method::GET, None, None).await {
        Ok(body) => json_response(StatusCode::OK, summarize_shared_catalog(&body)),
        Err(error) => json_response(StatusCode::OK, json!({ "services": [], "error": error })),
    }
}

async fn shared_get_operation(
    State(state): State<Arc<App>>,
    AxumPath(op): AxumPath<String>,
) -> Response {
    if !valid_id(&op) {
        return error_json(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "operation id is invalid",
        );
    }
    let Some(client) = smp_client(&state).await else {
        return error_json(
            StatusCode::BAD_GATEWAY,
            "manager_unavailable",
            "shared services daemon is unavailable",
        );
    };
    let path = format!("/v1/operations/{}", encode_path_segment(&op));
    proxy(&client, Method::GET, &path, None).await
}

async fn shared_operation(
    State(state): State<Arc<App>>,
    Json(body): Json<OperationBody>,
) -> Response {
    if let Err(message) = check_shared_action(&body) {
        return error_json(StatusCode::BAD_REQUEST, "invalid_request", message);
    }
    let Some(client) = smp_client(&state).await else {
        return error_json(
            StatusCode::BAD_GATEWAY,
            "manager_unavailable",
            "shared services daemon is unavailable",
        );
    };
    let payload = json!({ "requestId": new_request_id(), "serviceId": body.service_id, "action": body.action, "killUnowned": body.kill_unowned });
    proxy(&client, Method::POST, "/v1/operations", Some(&payload)).await
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SharedServiceBody {
    service: String,
    #[serde(default)]
    force: bool,
}

async fn shared_install(
    State(state): State<Arc<App>>,
    Json(body): Json<SharedServiceBody>,
) -> Response {
    if let Err(message) = shared_instance_id(&body.service) {
        return error_json(StatusCode::BAD_REQUEST, "invalid_service", message);
    }
    let Some(client) = smp_client(&state).await else {
        return error_json(
            StatusCode::BAD_GATEWAY,
            "manager_unavailable",
            "shared services daemon is unavailable",
        );
    };
    let payload = json!({ "service": body.service });
    proxy_slow(&client, "/v1/shared/install", &payload).await
}

async fn shared_remove(
    State(state): State<Arc<App>>,
    Json(body): Json<SharedServiceBody>,
) -> Response {
    if let Err(message) = shared_instance_id(&body.service) {
        return error_json(StatusCode::BAD_REQUEST, "invalid_service", message);
    }
    let Some(client) = smp_client(&state).await else {
        return error_json(
            StatusCode::BAD_GATEWAY,
            "manager_unavailable",
            "shared services daemon is unavailable",
        );
    };
    let mut payload = json!({ "service": body.service });
    if body.force {
        payload["force"] = json!(true);
    }
    proxy(&client, Method::POST, "/v1/shared/remove", Some(&payload)).await
}

async fn shared_logs(State(state): State<Arc<App>>, Query(query): Query<LogQuery>) -> Response {
    if shared_instance_id(&query.service).is_err() {
        return error_json(
            StatusCode::BAD_REQUEST,
            "invalid_service",
            "expected name@version",
        );
    }
    let Some(client) = smp_client(&state).await else {
        return error_json(
            StatusCode::BAD_GATEWAY,
            "manager_unavailable",
            "shared services daemon is unavailable",
        );
    };
    let path = format!(
        "/v1/logs/{}?limit=16384",
        encode_path_segment(&query.service)
    );
    proxy(&client, Method::GET, &path, None).await
}

fn check_shared_action(body: &OperationBody) -> Result<(), &'static str> {
    if body.kill_unowned && body.action != "start" {
        return Err("killUnowned only applies to a start action");
    }
    if !matches!(body.action.as_str(), "start" | "stop" | "restart")
        || shared_instance_id(&body.service_id).is_err()
    {
        return Err("action must be start, stop, or restart of name@version");
    }
    Ok(())
}

fn shared_instance_id(service: &str) -> Result<(), String> {
    let Some((name, version)) = service.split_once('@') else {
        return Err("expected name@version".to_string());
    };
    if name.is_empty() || version.is_empty() || version.contains('@') || !valid_id(service) {
        return Err("expected name@version".to_string());
    }
    Ok(())
}

fn summarize_shared_catalog(body: &Value) -> Value {
    let mut services = Vec::new();
    let Some(map) = body
        .get("catalog")
        .and_then(|catalog| catalog.get("services"))
        .and_then(Value::as_object)
    else {
        return json!({ "services": services });
    };
    for (name, recipe) in map {
        let versions: Vec<&String> = recipe
            .get("versions")
            .and_then(Value::as_object)
            .map(|versions| versions.keys().collect())
            .unwrap_or_default();
        services.push(json!({ "name": name, "versions": versions }));
    }
    json!({ "services": services })
}

async fn trusted_client(state: &App, id: &str) -> Option<hearth_cli::Client> {
    let record = workspace(state, id)?;
    if !record.trusted {
        return None;
    }
    connect(state, Path::new(&record.path)).await.ok()
}

async fn missing_or_untrusted(state: &App, id: &str) -> Response {
    match workspace(state, id) {
        None => missing_workspace(id),
        Some(record) if !record.trusted => error_json(
            StatusCode::CONFLICT,
            "untrusted",
            "Trust this workspace first.",
        ),
        Some(_) => error_json(
            StatusCode::BAD_GATEWAY,
            "manager_unavailable",
            "hearth manager is unavailable",
        ),
    }
}

fn missing_workspace(id: &str) -> Response {
    if valid_id(id) {
        error_json(
            StatusCode::NOT_FOUND,
            "workspace_not_found",
            "workspace not found",
        )
    } else {
        error_json(
            StatusCode::BAD_REQUEST,
            "invalid_workspace",
            "workspace id is invalid",
        )
    }
}

fn workspace(state: &App, id: &str) -> Option<WorkspaceRecord> {
    if !valid_id(id) {
        return None;
    }
    state
        .store
        .lock()
        .expect("workspace store")
        .get(id)
        .cloned()
}

async fn connect(state: &App, path: &Path) -> Result<hearth_cli::Client, String> {
    let key = path_key(path);
    let _guard = project_guard(state, path).await;
    if let Some(message) = recent_failure(state, &key) {
        return Err(message);
    }
    let loaded = load_catalog(path).map_err(|error| error.errors.join("; "))?;
    match require_client_for(path, &loaded.catalog).await {
        Ok(client) => {
            clear_failure(state, &key);
            return Ok(client);
        }
        Err(error) if error.exit_code == EXIT_UNAVAILABLE => {}
        Err(error) => return Err(error.message),
    }
    let spawn = state.spawn_daemon.clone();
    let options = LocalctlOptions {
        catalog: loaded.catalog,
        spawn_daemon: Box::new(move |root| spawn(root)),
    };
    match ensure(path, &options).await {
        Ok(client) => {
            clear_failure(state, &key);
            Ok(client)
        }
        Err(error) => {
            remember_failure(state, &key, &error.message);
            Err(error.message)
        }
    }
}

async fn connect_smp(state: &App) -> Result<hearth_cli::Client, String> {
    let _guard = smp_guard(state).await;
    if let Some(message) = recent_failure(state, "$smp") {
        return Err(message);
    }
    match hearth_cli::shared::ensure_smp(&state.spawn_smp).await {
        Ok(client) => {
            clear_failure(state, "$smp");
            Ok(client)
        }
        Err(error) if error.exit_code == EXIT_UNAVAILABLE => {
            remember_failure(state, "$smp", &error.message);
            Err(error.message)
        }
        Err(error) => Err(error.message),
    }
}

async fn smp_client(state: &App) -> Option<hearth_cli::Client> {
    connect_smp(state).await.ok()
}

async fn project_guard(state: &App, path: &Path) -> OwnedMutexGuard<()> {
    gate(state, &path_key(path)).lock_owned().await
}

async fn smp_guard(state: &App) -> OwnedMutexGuard<()> {
    gate(state, "$smp").lock_owned().await
}

fn gate(state: &App, key: &str) -> Arc<tokio::sync::Mutex<()>> {
    let mut gates = state.gates.lock().expect("connect gates");
    gates
        .entry(key.to_string())
        .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
        .clone()
}

fn recent_failure(state: &App, key: &str) -> Option<String> {
    let mut failures = state.failures.lock().expect("connect failures");
    let expired = failures
        .get(key)
        .is_some_and(|(at, _)| at.elapsed() >= FAILURE_COOLDOWN);
    if expired {
        failures.remove(key);
        return None;
    }
    failures.get(key).map(|(_, message)| message.clone())
}

fn remember_failure(state: &App, key: &str, message: &str) {
    state
        .failures
        .lock()
        .expect("connect failures")
        .insert(key.to_string(), (Instant::now(), message.to_string()));
}

fn clear_failure(state: &App, key: &str) {
    state.failures.lock().expect("connect failures").remove(key);
}

fn path_key(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

async fn proxy(
    client: &hearth_cli::Client,
    method: Method,
    path: &str,
    body: Option<&Value>,
) -> Response {
    match request(client, path, method, body, None).await {
        Ok(value) => json_response(StatusCode::OK, value),
        Err(error) => daemon_error(error),
    }
}

async fn proxy_slow(client: &hearth_cli::Client, path: &str, body: &Value) -> Response {
    match request_with_timeout(client, path, Method::POST, Some(body), None, None).await {
        Ok(value) => json_response(StatusCode::OK, value),
        Err(error) => daemon_error(error),
    }
}

fn daemon_error(error: String) -> Response {
    if error == "manager unavailable" || error == "manager request timed out" {
        return error_json(StatusCode::BAD_GATEWAY, "manager_unavailable", error);
    }
    let (code, message) = error
        .split_once(':')
        .unwrap_or(("request_failed", error.as_str()));
    let status = if code == "service_not_found" || code == "operation_not_found" {
        StatusCode::NOT_FOUND
    } else if code.starts_with("invalid_") {
        StatusCode::BAD_REQUEST
    } else if code == "unauthorized" {
        StatusCode::BAD_GATEWAY
    } else {
        StatusCode::CONFLICT
    };
    error_json(status, code, message.to_string())
}

fn workspace_view(record: &WorkspaceRecord) -> Value {
    json!({
        "id": record.id,
        "path": record.path,
        "trusted": record.trusted,
        "addedAt": record.added_at,
        "name": folder_name(&record.path),
        "displayPath": display_path(&record.path),
        "exists": Path::new(&record.path).is_dir(),
    })
}

fn json_response(status: StatusCode, body: Value) -> Response {
    (
        status,
        [
            (header::CACHE_CONTROL, "no-store"),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
            (header::REFERRER_POLICY, "no-referrer"),
        ],
        Json(body),
    )
        .into_response()
}

fn error_json(status: StatusCode, code: &str, message: impl Into<String>) -> Response {
    json_response(
        status,
        json!({ "error": { "code": code, "message": message.into() } }),
    )
}

fn new_request_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 256
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'@'))
}

fn host_allowed(headers: &HeaderMap, port: u16) -> bool {
    let Some(host) = headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
    else {
        return false;
    };
    host == format!("127.0.0.1:{port}") || (port == 80 && host == "127.0.0.1")
}

fn origin_allowed(origin: &str, port: u16) -> bool {
    origin == format!("http://127.0.0.1:{port}")
}

fn cookie_token(headers: &HeaderMap) -> Option<String> {
    let raw = headers.get(header::COOKIE)?.to_str().ok()?;
    for part in raw.split(';') {
        let part = part.trim();
        if let Some(value) = part.strip_prefix("hearth_session=") {
            return Some(value.to_string());
        }
    }
    None
}

fn query_value(query: Option<&str>, name: &str) -> Option<String> {
    let query = query?;
    for pair in query.split('&') {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        if key == name {
            return Some(percent_decode(value));
        }
    }
    None
}

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).ok();
            if let Some(byte) = hex.and_then(|text| u8::from_str_radix(text, 16).ok()) {
                out.push(byte);
                index += 3;
                continue;
            }
        }
        out.push(if bytes[index] == b'+' {
            b' '
        } else {
            bytes[index]
        });
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn tokens_match(presented: &str, expected: &str) -> bool {
    presented.as_bytes().ct_eq(expected.as_bytes()).into()
}

fn fresh_token() -> Result<String, String> {
    let mut bytes = [0u8; 32];
    let mut file = std::fs::File::open("/dev/urandom")
        .map_err(|error| format!("cannot read randomness: {error}"))?;
    use std::io::Read;
    file.read_exact(&mut bytes)
        .map_err(|error| format!("cannot read randomness: {error}"))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}
