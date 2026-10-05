//! The default `HearthMcpClient`, backed by the daemon's HTTP API through `hearth-cli`'s
//! `ManagerClient` — the same typed client the CLI and TUI use.
use std::path::PathBuf;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Value};

use hearth_cli::{
    encode_path_segment, request as cli_request, restart_manager, runnable_targets, stop_manager,
    Discovery, LocalctlOptions, ManagerClient,
};
use hearth_core::catalog::ServiceCatalog;
use hearth_core::shared::{project_id, shared_root, RemoteCatalog};
use hearth_core::state::{OperationStatus, ServiceOperationKind};

/// How long `manage` waits for its operation before handing the agent the operation id to
/// `trace` instead — an MCP tool call must not hang for a whole readiness timeout.
const MANAGE_WAIT_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Debug, Clone, Default)]
pub struct StatusArguments {
    pub service: Option<String>,
}

#[derive(Debug, Clone)]
pub struct LogsArguments {
    pub service: String,
    pub cursor: Option<u64>,
    pub generation: Option<u64>,
    pub limit: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct TraceArguments {
    pub operation_id: String,
}

#[derive(Debug, Clone, Default)]
pub struct EventsArguments {
    pub after: Option<u64>,
    pub epoch: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ManageArguments {
    pub service: String,
    /// `start`, `stop` or `restart` — never `status`.
    pub action: ServiceOperationKind,
    /// `action: start` only — the host-approved "kill the process holding my port" reclaim,
    /// forwarded verbatim to the daemon's `killUnowned` operation flag.
    pub kill_unowned: bool,
}

/// The dependency-injected MCP client shape a reusable package's MCP entrypoint needs: transport-
/// agnostic and unit-testable with a fake, rather than reaching for HTTP inline in the tool
/// dispatcher. `async_trait`-boxed (matching the rest of this codebase's object-safe-trait pattern,
/// e.g. `hearth-core`'s `Host`) so a server can hold it as `Arc<dyn HearthMcpClient>`.
#[async_trait]
pub trait HearthMcpClient: Send + Sync {
    async fn status(&self, arguments: StatusArguments) -> Result<Value, String>;
    async fn logs(&self, arguments: LogsArguments) -> Result<Value, String>;
    async fn trace(&self, arguments: TraceArguments) -> Result<Value, String>;
    async fn events(&self, arguments: EventsArguments) -> Result<Value, String>;
    async fn manage(&self, arguments: ManageArguments) -> Result<Value, String>;
    /// Restarts this project's daemon. Takes no arguments: there is exactly one daemon per project
    /// root, so there is nothing to select.
    async fn restart_daemon(&self) -> Result<Value, String>;
    /// Stops this project's daemon AND every service it manages (`stop-services`). Takes no
    /// arguments for the same reason — afterwards every other tool fails until a new daemon is
    /// ensured by a client that can spawn one (this MCP server deliberately cannot).
    async fn stop_daemon(&self) -> Result<Value, String>;

    /// The remote shared-services registry (`catalog.json`): which services/versions this machine
    /// can install. Read-only; the smp daemon is not required.
    async fn shared_list(&self) -> Result<Value, String> {
        Err("shared services are not supported by this client".to_string())
    }
    /// Instances the machine-global smp daemon knows about (ports, install state, attachments).
    async fn shared_status(&self) -> Result<Value, String> {
        Err("shared services are not supported by this client".to_string())
    }
    /// This project's rendered connection info for a shared service it has attached — the way an
    /// agent learns the `DATABASE_URL`-style values without env injection.
    async fn shared_connection(&self, _service: String) -> Result<Value, String> {
        Err("shared services are not supported by this client".to_string())
    }

