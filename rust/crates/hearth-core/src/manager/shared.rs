//! The smp-only HTTP surface (`/v1/shared/*`) — mounted by `router()` when the manager was built
//! with a `SharedContext`. Orchestrates register → install → catalog-insert → start →
//! provision per `docs/shared-services.md`. All mutations serialize on a per-instance lock so two
//! projects racing `postgres@16.4` converge on one install.
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::{get, post};
use axum::Router;
use serde_json::{json, Map, Value};

use crate::shared::install::ensure_installed;
use crate::shared::ports::{allocate_ports, bind_all, MAX_SHARED_PORTS};
use crate::shared::registry::{InstallState, SharedAttachment, SharedInstance};
use crate::shared::render::{instance_vars, project_vars, render_command_checked, render_connection};
use crate::shared::synthesize::synthesize_catalog;
use crate::shared::{output_with_timeout, project_id, SharedContext, SharedError};
use crate::state::ActualServiceState;
use crate::supervisor::command_argv;
use crate::supervisor::types::Host;

use super::http::{ensure_not_closing, json_response, now, parse_bool_flag, strict_body, HearthManager, HttpResult, ManagerHttpError};

const RECIPE_COMMAND_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

pub fn shared_routes() -> Router<Arc<HearthManager>> {
    Router::new()
        .route("/v1/shared", get(get_shared))
        .route("/v1/shared/catalog", get(get_shared_catalog))
        .route("/v1/shared/attach", post(post_shared_attach))
        .route("/v1/shared/detach", post(post_shared_detach))
        .route("/v1/shared/install", post(post_shared_install))
        .route("/v1/shared/remove", post(post_shared_remove))
}

/// `router()` only mounts these routes when the manager has a `SharedContext`, so this cannot fail
/// in practice — it stays an error rather than a panic all the same.
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

fn publish(manager: &HearthManager, event_type: &str, data: Value) {
    manager
        .events
        .publish(event_type, data.as_object().cloned().unwrap_or_default());
}

fn publish_install(manager: &HearthManager, id: &str, state: InstallState, message: Option<&str>) {
    let mut data = json!({ "service": id, "installState": state.as_wire_str() });
    if let Some(message) = message {
        data["message"] = json!(message);
    }
    publish(manager, "shared.install", data);
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

/// The body's `projectRoot`, canonicalized when it exists. An empty string is a 400 rather than
/// `PathBuf::from("")` — that would hash the daemon's own cwd into a project id.
fn parse_project_root(body: &Map<String, Value>) -> HttpResult<Option<PathBuf>> {
    match body.get("projectRoot") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) if !s.is_empty() => {
            let path = PathBuf::from(s);
            Ok(Some(path.canonicalize().unwrap_or(path)))
        }
        _ => Err(ManagerHttpError::new(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "projectRoot must be a non-empty string",
        )),
    }
}

// -----------------------------------------------------------------------------------------
// Read endpoints
// -----------------------------------------------------------------------------------------

async fn get_shared(State(manager): State<Arc<HearthManager>>) -> HttpResult<Response> {
    let ctx = shared_ctx(&manager)?;
    let states: std::collections::HashMap<String, Value> = manager
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
    Ok(json_response(json!({ "instances": instances }), StatusCode::OK))
}

async fn get_shared_catalog(State(manager): State<Arc<HearthManager>>) -> HttpResult<Response> {
    let ctx = shared_ctx(&manager)?;
    let doc = ctx.remote.load(false).await.map_err(shared_err)?;
    Ok(json_response(json!({ "catalog": doc.as_ref() }), StatusCode::OK))
}

// -----------------------------------------------------------------------------------------
// Mutations
// -----------------------------------------------------------------------------------------

async fn post_shared_attach(State(manager): State<Arc<HearthManager>>, bytes: Bytes) -> HttpResult<Response> {
    Ok(json_response(shared_attach(&manager, &bytes).await?, StatusCode::OK))
}

