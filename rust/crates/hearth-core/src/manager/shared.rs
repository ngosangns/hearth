//! The smp-only HTTP surface (`/v1/shared/*`) — mounted by `router()` when the manager was built
//! with a `SharedContext`. Orchestrates register → install → catalog-insert → start →
//! provision per `docs/shared-services.md`. All mutations serialize on a per-instance lock so two
//! projects racing `postgres@16.4` converge on one install.
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::response::Response;
use axum::routing::{get, post};
use axum::Router;
use serde_json::{json, Value};

use crate::shared::install::ensure_installed;
use crate::shared::ports::{allocate_ports, MAX_SHARED_PORTS};
use crate::shared::registry::{InstallState, SharedAttachment, SharedInstance};
use crate::shared::render::{instance_vars, project_vars, render_command, render_connection};
use crate::shared::synthesize::synthesize_catalog;
use crate::shared::{project_id, SharedContext, SharedError};
use crate::state::ActualServiceState;
use crate::supervisor::command_argv;
use crate::supervisor::types::Host;

use super::http::{json_response, strict_body, HearthManager, HttpResult, ManagerHttpError};

pub fn shared_routes() -> Router<Arc<HearthManager>> {
    Router::new()
        .route("/v1/shared", get(get_shared))
        .route("/v1/shared/catalog", get(get_shared_catalog))
        .route("/v1/shared/attach", post(post_shared_attach))
        .route("/v1/shared/detach", post(post_shared_detach))
        .route("/v1/shared/install", post(post_shared_install))
        .route("/v1/shared/remove", post(post_shared_remove))
}

fn shared_ctx(manager: &HearthManager) -> HttpResult<Arc<SharedContext>> {
    manager.shared.clone().ok_or_else(|| {
        ManagerHttpError::new(
            StatusCode::NOT_FOUND,
            "not_shared",
            "This daemon does not manage shared services",
        )
    })
}

fn shared_err(e: SharedError) -> ManagerHttpError {
    ManagerHttpError::new(StatusCode::INTERNAL_SERVER_ERROR, "shared_error", e.0)
}

fn now() -> String {
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    crate::supervisor::types::format_iso8601_millis(millis)
}

/// `"postgres@16.4"` → `("postgres", "16.4")`. Versions may contain `.`/`_`; neither name nor
/// version may be empty or contain another `@`.
fn parse_instance_id(value: &Value) -> HttpResult<(String, String)> {
    let raw = value.as_str().unwrap_or("");
    let Some((name, version)) = raw.split_once('@') else {
        return Err(ManagerHttpError::new(
            StatusCode::BAD_REQUEST,
            "invalid_service",
            "service must be <name>@<version>",
        ));
    };
    if name.is_empty() || version.is_empty() || version.contains('@') {
        return Err(ManagerHttpError::new(
            StatusCode::BAD_REQUEST,
            "invalid_service",
            "service must be <name>@<version>",
        ));
    }
    Ok((name.to_string(), version.to_string()))
}

// -----------------------------------------------------------------------------------------
// Read endpoints
// -----------------------------------------------------------------------------------------

async fn get_shared(State(manager): State<Arc<HearthManager>>) -> Response {
    let ctx = match shared_ctx(&manager) {
        Ok(ctx) => ctx,
        Err(e) => return e.into_response(),
    };
    let states: HashMap<String, Value> = manager
        .service_states()
        .into_iter()
        .map(|s| {
            (
                s.service_id.clone(),
                json!({ "actualState": s.actual_state.as_wire_str(), "readiness": s.readiness }),
            )
        })
        .collect();
    let instances: Vec<Value> = ctx
        .registry
        .list()
        .iter()
        .map(|i| {
            let id = i.id();
            json!({
                "id": id,
                "name": i.name,
                "version": i.version,
                "port": i.port,
                "extraPorts": i.extra_ports,
                "installState": i.install_state.as_wire_str(),
                "installError": i.install_error,
                "state": states.get(&id).cloned().unwrap_or(Value::Null),
                "attachments": i.attachments.iter().map(|(pid, a)| json!({
                    "projectId": pid,
                    "projectRoot": a.project_root,
                    "provisioned": a.provisioned,
                    // Rendered per-project connection info — loopback-only daemon, same exposure
                    // level as the log/state endpoints.
                    "connection": a.connection,
                })).collect::<Vec<_>>(),
            })
        })
        .collect();
    json_response(json!({ "instances": instances }), StatusCode::OK)
}

