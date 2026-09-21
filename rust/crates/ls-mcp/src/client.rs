//! Port of the `ManagerApiClient` half of `src/mcp/mcp-server.ts` — the default
//! `LocalServicesMcpClient` backed by the daemon's HTTP API, reusing `ls-cli`'s own
//! `require_client`/`request`/`runnable_targets` rather than re-implementing daemon HTTP plumbing a
//! third time (`ls-tui`'s `ManagerTuiClient` already reuses them the same way).
use std::path::PathBuf;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Value};

use ls_cli::{require_client, request as cli_request, runnable_targets, LocalctlOptions};

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManageAction {
    Start,
    Stop,
    Restart,
}

impl ManageAction {
    pub fn as_str(self) -> &'static str {
        match self {
            ManageAction::Start => "start",
            ManageAction::Stop => "stop",
            ManageAction::Restart => "restart",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ManageArguments {
    pub service: String,
    pub action: ManageAction,
}

/// The dependency-injected MCP client shape a reusable package's MCP entrypoint needs: transport-
/// agnostic and unit-testable with a fake, rather than reaching for HTTP inline in the tool
/// dispatcher. `async_trait`-boxed (matching the rest of this codebase's object-safe-trait pattern,
/// e.g. `ls-core`'s `Host`) so a server can hold it as `Arc<dyn LocalServicesMcpClient>`.
#[async_trait]
pub trait LocalServicesMcpClient: Send + Sync {
    async fn status(&self, arguments: StatusArguments) -> Result<Value, String>;
    async fn logs(&self, arguments: LogsArguments) -> Result<Value, String>;
    async fn trace(&self, arguments: TraceArguments) -> Result<Value, String>;
    async fn events(&self, arguments: EventsArguments) -> Result<Value, String>;
    async fn manage(&self, arguments: ManageArguments) -> Result<Value, String>;
}

/// Default `LocalServicesMcpClient` backed by the daemon's HTTP API.
pub struct ManagerApiClient {
    root: PathBuf,
    options: LocalctlOptions,
}

impl ManagerApiClient {
    pub fn new(root: PathBuf, options: LocalctlOptions) -> Self {
        Self { root, options }
    }

    async fn call(&self, path: &str, method: reqwest::Method, body: Option<&Value>) -> Result<Value, String> {
        let client = require_client(&self.root, &self.options).await.map_err(|e| e.message)?;
        cli_request(&client, path, method, body, None).await
    }
}

#[async_trait]
impl LocalServicesMcpClient for ManagerApiClient {
    async fn status(&self, arguments: StatusArguments) -> Result<Value, String> {
        let client = require_client(&self.root, &self.options).await.map_err(|e| e.message)?;
        let (manager, services) = tokio::try_join!(cli_request(&client, "/v1/manager", reqwest::Method::GET, None, None), cli_request(&client, "/v1/services", reqwest::Method::GET, None, None))?;
        let mut result = json!({ "manager": manager, "services": filter_service_states(services, arguments.service.as_deref()) });
        // Additive, and best-effort: a daemon from before `/v1/urls` existed still answers
        // `status`, just without `urls`, rather than failing the whole call.
        if let Ok(urls) = cli_request(&client, "/v1/urls", reqwest::Method::GET, None, None).await {
            result["urls"] = filter_service_urls(urls, arguments.service.as_deref());
        }
        Ok(result)
    }

    async fn logs(&self, arguments: LogsArguments) -> Result<Value, String> {
        let query = query_string(&[("cursor", arguments.cursor.map(|v| v.to_string())), ("generation", arguments.generation.map(|v| v.to_string())), ("limit", arguments.limit.map(|v| v.to_string()))]);
        self.call(&format!("/v1/logs/{}{query}", encode_path_segment(&arguments.service)), reqwest::Method::GET, None).await
    }

    async fn trace(&self, arguments: TraceArguments) -> Result<Value, String> {
        self.call(&format!("/v1/operations/{}", encode_path_segment(&arguments.operation_id)), reqwest::Method::GET, None).await
    }

    async fn events(&self, arguments: EventsArguments) -> Result<Value, String> {
        let query = query_string(&[("after", arguments.after.map(|v| v.to_string())), ("epoch", arguments.epoch.clone())]);
        self.call(&format!("/v1/events{query}"), reqwest::Method::GET, None).await
    }

    async fn manage(&self, arguments: ManageArguments) -> Result<Value, String> {
        runnable_targets(&self.options.catalog, Some(&arguments.service)).map_err(|e| e.message)?;
        let body = json!({ "requestId": uuid::Uuid::new_v4().to_string(), "serviceId": arguments.service, "action": arguments.action.as_str() });
        let mut response = self.call("/v1/operations", reqwest::Method::POST, Some(&body)).await?;
        let mut operation = response["operation"].take();
        loop {
            let status = operation["status"].as_str().unwrap_or_default();
            if status != "queued" && status != "running" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(300)).await;
            let id = operation["id"].as_str().unwrap_or_default().to_string();
            response = self.call(&format!("/v1/operations/{id}"), reqwest::Method::GET, None).await?;
            operation = response["operation"].take();
        }
        if operation["status"].as_str() == Some("failed") {
            let message = operation["error"]["message"].as_str().unwrap_or("operation failed").to_string();
            return Err(message);
        }
        self.status(StatusArguments { service: Some(arguments.service) }).await
    }
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
        Value::Array(entries) => Value::Array(entries.into_iter().filter(|entry| entry.get("serviceId").and_then(Value::as_str) == Some(service)).collect()),
        Value::Object(mut object) => {
            if let Some(services) = object.get("services").cloned() {
                object.insert("services".to_string(), filter_service_states(services, Some(service)));
            }
            Value::Object(object)
        }
        other => other,
    }
}

fn encode_path_segment(segment: &str) -> String {
    percent_encoding::utf8_percent_encode(segment, percent_encoding::NON_ALPHANUMERIC).to_string()
}

fn query_string(pairs: &[(&str, Option<String>)]) -> String {
    let parts: Vec<String> = pairs.iter().filter_map(|(key, value)| value.as_ref().map(|value| format!("{}={}", encode_path_segment(key), encode_path_segment(value)))).collect();
    if parts.is_empty() {
        String::new()
    } else {
        format!("?{}", parts.join("&"))
    }
}