async fn post_shared_detach(State(manager): State<Arc<HearthManager>>, bytes: Bytes) -> HttpResult<Response> {
    Ok(json_response(shared_detach(&manager, &bytes).await?, StatusCode::OK))
}

async fn post_shared_install(State(manager): State<Arc<HearthManager>>, bytes: Bytes) -> HttpResult<Response> {
    ensure_not_closing(&manager)?;
    let ctx = shared_ctx(&manager)?;
    let body = strict_body(&bytes, &["service"], &["service"])?;
    let (name, version) = parse_instance_id(body.get("service").unwrap_or(&Value::Null))?;
    let id = crate::shared::instance_id(&name, &version);
    let lock = ctx.instance_lock(&id);
    let _lock = lock.lock().await;
    let instance = ensure_registered_and_installed(&manager, &ctx, &name, &version).await?;
    Ok(json_response(
        json!({ "service": instance.id(), "port": instance.port, "installState": instance.install_state.as_wire_str() }),
        StatusCode::OK,
    ))
}

/// Stops the instance, unregisters it and deletes its install and data directories. The data
/// directory holds every attached project's databases, so an instance with attachments is refused
/// (409 `shared_service_attached`) unless the caller sends `force: true` after its own explicit
/// confirmation.
async fn post_shared_remove(State(manager): State<Arc<HearthManager>>, bytes: Bytes) -> HttpResult<Response> {
    ensure_not_closing(&manager)?;
    let ctx = shared_ctx(&manager)?;
    let body = strict_body(&bytes, &["service", "force"], &["service"])?;
    let (name, version) = parse_instance_id(body.get("service").unwrap_or(&Value::Null))?;
    let force = parse_bool_flag(&body, "force")?;
    let id = crate::shared::instance_id(&name, &version);
    let lock = ctx.instance_lock(&id);
    let _lock = lock.lock().await;
    let Some(instance) = ctx.registry.get(&id) else {
        return Err(ManagerHttpError::new(
            StatusCode::NOT_FOUND,
            "unknown_shared_service",
            format!("{id} is not registered"),
        ));
    };
    if !force && !instance.attachments.is_empty() {
        let projects: Vec<&str> = instance.attachments.values().map(|a| a.project_root.as_str()).collect();
        return Err(ManagerHttpError::new(
            StatusCode::CONFLICT,
            "shared_service_attached",
            format!(
                "{id} is attached to {} project(s) ({}); removing it deletes their data — retry with force: true to remove anyway",
                projects.len(),
                projects.join(", ")
            ),
        ));
    }
    manager
        .supervisor()
        .stop(&id, None)
        .await
        .map_err(|e| shared_err(SharedError(e.0)))?;
    ctx.registry.remove(&id).map_err(shared_err)?;
    sync_catalog(&manager, &ctx).await.map_err(shared_err)?;
    let (install_dir, data_dir) = (instance.install_dir(&ctx.root), instance.data_dir(&ctx.root));
    let _ = tokio::task::spawn_blocking(move || {
        let _ = crate::file_io::remove_directory(&install_dir);
        let _ = crate::file_io::remove_directory(&data_dir);
    })
    .await;
    Ok(json_response(json!({ "removed": id }), StatusCode::OK))
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
        .map_err(|error| SharedError(error.to_string()))
}