async fn get_shared_catalog(State(manager): State<Arc<HearthManager>>) -> Response {
    let ctx = match shared_ctx(&manager) {
        Ok(ctx) => ctx,
        Err(e) => return e.into_response(),
    };
    match ctx.remote.load(false).await {
        Ok(doc) => json_response(json!({ "catalog": doc.as_ref() }), StatusCode::OK),
        Err(e) => shared_err(e).into_response(),
    }
}

// -----------------------------------------------------------------------------------------
// Mutations
// -----------------------------------------------------------------------------------------

async fn post_shared_attach(State(manager): State<Arc<HearthManager>>, bytes: Bytes) -> Response {
    match shared_attach(&manager, &bytes).await {
        Ok(body) => json_response(body, StatusCode::OK),
        Err(e) => e.into_response(),
    }
}

async fn post_shared_detach(State(manager): State<Arc<HearthManager>>, bytes: Bytes) -> Response {
    match shared_detach(&manager, &bytes).await {
        Ok(body) => json_response(body, StatusCode::OK),
        Err(e) => e.into_response(),
    }
}

async fn post_shared_install(State(manager): State<Arc<HearthManager>>, bytes: Bytes) -> Response {
    let ctx = match shared_ctx(&manager) {
        Ok(ctx) => ctx,
        Err(e) => return e.into_response(),
    };
    let body = match strict_body(&bytes, &["service"], &["service"]) {
        Ok(b) => b,
        Err(e) => return e.into_response(),
    };
    let (name, version) = match parse_instance_id(body.get("service").unwrap_or(&Value::Null)) {
        Ok(v) => v,
        Err(e) => return e.into_response(),
    };
    match ensure_registered_and_installed(&manager, &ctx, &name, &version).await {
        Ok(instance) => json_response(
            json!({ "service": instance.id(), "port": instance.port, "installState": instance.install_state.as_wire_str() }),
            StatusCode::OK,
        ),
        Err(e) => e.into_response(),
    }
}

async fn post_shared_remove(State(manager): State<Arc<HearthManager>>, bytes: Bytes) -> Response {
    let ctx = match shared_ctx(&manager) {
        Ok(ctx) => ctx,
        Err(e) => return e.into_response(),
    };
    let body = match strict_body(&bytes, &["service"], &["service"]) {
        Ok(b) => b,
        Err(e) => return e.into_response(),
    };
    let (name, version) = match parse_instance_id(body.get("service").unwrap_or(&Value::Null)) {
        Ok(v) => v,
        Err(e) => return e.into_response(),
    };
    let id = crate::shared::instance_id(&name, &version);
    let lock = ctx.instance_lock(&id);
    let _lock = lock.lock().await;
    let Some(instance) = ctx.registry.get(&id) else {
        return ManagerHttpError::new(
            StatusCode::NOT_FOUND,
            "unknown_shared_service",
            format!("{id} is not registered"),
        )
        .into_response();
    };
    if let Err(e) = manager.supervisor().stop(&id, None).await {
        return shared_err(SharedError(e.0)).into_response();
    }
    if let Err(e) = ctx.registry.remove(&id) {
        return shared_err(e).into_response();
    }
    if let Err(e) = sync_catalog(&manager, &ctx).await {
        return shared_err(e).into_response();
    }
    let _ = crate::file_io::remove_directory(&instance.install_dir(&ctx.root));
    let _ = crate::file_io::remove_directory(&instance.data_dir(&ctx.root));
    json_response(json!({ "removed": id }), StatusCode::OK)
}