    /// The service ids the daemon currently serves, when the client can tell — the server
    /// refreshes its tool schemas and argument validation from this, so a `manager reload` that
    /// adds a service doesn't need an MCP server restart. `None` keeps the ids it started with.
    async fn service_ids(&self) -> Option<Vec<String>> {
        None
    }
}

/// Default `HearthMcpClient` backed by the daemon's HTTP API.
pub struct ManagerApiClient {
    root: PathBuf,
    options: LocalctlOptions,
    api: ManagerClient,
}

impl ManagerApiClient {
    pub fn new(root: PathBuf, options: LocalctlOptions) -> Self {
        let api = ManagerClient::new(root.clone(), options.catalog.clone());
        Self { root, options, api }
    }

    async fn call(&self, path: &str) -> Result<Value, String> {
        self.api
            .request(path, reqwest::Method::GET, None)
            .await
            .map_err(|e| e.message)
    }

    /// The catalog the daemon serves now (it may have been reloaded since this process started),
    /// falling back to the startup catalog when the daemon can't be asked.
    async fn current_catalog(&self) -> ServiceCatalog {
        self.api
            .catalog()
            .await
            .unwrap_or_else(|_| self.options.catalog.clone())
    }
}

#[async_trait]
impl HearthMcpClient for ManagerApiClient {
    async fn status(&self, arguments: StatusArguments) -> Result<Value, String> {
        let (manager, services) =
            tokio::try_join!(self.call("/v1/manager"), self.call("/v1/services"))?;
        let mut result = json!({ "manager": manager, "services": filter_service_states(services, arguments.service.as_deref()) });
        // Additive, and best-effort: a daemon from before `/v1/urls` existed still answers
        // `status`, just without `urls`, rather than failing the whole call.
        if let Ok(urls) = self.call("/v1/urls").await {
            result["urls"] = filter_service_urls(urls, arguments.service.as_deref());
        }
        Ok(result)
    }

    async fn logs(&self, arguments: LogsArguments) -> Result<Value, String> {
        let slice = self
            .api
            .log(
                &arguments.service,
                arguments.cursor,
                arguments.generation,
                arguments.limit,
            )
            .await
            .map_err(|e| e.message)?;
        serde_json::to_value(slice).map_err(|e| e.to_string())
    }

    async fn trace(&self, arguments: TraceArguments) -> Result<Value, String> {
        self.call(&format!(
            "/v1/operations/{}",
            encode_path_segment(&arguments.operation_id)
        ))
        .await
    }

    async fn events(&self, arguments: EventsArguments) -> Result<Value, String> {
        let query = query_string(&[
            ("after", arguments.after.map(|v| v.to_string())),
            ("epoch", arguments.epoch.clone()),
        ]);
        self.call(&format!("/v1/events{query}")).await
    }

    /// Submits the operation and waits up to `MANAGE_WAIT_TIMEOUT`. Still running after that is not
    /// an error: the reply carries the operation so the agent can `trace` it.
    async fn manage(&self, arguments: ManageArguments) -> Result<Value, String> {
        runnable_targets(&self.current_catalog().await, Some(&arguments.service))
            .map_err(|e| e.message)?;
        let accepted = self
            .api
            .submit(arguments.action, &arguments.service, arguments.kill_unowned)
            .await
            .map_err(|e| e.message)?;
        let deadline = tokio::time::Instant::now() + MANAGE_WAIT_TIMEOUT;
        let operation = match self.api.wait(&accepted.id, Some(deadline)).await {
            Ok(operation) => operation,
            // A lost poll doesn't lose the operation — hand its id back rather than an opaque error.
            Err(error) => {
                return Err(format!(
                    "{} (operation {} may still be running — trace it)",
                    error.message, accepted.id
                ))
            }
        };
        match operation.status {
            OperationStatus::Failed => Err(operation
                .error
                .map(|e| e.message)
                .unwrap_or_else(|| "operation failed".to_string())),
            OperationStatus::Succeeded => {
                self.status(StatusArguments {
                    service: Some(arguments.service),
                })
                .await
            }
            OperationStatus::Queued | OperationStatus::Running => Ok(json!({
                "operation": operation,
                "message": format!("still {} after {}s — call trace with operationId {} to follow it", operation.status.as_wire_str(), MANAGE_WAIT_TIMEOUT.as_secs(), operation.id),
            })),
        }
    }