/// Picks the new instance's ports and inserts its registry row. Runs under the machine-wide
/// `port_allocation` lock: per-instance locks don't serialize two *different* new ids, and two
/// of those whose hash slots are close could otherwise both pass the bind probe and share a port.
async fn register_instance(
    ctx: &SharedContext,
    id: &str,
    name: &str,
    version: &str,
    recipe: crate::shared::SharedRecipe,
) -> HttpResult<()> {
    let _allocation = ctx.port_allocation.lock().await;
    let ports = if !recipe.ports.is_empty() {
        // Pinned ports: the recipe IS its ports — nothing to probe, just a free check.
        if recipe.ports.len() > MAX_SHARED_PORTS as usize {
            return Err(ManagerHttpError::new(
                StatusCode::BAD_REQUEST,
                "too_many_ports",
                format!("{id} pins {} ports; the maximum is {MAX_SHARED_PORTS}", recipe.ports.len()),
            ));
        }
        if !bind_all(&recipe.ports).await {
            return Err(ManagerHttpError::new(
                StatusCode::CONFLICT,
                "port_in_use",
                format!("{id} cannot pin {:?}; one or more are in use", recipe.ports),
            ));
        }
        recipe.ports.clone()
    } else {
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
        allocate_ports(id, count, &taken).await.ok_or_else(|| {
            ManagerHttpError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "no_free_port",
                "no free port block in the shared range",
            )
        })?
    };
    let port = ports[0];
    let extra_ports = ports[1..].to_vec();
    ctx.registry
        .update(|data| {
            data.instances.insert(
                id.to_string(),
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
        .map_err(shared_err)
}

fn set_install_state(ctx: &SharedContext, id: &str, state: InstallState, error: Option<String>) -> HttpResult<()> {
    ctx.registry
        .update(|d| {
            if let Some(i) = d.instances.get_mut(id) {
                i.install_state = state;
                i.install_error = error;
            }
        })
        .map_err(shared_err)
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
            register_instance(ctx, &id, name, version, recipe).await?;
            sync_catalog(manager, ctx).await.map_err(shared_err)?;
            ctx.registry.get(&id).expect("just inserted")
        }
    };

    if crate::shared::install::is_installed(ctx, &instance) {
        if instance.install_state != InstallState::Installed {
            set_install_state(ctx, &id, InstallState::Installed, None)?;
        }
        return Ok(ctx.registry.get(&id).unwrap_or(instance));
    }

    set_install_state(ctx, &id, InstallState::Installing, None)?;
    publish_install(manager, &id, InstallState::Installing, None);
    let progress = {
        let manager = manager.clone();
        let id = id.clone();
        move |line: &str| publish_install(&manager, &id, InstallState::Installing, Some(line))
    };
    match ensure_installed(ctx, &instance, progress).await {
        Ok(_) => {
            set_install_state(ctx, &id, InstallState::Installed, None)?;
            publish_install(manager, &id, InstallState::Installed, None);
            Ok(ctx.registry.get(&id).expect("just installed"))
        }
        Err(error) => {
            set_install_state(ctx, &id, InstallState::Failed, Some(error.0.clone()))?;
            publish_install(manager, &id, InstallState::Failed, Some(&error.0));
            Err(ManagerHttpError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "install_failed",
                error.0,
            ))
        }
    }
}