// -----------------------------------------------------------------------------------------
// Orchestration
// -----------------------------------------------------------------------------------------

/// Re-synthesizes smp's catalog from the registry and swaps it in — `reload_catalog` handles
/// stopping removed-but-active services with the old catalog, so this is the only mutation path
/// for smp's service set.
async fn sync_catalog(
    manager: &Arc<HearthManager>,
    ctx: &SharedContext,
) -> Result<(), SharedError> {
    let catalog = synthesize_catalog(&ctx.root, &ctx.registry.list())?;
    manager
        .reload_catalog(catalog)
        .await
        .map(|_| ())
        .map_err(|errors| SharedError(errors.join("; ")))
}

/// Registers `name@version` in the registry if absent (fetching the remote recipe + allocating a
/// port), ensures it is installed, and makes sure the running catalog knows the service. Returns
/// the registry row; the caller holds the instance lock.
async fn ensure_registered_and_installed(
    manager: &Arc<HearthManager>,
    ctx: &Arc<SharedContext>,
    name: &str,
    version: &str,
) -> HttpResult<SharedInstance> {
    let id = crate::shared::instance_id(name, version);
    let instance = match ctx.registry.get(&id) {
        Some(instance) => instance,
        None => {
            let document = ctx.remote.load(true).await.map_err(|e| {
                ManagerHttpError::new(StatusCode::BAD_GATEWAY, "catalog_unavailable", e.0.clone())
            })?;
            let Some(recipe) = document.recipe(name, version).cloned() else {
                return Err(ManagerHttpError::new(
                    StatusCode::NOT_FOUND,
                    "unknown_shared_service",
                    format!("{id} is not in the shared catalog"),
                ));
            };
            let count = recipe.additional_ports.saturating_add(1);
            if count > MAX_SHARED_PORTS {
                return Err(ManagerHttpError::new(
                    StatusCode::BAD_REQUEST,
                    "too_many_ports",
                    format!("{id} asks for {count} ports; the maximum is {MAX_SHARED_PORTS}"),
                ));
            }
            let taken: HashSet<u16> = ctx
                .registry
                .list()
                .iter()
                .flat_map(|i| i.all_ports())
                .collect();
            let ports = allocate_ports(&id, count, &taken).await.ok_or_else(|| {
                ManagerHttpError::new(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "no_free_port",
                    "no free port block in the shared range",
                )
            })?;
            let port = ports[0];
            let extra_ports = ports[1..].to_vec();
            ctx.registry
                .update(|data| {
                    data.instances.insert(
                        id.clone(),
                        SharedInstance {
                            name: name.to_string(),
                            version: version.to_string(),
                            port,
                            extra_ports,
                            install_state: InstallState::Pending,
                            install_error: None,
                            recipe,
                            attachments: Default::default(),
                        },
                    );
                })
                .map_err(shared_err)?;
            sync_catalog(manager, ctx).await.map_err(shared_err)?;
            ctx.registry.get(&id).expect("just inserted")
        }
    };

    if crate::shared::install::is_installed(ctx, &instance) {
        if instance.install_state != InstallState::Installed {
            ctx.registry
                .update(|d| {
                    if let Some(i) = d.instances.get_mut(&id) {
                        i.install_state = InstallState::Installed;
                        i.install_error = None;
                    }
                })
                .map_err(shared_err)?;
        }
        return Ok(ctx.registry.get(&id).unwrap_or(instance));
    }

    ctx.registry
        .update(|d| {
            if let Some(i) = d.instances.get_mut(&id) {
                i.install_state = InstallState::Installing;
                i.install_error = None;
            }
        })
        .map_err(shared_err)?;
    manager.events.publish(
        "shared.install",
        json!({ "service": id, "installState": "installing" })
            .as_object()
            .unwrap()
            .clone(),
    );
    let progress = {
        let manager = manager.clone();
        let id = id.clone();
        move |line: &str| {
            manager.events.publish(
                "shared.install",
                json!({ "service": id, "installState": "installing", "message": line })
                    .as_object()
                    .unwrap()
                    .clone(),
            );
        }
    };
    match ensure_installed(ctx, &instance, progress).await {
        Ok(_) => {
            ctx.registry
                .update(|d| {
                    if let Some(i) = d.instances.get_mut(&id) {
                        i.install_state = InstallState::Installed;
                        i.install_error = None;
                    }
                })
                .map_err(shared_err)?;
            manager.events.publish(
                "shared.install",
                json!({ "service": id, "installState": "installed" })
                    .as_object()
                    .unwrap()
                    .clone(),
            );
            Ok(ctx.registry.get(&id).expect("just installed"))
        }
        Err(error) => {
            ctx.registry
                .update(|d| {
                    if let Some(i) = d.instances.get_mut(&id) {
                        i.install_state = InstallState::Failed;
                        i.install_error = Some(error.0.clone());
                    }
                })
                .map_err(shared_err)?;
            manager.events.publish(
                "shared.install",
                json!({ "service": id, "installState": "failed", "message": error.0 })
                    .as_object()
                    .unwrap()
                    .clone(),
            );
            Err(ManagerHttpError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "install_failed",
                error.0,
            ))
        }
    }
}