    /// The same `hearth manager restart` the CLI runs, in-process: shut the daemon down leaving its
    /// services running, wait for it to exit, ensure a fresh one.
    async fn restart_daemon(&self) -> Result<Value, String> {
        let result = restart_manager(&self.root, &self.options)
            .await
            .map_err(|e| e.message);
        self.api.invalidate();
        result
    }

    /// The same `hearth manager stop` the CLI runs, in-process: `stop-services` shutdown, then the
    /// wait for the daemon pid to exit — returning early would let the caller reconnect into a
    /// still-draining daemon that answers every request with `manager_closing`.
    async fn stop_daemon(&self) -> Result<Value, String> {
        let result = stop_manager(&self.root, &self.options)
            .await
            .map_err(|e| e.message);
        self.api.invalidate();
        result
    }

    async fn service_ids(&self) -> Option<Vec<String>> {
        let catalog = self.api.catalog().await.ok()?;
        Some(catalog.services.into_iter().map(|s| s.id).collect())
    }

    async fn shared_list(&self) -> Result<Value, String> {
        let remote = RemoteCatalog::new(
            &shared_root(),
            std::env::var("HEARTH_SHARED_CATALOG_URL").ok(),
        );
        let doc = remote.load(false).await.map_err(|e| e.0)?;
        let mut result = json!({ "catalog": doc.as_ref() });
        // Best-effort merge of what's already installed/running under smp.
        if let Discovery::Live { client } = hearth_cli::shared::discover_smp().await {
            if let Ok(installed) =
                cli_request(&client, "/v1/shared", reqwest::Method::GET, None, None).await
            {
                result["instances"] = installed["instances"].clone();
            }
        }
        Ok(result)
    }

    async fn shared_status(&self) -> Result<Value, String> {
        match hearth_cli::shared::discover_smp().await {
            Discovery::Live { client } => {
                cli_request(&client, "/v1/shared", reqwest::Method::GET, None, None).await
            }
            Discovery::Incompatible { .. } => Err("smp protocol is incompatible".to_string()),
            _ => Ok(json!({ "running": false, "instances": [] })),
        }
    }