async fn shared_attach(manager: &Arc<HearthManager>, bytes: &Bytes) -> HttpResult<Value> {
    ensure_not_closing(manager)?;
    let ctx = shared_ctx(manager)?;
    let body = strict_body(bytes, &["service", "projectRoot", "args"], &["service"])?;
    let attach_args: Vec<String> = match body.get("args") {
        None | Some(Value::Null) => Vec::new(),
        Some(v) => match v.as_array() {
            Some(items) if items.iter().all(Value::is_string) => items.iter().filter_map(|s| s.as_str().map(str::to_string)).collect(),
            _ => {
                return Err(ManagerHttpError::new(
                    StatusCode::BAD_REQUEST,
                    "invalid_request",
                    "args must be an array of strings",
                ))
            }
        },
    };
    let (name, version) = parse_instance_id(body.get("service").unwrap_or(&Value::Null))?;
    let id = crate::shared::instance_id(&name, &version);
    let project_root = parse_project_root(&body)?;

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

    // Every template is rendered and checked before anything runs: a typo'd `{projectDB}` must
    // fail the attach up front, not run literally — and a bad connection template must not wait
    // until after provisioning, where its failure would skip recording the attachment and re-run
    // provisioning on every attach.
    let mut vars = instance_vars(&instance, &ctx.root);
    project_vars(&mut vars, &pid);
    vars.insert("projectRoot".to_string(), project_root.display().to_string());
    let invalid_recipe = |e: SharedError| ManagerHttpError::new(StatusCode::INTERNAL_SERVER_ERROR, "invalid_recipe", e.0);
    let mut commands = Vec::with_capacity(instance.recipe.provision.len());
    for step in &instance.recipe.provision {
        commands.push(render_command_checked(step, &vars, "provision").map_err(invalid_recipe)?);
    }
    let rendered_connection = instance
        .recipe
        .connection
        .as_ref()
        .map(|conn| render_connection(conn, &vars))
        .transpose()
        .map_err(invalid_recipe)?;

    // Provision this project's logical resources (e.g. its own database+user). A failure is
    // recorded on the attachment so the next attach retries the commands — recipes must be
    // idempotent.
    let mut provision_error = None;
    for mut command in commands {
        // Attach args append to the rendered argv — e.g. the shared nginx recipe takes the
        // attaching project's rendered conf directory as a trailing argument.
        if !attach_args.is_empty() {
            match &mut command {
                crate::catalog::CommandSpec::Argv { argv } => argv.extend(attach_args.iter().cloned()),
                crate::catalog::CommandSpec::Shell { .. } => {
                    provision_error = Some("attach args require argv provision commands".to_string());
                    break;
                }
            }
        }
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

    let connection = rendered_connection.filter(|_| provision_error.is_none());
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
    publish(
        manager,
        "shared.attach",
        json!({ "service": id, "projectId": pid, "provisioned": provision_error.is_none() }),
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
    ensure_not_closing(manager)?;
    let ctx = shared_ctx(manager)?;
    let body = strict_body(
        bytes,
        &["service", "projectRoot"],
        &["service", "projectRoot"],
    )?;
    let (name, version) = parse_instance_id(body.get("service").unwrap_or(&Value::Null))?;
    let id = crate::shared::instance_id(&name, &version);
    let Some(project_root) = parse_project_root(&body)? else {
        return Err(ManagerHttpError::new(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "projectRoot must be a non-empty string",
        ));
    };
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
    if !instance.attachments.contains_key(&pid) {
        // Detaching what was never attached is a no-op — `stop` in a project must be idempotent.
        return Ok(json!({ "detached": id, "projectId": pid }));
    }

    // Best-effort deprovision: a failing teardown must not block the detach itself — the project
    // is leaving either way, and the attachment row is what the probe keys on. A step whose
    // template doesn't render is skipped rather than run with a literal `{var}` in it.
    let mut vars = instance_vars(&instance, &ctx.root);
    project_vars(&mut vars, &pid);
    vars.insert("projectRoot".to_string(), project_root.display().to_string());
    for step in &instance.recipe.deprovision {
        if let Ok(command) = render_command_checked(step, &vars, "deprovision") {
            let _ = run_recipe_command(&command, &instance.data_dir(&ctx.root)).await;
        }
    }
    ctx.registry
        .update(|d| {
            if let Some(i) = d.instances.get_mut(&id) {
                i.attachments.remove(&pid);
            }
        })
        .map_err(shared_err)?;
    publish(manager, "shared.detach", json!({ "service": id, "projectId": pid }));
    Ok(json!({ "detached": id, "projectId": pid }))
}

/// Runs one rendered recipe command (provision/deprovision) with a bounded timeout and its output
/// discarded — provision output is not a service log; failures surface via the exit code. A
/// timeout kills the command's whole process group, not just the direct child.
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
    match output_with_timeout(cmd, RECIPE_COMMAND_TIMEOUT).await {
        Ok(Some(output)) => Ok(output.status.code().unwrap_or(-1)),
        Ok(None) => Err(SharedError(format!(
            "recipe command timed out: {:?}",
            argv[0]
        ))),
        Err(e) => Err(SharedError(format!("failed to spawn {:?}: {e}", argv[0]))),
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
    use crate::state::PROTOCOL_VERSION;
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

    /// The catalog URL is the seeded `catalog.json` itself, so the test never reaches the network
    /// (the pinned URL may well answer, with a catalog that lacks the fixture recipes).
    async fn boot_smp(root: &Path) -> Arc<HearthManager> {
        let url = format!("file://{}", root.join("catalog.json").display());
        let ctx = SharedContext::open(root.to_path_buf(), Some(url)).unwrap();
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
            .header("x-hearth-protocol", PROTOCOL_VERSION.to_string())
    }

    #[tokio::test]
    async fn shared_attach_installs_starts_provisions_and_detaches() {
        let src = tempfile::tempdir().unwrap();
        let (archive, sha) = make_tarball(src.path());
        let shared_root = tempfile::tempdir().unwrap();
        // Seed the catalog — `boot_smp` points the catalog URL at this file.
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
    async fn remove_refuses_an_attached_instance_unless_forced() {
        let shared_root = tempfile::tempdir().unwrap();
        std::fs::write(
            shared_root.path().join("catalog.json"),
            json!({ "version": 1, "services": {} }).to_string(),
        )
        .unwrap();
        {
            let ctx = SharedContext::open(shared_root.path().to_path_buf(), None).unwrap();
            let recipe: crate::shared::SharedRecipe = serde_json::from_value(json!({
                "artifacts": {},
                "run": { "argv": ["true"] },
                "readiness": { "kind": "process" }
            }))
            .unwrap();
            ctx.registry
                .update(|d| {
                    d.instances.insert(
                        "fakesvc@1.0".to_string(),
                        SharedInstance {
                            name: "fakesvc".to_string(),
                            version: "1.0".to_string(),
                            port: 43999,
                            extra_ports: vec![],
                            install_state: InstallState::Installed,
                            install_error: None,
                            recipe,
                            attachments: [(
                                "abc".to_string(),
                                SharedAttachment {
                                    project_root: "/tmp/project".to_string(),
                                    provisioned: true,
                                    connection: None,
                                    error: None,
                                    attached_at: now(),
                                },
                            )]
                            .into(),
                        },
                    );
                })
                .unwrap();
        }
        let manager = boot_smp(shared_root.path()).await;
        let http = client();

        let refused = authed(&http, &manager, reqwest::Method::POST, "/v1/shared/remove")
            .json(&json!({ "service": "fakesvc@1.0" }))
            .send()
            .await
            .unwrap();
        assert_eq!(refused.status(), StatusCode::CONFLICT);
        let body: Value = refused.json().await.unwrap();
        assert_eq!(body["error"]["code"], json!("shared_service_attached"));
        assert!(manager.shared.as_ref().unwrap().registry.get("fakesvc@1.0").is_some());

        let forced = authed(&http, &manager, reqwest::Method::POST, "/v1/shared/remove")
            .json(&json!({ "service": "fakesvc@1.0", "force": true }))
            .send()
            .await
            .unwrap();
        assert_eq!(forced.status(), StatusCode::OK);
        assert!(manager.shared.as_ref().unwrap().registry.get("fakesvc@1.0").is_none());
        manager.close().await;
    }

    #[tokio::test]
    async fn attach_rejects_an_empty_project_root() {
        let shared_root = tempfile::tempdir().unwrap();
        std::fs::write(
            shared_root.path().join("catalog.json"),
            json!({ "version": 1, "services": {} }).to_string(),
        )
        .unwrap();
        let manager = boot_smp(shared_root.path()).await;
        let http = client();
        for path in ["/v1/shared/attach", "/v1/shared/detach"] {
            let resp = authed(&http, &manager, reqwest::Method::POST, path)
                .json(&json!({ "service": "nope@1.0", "projectRoot": "" }))
                .send()
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "{path}");
        }
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