async fn shared_attach(manager: &Arc<HearthManager>, bytes: &Bytes) -> HttpResult<Value> {
    if manager.closing.load(std::sync::atomic::Ordering::SeqCst) {
        return Err(ManagerHttpError::new(
            StatusCode::CONFLICT,
            "manager_closing",
            "Manager is shutting down",
        ));
    }
    let ctx = shared_ctx(manager)?;
    let body = strict_body(
        bytes,
        &["requestId", "service", "projectRoot"],
        &["service"],
    )?;
    let (name, version) = parse_instance_id(body.get("service").unwrap_or(&Value::Null))?;
    let id = crate::shared::instance_id(&name, &version);
    let project_root = body.get("projectRoot").and_then(Value::as_str).map(|s| {
        PathBuf::from(s)
            .canonicalize()
            .unwrap_or_else(|_| PathBuf::from(s))
    });

    let lock = ctx.instance_lock(&id);
    let _lock = lock.lock().await;
    let instance = ensure_registered_and_installed(manager, &ctx, &name, &version).await?;
    // The synthesized run command uses dataDir as cwd — it must exist before spawn.
    std::fs::create_dir_all(instance.data_dir(&ctx.root))
        .map_err(|e| shared_err(SharedError(format!("cannot create data dir: {e}"))))?;

    // Start the instance. `supervisor().start` already awaits readiness, so on Ok the service is
    // Ready; a failure lands as `failed` in state.json and propagates as the attach error.
    let already_ready = manager
        .service_state(&id)
        .map(|s| s.actual_state == ActualServiceState::Ready)
        .unwrap_or(false);
    if !already_ready {
        manager.supervisor().start(&id, None).await.map_err(|e| {
            ManagerHttpError::new(StatusCode::INTERNAL_SERVER_ERROR, "start_failed", e.0)
        })?;
    }

    let mut result = json!({ "service": id, "port": instance.port });
    let Some(project_root) = project_root else {
        return Ok(result);
    };

    let pid = project_id(&project_root);
    if let Some(existing) = ctx
        .registry
        .get(&id)
        .and_then(|i| i.attachments.get(&pid).cloned())
    {
        if existing.provisioned {
            result["attachment"] = json!({ "projectId": pid, "projectRoot": project_root, "connection": existing.connection });
            return Ok(result);
        }
    }

    // Provision this project's logical resources (e.g. its own database+user). A failure is
    // recorded on the attachment so the next attach retries the commands — recipes must be
    // idempotent.
    let mut vars = instance_vars(&instance, &ctx.root);
    project_vars(&mut vars, &pid);
    let mut provision_error = None;
    for step in &instance.recipe.provision {
        let command = render_command(step, &vars);
        match run_recipe_command(&command, &instance.data_dir(&ctx.root)).await {
            Ok(0) => {}
            Ok(code) => {
                provision_error = Some(format!("provision command exited with {code}"));
                break;
            }
            Err(e) => {
                provision_error = Some(e.0);
                break;
            }
        }
    }

    let connection = match &instance.recipe.connection {
        Some(conn) if provision_error.is_none() => {
            Some(render_connection(conn, &vars).map_err(shared_err)?)
        }
        _ => None,
    };
    ctx.registry
        .update(|d| {
            if let Some(i) = d.instances.get_mut(&id) {
                i.attachments.insert(
                    pid.clone(),
                    SharedAttachment {
                        project_root: project_root.display().to_string(),
                        provisioned: provision_error.is_none(),
                        connection: connection.clone(),
                        error: provision_error.clone(),
                        attached_at: now(),
                    },
                );
            }
        })
        .map_err(shared_err)?;
    manager.events.publish(
        "shared.attach",
        json!({ "service": id, "projectId": pid, "provisioned": provision_error.is_none() })
            .as_object()
            .unwrap()
            .clone(),
    );
    if let Some(error) = provision_error {
        return Err(ManagerHttpError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "provision_failed",
            error,
        ));
    }
    result["attachment"] =
        json!({ "projectId": pid, "projectRoot": project_root, "connection": connection });
    Ok(result)
}

