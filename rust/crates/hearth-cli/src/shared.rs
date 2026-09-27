//! `hearthd shared …` — the CLI surface for the machine-global smp daemon (`docs/shared-services.md`).
//! These commands deliberately do not require a project `hearth.yaml`: `attach`/`detach`/`probe`
//! derive the project identity from `--root`/cwd so the generated `shared:` service commands work
//! in any directory a project daemon runs them from.
use std::path::Path;
use std::sync::Arc;

use serde_json::{json, Value};

use hearth_core::catalog::{ServiceCatalog, StartFailurePolicy};
use hearth_core::shared::{project_id, registry::SharedRegistry, shared_root, RemoteCatalog, SHARED_RUNTIME_DIRECTORY_NAME};
use hearth_core::file_io::create_file_io;

use crate::{
    discover, ensure, fail_err, parse_command_flags, request, request_with_timeout, usage_err, wait_operation, Client, Discovery, FlagName, Io,
    LocalctlOptions, LocalctlResult, EXIT_UNAVAILABLE,
};

/// The catalog `discover()`/`ensure()` run against for smp — no services (they're synthesized
/// daemon-side); only `runtime_directory` matters, pointing the lock/token search at
/// `~/.hearth/shared/runtime-v1`.
fn smp_catalog() -> ServiceCatalog {
    ServiceCatalog {
        services: vec![],
        groups: Default::default(),
        group_tree: Vec::new(),
        compose_file: None,
        runtime_directory: Some(SHARED_RUNTIME_DIRECTORY_NAME.to_string()),
        start_failure_policy: StartFailurePolicy::StopOnFirstFailureKeepStarted,
        private_file_guard: None,
    }
}

pub async fn discover_smp() -> Discovery {
    discover(&shared_root(), &smp_catalog()).await
}

/// Live client for smp, spawning `hearthd smp` (via `spawn_smp`) when none is running.
pub async fn ensure_smp(spawn_smp: &Arc<dyn Fn(&Path) + Send + Sync>) -> LocalctlResult<Client> {
    let spawn = spawn_smp.clone();
    let options = LocalctlOptions {
        catalog: smp_catalog(),
        spawn_daemon: Box::new(move |path: &Path| spawn(path)),
        doctor_checks: None,
    };
    ensure(&shared_root(), &options).await
}

/// `discover` that never spawns — for read-only commands (`status`, `probe`) that must not bring a
/// daemon up as a side effect.
fn live_smp_client(discovery: Discovery) -> Option<Client> {
    match discovery {
        Discovery::Live { client } => Some(client),
        _ => None,
    }
}

async fn smp_request(client: &Client, path: &str, method: reqwest::Method, body: Option<&Value>) -> Result<Value, String> {
    request(client, path, method, body, None).await
}

/// Long-running smp calls (attach/install may download+extract a tarball on first use).
async fn smp_request_slow(client: &Client, path: &str, body: &Value) -> Result<Value, String> {
    request_with_timeout(client, path, reqwest::Method::POST, Some(body), None, None).await
}

fn parse_shared_id(raw: Option<&String>) -> LocalctlResult<String> {
    let Some(id) = raw else {
        return usage_err("expected <name>@<version>");
    };
    let Some((name, version)) = id.split_once('@') else {
        return usage_err(format!("expected <name>@<version>, got {id}"));
    };
    if name.is_empty() || version.is_empty() || version.contains('@') {
        return usage_err(format!("expected <name>@<version>, got {id}"));
    }
    Ok(id.clone())
}

/// `smp` `/v1/shared` instance row for `id`, or None (unknown id / smp down).
async fn shared_instance(client: &Client, id: &str) -> Option<Value> {
    let body = smp_request(client, "/v1/shared", reqwest::Method::GET, None).await.ok()?;
    body["instances"].as_array()?.iter().find(|i| i["id"].as_str() == Some(id)).cloned()
}

pub async fn run(root: &Path, args: &[String], io: &mut Io<'_>, spawn_smp: Arc<dyn Fn(&Path) + Send + Sync>) -> i32 {
    match run_inner(root, args, io, &spawn_smp).await {
        Ok(code) => code,
        Err(error) => {
            (io.err)(&error.message);
            error.exit_code
        }
    }
}