    async fn shared_connection(&self, service: String) -> Result<Value, String> {
        let instance_id = resolve_shared_instance_id(&self.current_catalog().await, &service)?;
        let Discovery::Live { client } = hearth_cli::shared::discover_smp().await else {
            return Err("smp is not running".to_string());
        };
        let body = cli_request(&client, "/v1/shared", reqwest::Method::GET, None, None).await?;
        let pid = project_id(&self.root);
        let instance = body["instances"]
            .as_array()
            .and_then(|instances| {
                instances
                    .iter()
                    .find(|i| i["id"].as_str() == Some(instance_id.as_str()))
            })
            .ok_or_else(|| format!("{instance_id} is not a registered shared instance"))?;
        let attachment = instance["attachments"]
            .as_array()
            .and_then(|attachments| {
                attachments
                    .iter()
                    .find(|a| a["projectId"].as_str() == Some(pid.as_str()))
            })
            .ok_or_else(|| {
                format!("this project has not attached {instance_id} — start the service first")
            })?;
        if attachment["provisioned"].as_bool() != Some(true) {
            return Err(format!("{instance_id} is attached but not yet provisioned"));
        }
        Ok(
            json!({ "service": instance_id, "projectId": pid, "connection": attachment["connection"] }),
        )
    }
}

/// `"postgres"` → `"postgres@16.4"` via this project catalog's generated `shared:` service (its run
/// command is `hearth shared attach <name@version> [attach-args…]`); `"postgres@16.4"` passes
/// through.
fn resolve_shared_instance_id(
    catalog: &hearth_core::catalog::ServiceCatalog,
    service: &str,
) -> Result<String, String> {
    if service.contains('@') {
        return Ok(service.to_string());
    }
    let definition = catalog
        .services
        .iter()
        .find(|s| s.id == service)
        .ok_or_else(|| format!("unknown service: {service}"))?;
    let hearth_core::catalog::ServiceRunProfile::Verified { command, .. } =
        &definition.profiles.run
    else {
        return Err(format!("{service} is not a shared service"));
    };
    let hearth_core::catalog::CommandSpec::Argv { argv } = &command.command else {
        return Err(format!("{service} is not a shared service"));
    };
    // The id follows the `shared attach` pair; `attachArgs` may follow the id.
    argv.windows(3)
        .find(|w| w[0] == "shared" && w[1] == "attach")
        .map(|w| w[2].clone())
        .ok_or_else(|| format!("{service} is not a shared service"))
}

/// `/v1/urls`' body narrowed to one service's entries when `status` was asked about one service.
fn filter_service_urls(mut value: Value, service: Option<&str>) -> Value {
    let Some(service) = service else { return value };
    for key in ["urls", "unresolved"] {
        if let Some(entries) = value.get_mut(key).and_then(Value::as_array_mut) {
            entries.retain(|entry| entry.get("serviceId").and_then(Value::as_str) == Some(service));
        }
    }
    value
}

fn filter_service_states(value: Value, service: Option<&str>) -> Value {
    let Some(service) = service else { return value };
    match value {
        Value::Array(entries) => Value::Array(
            entries
                .into_iter()
                .filter(|entry| entry.get("serviceId").and_then(Value::as_str) == Some(service))
                .collect(),
        ),
        Value::Object(mut object) => {
            if let Some(services) = object.get("services").cloned() {
                object.insert(
                    "services".to_string(),
                    filter_service_states(services, Some(service)),
                );
            }
            Value::Object(object)
        }
        other => other,
    }
}

fn query_string(pairs: &[(&str, Option<String>)]) -> String {
    let parts: Vec<String> = pairs
        .iter()
        .filter_map(|(key, value)| {
            value.as_ref().map(|value| {
                format!(
                    "{}={}",
                    encode_path_segment(key),
                    encode_path_segment(value)
                )
            })
        })
        .collect();
    if parts.is_empty() {
        String::new()
    } else {
        format!("?{}", parts.join("&"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn catalog_with(definition: hearth_core::catalog::ServiceDefinition) -> ServiceCatalog {
        ServiceCatalog {
            services: vec![definition],
            groups: Default::default(),
            group_tree: Vec::new(),
            compose_file: None,
            runtime_directory: None,
            start_failure_policy:
                hearth_core::catalog::StartFailurePolicy::StopOnFirstFailureKeepStarted,
            private_file_guard: None,
        }
    }

    /// viclass/infra's shared nginx passes `attachArgs`, which land after the instance id.
    #[test]
    fn resolves_the_instance_id_even_when_attach_args_follow_it() {
        let exe = Path::new("hearth");
        let with_args = hearth_core::shared::synthesize::project_service_entry(
            "nginx".to_string(),
            "nginx@1.27",
            exe,
            None,
            vec!["--conf".to_string(), "/tmp/conf".to_string()],
            None,
        );
        assert_eq!(
            resolve_shared_instance_id(&catalog_with(with_args), "nginx").unwrap(),
            "nginx@1.27"
        );
        let bare = hearth_core::shared::synthesize::project_service_entry(
            "postgres".to_string(),
            "postgres@16.4",
            exe,
            None,
            Vec::new(),
            None,
        );
        assert_eq!(
            resolve_shared_instance_id(&catalog_with(bare), "postgres").unwrap(),
            "postgres@16.4"
        );
        assert_eq!(
            resolve_shared_instance_id(
                &catalog_with(hearth_core::shared::synthesize::project_service_entry(
                    "x".to_string(),
                    "x@1",
                    exe,
                    None,
                    Vec::new(),
                    None
                )),
                "x@2"
            )
            .unwrap(),
            "x@2"
        );
    }
}