async fn shared_detach(manager: &Arc<HearthManager>, bytes: &Bytes) -> HttpResult<Value> {
    let ctx = shared_ctx(manager)?;
    let body = strict_body(
        bytes,
        &["service", "projectRoot"],
        &["service", "projectRoot"],
    )?;
    let (name, version) = parse_instance_id(body.get("service").unwrap_or(&Value::Null))?;
    let id = crate::shared::instance_id(&name, &version);
    let project_root = PathBuf::from(
        body.get("projectRoot")
            .and_then(Value::as_str)
            .unwrap_or_default(),
    )
    .canonicalize()
    .unwrap_or_else(|_| {
        PathBuf::from(
            body.get("projectRoot")
                .and_then(Value::as_str)
                .unwrap_or_default(),
        )
    });
    let pid = project_id(&project_root);

    let lock = ctx.instance_lock(&id);
    let _lock = lock.lock().await;
    let Some(instance) = ctx.registry.get(&id) else {
        return Err(ManagerHttpError::new(
            StatusCode::NOT_FOUND,
            "unknown_shared_service",
            format!("{id} is not registered"),
        ));
    };
    let Some(attachment) = instance.attachments.get(&pid).cloned() else {
        // Detaching what was never attached is a no-op — `stop` in a project must be idempotent.
        return Ok(json!({ "detached": id, "projectId": pid }));
    };

    // Best-effort deprovision: a failing teardown must not block the detach itself — the project
    // is leaving either way, and the attachment row is what the probe keys on.
    let mut vars = instance_vars(&instance, &ctx.root);
    project_vars(&mut vars, &pid);
    for step in &instance.recipe.deprovision {
        let command = render_command(step, &vars);
        let _ = run_recipe_command(&command, &instance.data_dir(&ctx.root)).await;
    }
    ctx.registry
        .update(|d| {
            if let Some(i) = d.instances.get_mut(&id) {
                i.attachments.remove(&pid);
            }
        })
        .map_err(shared_err)?;
    let _ = attachment;
    manager.events.publish(
        "shared.detach",
        json!({ "service": id, "projectId": pid })
            .as_object()
            .unwrap()
            .clone(),
    );
    Ok(json!({ "detached": id, "projectId": pid }))
}