async fn run_inner(root: &Path, args: &[String], io: &mut Io<'_>, spawn_smp: &Arc<dyn Fn(&Path) + Send + Sync>) -> LocalctlResult<i32> {
    let flags = parse_command_flags(args, &[FlagName::Json])?;
    let Some(subcommand) = flags.positionals.first().cloned() else {
        return usage_err("usage: hearthd shared ensure|list|installed|status|attach|detach|probe|install|start|stop|remove [<name>@<version>] [attach-args...] [--json]");
    };
    let rest = &flags.positionals[1..];
    match subcommand.as_str() {
        // The `manager ensure --json` contract for smp — find-or-start the shared daemon and print
        // the connection a client (the macOS app) needs to talk to it directly.
        "ensure" => {
            let client = ensure_smp(spawn_smp).await?;
            (io.out)(&crate::print_value(&crate::ensure_payload(&client), flags.json));
            Ok(0)
        }
        // The remote registry — readable without smp running.
        "list" => {
            let remote = RemoteCatalog::new(&shared_root(), std::env::var("HEARTH_SHARED_CATALOG_URL").ok());
            let doc = remote.load(true).await.map_err(|e| crate::LocalctlError { exit_code: EXIT_UNAVAILABLE, message: e.0 })?;
            if flags.json {
                (io.out)(&serde_json::to_string_pretty(&doc.as_ref()).unwrap());
            } else {
                for (name, family) in &doc.services {
                    let mut versions: Vec<&String> = family.versions.keys().collect();
                    versions.sort();
                    (io.out)(&format!("{name}  {}", versions.iter().map(|v| v.as_str()).collect::<Vec<_>>().join(", ")));
                }
            }
            Ok(0)
        }
        // The local registry — what this machine has installed/running.
        "installed" => {
            let io_files = create_file_io(false);
            let registry = SharedRegistry::load(std::sync::Arc::from(io_files), &shared_root()).map_err(|e| crate::LocalctlError { exit_code: EXIT_UNAVAILABLE, message: e.0 })?;
            let instances = registry.list();
            if flags.json {
                (io.out)(&serde_json::to_string_pretty(&json!({ "instances": instances })).unwrap());
            } else {
                for i in &instances {
                    (io.out)(&format!("{}  port {}  {}  ({} project(s) attached)", i.id(), i.port, i.install_state.as_wire_str(), i.attachments.len()));
                }
            }
            Ok(0)
        }
        "status" => {
            match live_smp_client(discover_smp().await) {
                None => {
                    (io.err)("smp is not running");
                    Ok(EXIT_UNAVAILABLE)
                }
                Some(client) => {
                    let body = smp_request(&client, "/v1/shared", reqwest::Method::GET, None).await.or_else(|e| fail_err(crate::EXIT_UNAVAILABLE, e))?;
                    if flags.json {
                        (io.out)(&serde_json::to_string_pretty(&body).unwrap());
                    } else {
                        for i in body["instances"].as_array().cloned().unwrap_or_default() {
                            let state = i["state"]["actualState"].as_str().unwrap_or("stopped");
                            (io.out)(&format!("{}  {}  port {}  {}", i["id"].as_str().unwrap_or(""), state, i["port"], i["installState"].as_str().unwrap_or("")));
                        }
                    }
                    Ok(0)
                }
            }
        }
        "attach" => {
            let id = parse_shared_id(rest.first())?;
            // Trailing positionals are forwarded to the recipe's provision argv (e.g. the shared
            // nginx recipe takes the project's rendered conf directory).
            let attach_args: Vec<String> = rest.iter().skip(1).cloned().collect();
            let client = ensure_smp(spawn_smp).await?;
            let body = json!({ "service": id, "projectRoot": root, "args": attach_args });
            let result = smp_request_slow(&client, "/v1/shared/attach", &body).await.map_err(|e| crate::LocalctlError { exit_code: crate::EXIT_FAILED, message: e })?;
            if flags.json {
                (io.out)(&serde_json::to_string_pretty(&result).unwrap());
            } else {
                // Human/agent-readable summary — the attach run command's stdout lands in the
                // project's service log, so keep it terse but complete.
                (io.out)(&format!("attached {}", result["service"].as_str().unwrap_or(&id)));
                if let Some(conn) = result["attachment"]["connection"].as_object() {
                    for (k, v) in conn {
                        (io.out)(&format!("  {k}: {}", v.as_str().unwrap_or(&v.to_string())));
                    }
                }
            }
            Ok(0)
        }
        "detach" => {
            let id = parse_shared_id(rest.first())?;
            let Some(client) = live_smp_client(discover_smp().await) else {
                // smp down means the probe already reports not-ready — detach is a no-op.
                return Ok(0);
            };
            let body = json!({ "service": id, "projectRoot": root });
            smp_request(&client, "/v1/shared/detach", reqwest::Method::POST, Some(&body)).await.map_err(|e| crate::LocalctlError { exit_code: crate::EXIT_FAILED, message: e })?;
            (io.out)(&format!("detached {id}"));
            Ok(0)
        }
        // The readiness probe the generated `shared:` service polls: exit 0 iff the instance is
        // ready AND this project is attached. Silent — its output would land in probe noise.
        "probe" => {
            let id = parse_shared_id(rest.first())?;
            let Some(client) = live_smp_client(discover_smp().await) else {
                return Ok(1);
            };
            let pid = project_id(root);
            let ready = shared_instance(&client, &id).await.is_some_and(|i| {
                i["state"]["actualState"].as_str() == Some("ready")
                    && i["attachments"].as_array().map(|a| {
                        a.iter().any(|att| att["projectId"].as_str() == Some(pid.as_str()) && att["provisioned"].as_bool() == Some(true))
                    }).unwrap_or(false)
            });
            Ok(if ready { 0 } else { 1 })
        }
        "install" => {
            let id = parse_shared_id(rest.first())?;
            let client = ensure_smp(spawn_smp).await?;
            let result = smp_request_slow(&client, "/v1/shared/install", &json!({ "service": id })).await.map_err(|e| crate::LocalctlError { exit_code: crate::EXIT_FAILED, message: e })?;
            (io.out)(&format!("installed {} (port {})", result["service"].as_str().unwrap_or(&id), result["port"]));
            Ok(0)
        }
        "start" => {
            let id = parse_shared_id(rest.first())?;
            let client = ensure_smp(spawn_smp).await?;
            // install first (the service only enters smp's catalog once registered), then drive a
            // normal service start through the operations API.
            smp_request_slow(&client, "/v1/shared/install", &json!({ "service": id })).await.map_err(|e| crate::LocalctlError { exit_code: crate::EXIT_FAILED, message: e })?;
            let body = json!({ "requestId": uuid::Uuid::new_v4().to_string(), "serviceId": id, "action": "start" });
            let accepted = smp_request(&client, "/v1/operations", reqwest::Method::POST, Some(&body)).await.map_err(|e| crate::LocalctlError { exit_code: crate::EXIT_FAILED, message: e })?;
            let op_id = accepted["operation"]["id"].as_str().unwrap_or_default().to_string();
            let operation = wait_operation(&client, &op_id).await?;
            (io.out)(&format!("{} {} {}", crate::operation_status_str(operation.status), id, operation.id));
            if operation.status == hearth_core::state::OperationStatus::Failed {
                return fail_err(crate::EXIT_FAILED, "service operation failed");
            }
            Ok(0)
        }
        "stop" => {
            let id = parse_shared_id(rest.first())?;
            let Some(client) = live_smp_client(discover_smp().await) else {
                return fail_err(crate::EXIT_UNAVAILABLE, "smp is not running");
            };
            let body = json!({ "requestId": uuid::Uuid::new_v4().to_string(), "serviceId": id, "action": "stop" });
            let accepted = smp_request(&client, "/v1/operations", reqwest::Method::POST, Some(&body)).await.map_err(|e| crate::LocalctlError { exit_code: crate::EXIT_FAILED, message: e })?;
            let op_id = accepted["operation"]["id"].as_str().unwrap_or_default().to_string();
            let operation = wait_operation(&client, &op_id).await?;
            (io.out)(&format!("{} {} {}", crate::operation_status_str(operation.status), id, operation.id));
            Ok(0)
        }
        "remove" => {
            let id = parse_shared_id(rest.first())?;
            let Some(client) = live_smp_client(discover_smp().await) else {
                return fail_err(crate::EXIT_UNAVAILABLE, "smp is not running");
            };
            smp_request(&client, "/v1/shared/remove", reqwest::Method::POST, Some(&json!({ "service": id }))).await.map_err(|e| crate::LocalctlError { exit_code: crate::EXIT_FAILED, message: e })?;
            (io.out)(&format!("removed {id}"));
            Ok(0)
        }
        other => usage_err(format!("unknown shared subcommand: {other}")),
    }
}

// Re-exported for main.rs's generated shared-service commands and the MCP tools.
pub use hearth_core::shared::shared_root as root_dir;