/// Runs one rendered recipe command (provision/deprovision) with a bounded timeout and its output
/// discarded — provision output is not a service log; failures surface via the exit code.
async fn run_recipe_command(
    command: &crate::catalog::CommandSpec,
    cwd: &Path,
) -> Result<i32, SharedError> {
    let (argv, _) = command_argv(command);
    if argv.is_empty() {
        return Err(SharedError("empty recipe command".to_string()));
    }
    let mut cmd = tokio::process::Command::new(&argv[0]);
    cmd.args(&argv[1..])
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    let mut child = cmd
        .spawn()
        .map_err(|e| SharedError(format!("failed to spawn {:?}: {e}", argv[0])))?;
    match tokio::time::timeout(std::time::Duration::from_secs(60), child.wait()).await {
        Ok(Ok(status)) => Ok(status.code().unwrap_or(-1)),
        Ok(Err(e)) => Err(SharedError(format!("failed to wait on {:?}: {e}", argv[0]))),
        Err(_) => {
            let _ = child.kill().await;
            Err(SharedError(format!(
                "recipe command timed out: {:?}",
                argv[0]
            )))
        }
    }
}

// =============================================================================================
// End-to-end: a real smp manager, a real tarball install (file:// artifact), a real spawned
// service (nc listener), attach → ready + provisioned → detach. This is the same test category as
// `manager::http`'s full-lifecycle test — it proves the synthesized catalog, installer, port
// allocation and provisioning wiring all hang together behind the real HTTP surface.
// =============================================================================================
#[cfg(test)]
mod tests {
    use super::*;
    use crate::manager::http::{bootstrap, HearthManagerOptions};
    use crate::shared::synthesize::synthesize_catalog;
    use sha2::Digest;

    fn make_tarball(dir: &Path) -> (PathBuf, String) {
        let payload = dir.join("pkg/bin");
        std::fs::create_dir_all(&payload).unwrap();
        std::fs::write(payload.join("hello"), "world").unwrap();
        let archive = dir.join("pkg.tar.gz");
        let status = std::process::Command::new("tar")
            .arg("-czf")
            .arg(&archive)
            .arg("-C")
            .arg(dir)
            .arg("pkg")
            .status()
            .unwrap();
        assert!(status.success());
        let sha = sha2::Sha256::digest(std::fs::read(&archive).unwrap())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>();
        (archive, sha)
    }

    fn catalog_json(archive: &Path, sha: &str) -> String {
        json!({
            "version": 1,
            "services": {
                "fakesvc": {
                    "versions": {
                        "1.0": {
                            "artifacts": { "darwin-arm64": { "url": format!("file://{}", archive.display()), "sha256": sha } },
                            "run": { "argv": ["sh", "-c", "exec nc -lk {port}"] },
                            "readiness": { "kind": "command", "command": { "argv": ["nc", "-z", "127.0.0.1", "{port}"] } },
                            "provision": [ { "argv": ["sh", "-c", "echo provisioned > {dataDir}/{projectDb}.provisioned"] } ],
                            "connection": { "url": "fake://127.0.0.1:{port}/{projectDb}", "env": { "FAKE_URL": "{url}" } }
                        }
                    }
                }
            }
        })
        .to_string()
    }

    async fn boot_smp(root: &Path) -> Arc<HearthManager> {
        let ctx = SharedContext::open(root.to_path_buf(), None).unwrap();
        let catalog = synthesize_catalog(&ctx.root, &ctx.registry.list()).unwrap();
        bootstrap(HearthManagerOptions {
            runtime_directory: Some(ctx.runtime_directory()),
            root: Some(root.to_path_buf()),
            catalog,
            event_capacity: None,
            log_tail_bytes: None,
            log_max_bytes: None,
            log_rotation_count: None,
            supervisor: None,
            shared: Some(ctx),
        })
        .await
        .unwrap()
    }

    fn client() -> reqwest::Client {
        reqwest::Client::new()
    }

    fn authed(
        client: &reqwest::Client,
        manager: &HearthManager,
        method: reqwest::Method,
        path: &str,
    ) -> reqwest::RequestBuilder {
        client
            .request(method, format!("{}{}", manager.base_url(), path))
            .bearer_auth(manager.bearer_token())
            .header("x-hearth-protocol", "1")
    }

    #[tokio::test]
    async fn shared_attach_installs_starts_provisions_and_detaches() {
        let src = tempfile::tempdir().unwrap();
        let (archive, sha) = make_tarball(src.path());
        let shared_root = tempfile::tempdir().unwrap();
        // Seed the catalog cache — RemoteCatalog::load(false) reads it without any network.
        std::fs::write(
            shared_root.path().join("catalog.json"),
            catalog_json(&archive, &sha),
        )
        .unwrap();

        let manager = boot_smp(shared_root.path()).await;
        let http = client();
        let project = tempfile::tempdir().unwrap();

        let attach: Value = authed(&http, &manager, reqwest::Method::POST, "/v1/shared/attach")
            .json(&json!({ "service": "fakesvc@1.0", "projectRoot": project.path().display().to_string() }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(attach["service"], json!("fakesvc@1.0"), "{attach}");
        let conn_url = attach["attachment"]["connection"]["url"]
            .as_str()
            .unwrap_or("");
        let pid = project_id(&project.path().canonicalize().unwrap());
        assert!(conn_url.contains(&format!("/h_{pid}")), "{conn_url}");
        assert_eq!(
            attach["attachment"]["connection"]["env"]["FAKE_URL"],
            json!(conn_url)
        );

        // The instance is a real managed service in smp's own state.
        let services: Value = authed(&http, &manager, reqwest::Method::GET, "/v1/services")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let row = services["services"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["serviceId"] == "fakesvc@1.0")
            .unwrap();
        assert_eq!(row["actualState"], json!("ready"), "{row}");

        // The install actually landed under the isolated prefix.
        let install_dir = shared_root.path().join("installs/fakesvc/1.0");
        assert!(install_dir.join("bin/hello").exists());
        // Provision ran for this project.
        assert!(shared_root
            .path()
            .join(format!("instances/fakesvc@1.0/h_{pid}.provisioned"))
            .exists());

        // /v1/shared reports the attachment.
        let shared: Value = authed(&http, &manager, reqwest::Method::GET, "/v1/shared")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let inst = &shared["instances"][0];
        assert_eq!(inst["id"], json!("fakesvc@1.0"));
        assert_eq!(inst["attachments"][0]["projectId"], json!(pid));
        assert_eq!(inst["attachments"][0]["provisioned"], json!(true));

        // Re-attach is idempotent and returns the cached connection.
        let again: Value = authed(&http, &manager, reqwest::Method::POST, "/v1/shared/attach")
            .json(&json!({ "service": "fakesvc@1.0", "projectRoot": project.path().display().to_string() }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(again["attachment"]["connection"]["url"], json!(conn_url));

        // Detach removes only the attachment — the instance keeps running.
        authed(&http, &manager, reqwest::Method::POST, "/v1/shared/detach")
            .json(&json!({ "service": "fakesvc@1.0", "projectRoot": project.path().display().to_string() }))
            .send()
            .await
            .unwrap();
        let shared: Value = authed(&http, &manager, reqwest::Method::GET, "/v1/shared")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert!(shared["instances"][0]["attachments"]
            .as_array()
            .unwrap()
            .is_empty());
        let services: Value = authed(&http, &manager, reqwest::Method::GET, "/v1/services")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let row = services["services"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["serviceId"] == "fakesvc@1.0")
            .unwrap();
        assert_eq!(
            row["actualState"],
            json!("ready"),
            "detach must not stop the shared instance"
        );

        manager.close().await;
    }

    #[tokio::test]
    async fn attach_rejects_an_unknown_version() {
        let shared_root = tempfile::tempdir().unwrap();
        std::fs::write(
            shared_root.path().join("catalog.json"),
            json!({ "version": 1, "services": {} }).to_string(),
        )
        .unwrap();
        let manager = boot_smp(shared_root.path()).await;
        let http = client();
        let resp = authed(&http, &manager, reqwest::Method::POST, "/v1/shared/attach")
            .json(&json!({ "service": "nope@1.0", "projectRoot": "/tmp/x" }))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        let body: Value = resp.json().await.unwrap();
        assert_eq!(body["error"]["code"], json!("unknown_shared_service"));
        manager.close().await;
    }
}
