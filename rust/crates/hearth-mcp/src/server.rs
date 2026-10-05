//! The MCP tool surface — a manual `ServerHandler` (not `rmcp`'s `#[tool]` macros) because the tool
//! set is only known at runtime: tool names carry a configurable prefix, the `manage`/`status`/
//! `logs` schemas embed an `enum` of the daemon's current service ids, and `manage`'s required
//! fields depend on `requireConfirm`. Only `get_info`/`list_tools`/`call_tool` are overridden;
//! every other `ServerHandler` method stays at its default.
use std::sync::{Arc, RwLock};

use regex::Regex;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation,
    JsonObject, ListToolsResult, PaginatedRequestParams, ServerCapabilities, ServerConfig, Tool,
};
use rmcp::service::RequestContext;
use rmcp::{ErrorData as McpError, RoleServer, ServerHandler};
use serde_json::{json, Value};

use hearth_core::state::ServiceOperationKind;

use crate::client::{
    EventsArguments, HearthMcpClient, LogsArguments, ManageArguments, StatusArguments,
    TraceArguments,
};

fn secret_key_pattern() -> &'static Regex {
    use std::sync::OnceLock;
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)authorization|token|ownership(?:key|proof)|secret|password|api[_-]?key")
            .unwrap()
    })
}

/// Strips anything that looks like a credential out of a value before it reaches the model —
/// recurses through arrays/objects, dropping any object key matching `SECRET_KEY_PATTERN`.
fn redact(value: &Value) -> Value {
    redact_at_depth(value, 0)
}

/// `redact`, except for `shared_connection`'s `connection` subtree: handing the agent a shared
/// service's env values (`AWS_SECRET_ACCESS_KEY`, a `DATABASE_URL` password) is that tool's whole
/// purpose, and they are local-dev constants from the shared catalog, not real credentials.
fn redact_tool_result(tool: &str, value: &Value) -> Value {
    let mut redacted = redact(value);
    if tool == "shared_connection" {
        if let (Some(object), Some(connection)) =
            (redacted.as_object_mut(), value.get("connection"))
        {
            object.insert("connection".to_string(), connection.clone());
        }
    }
    redacted
}

fn redact_at_depth(value: &Value, depth: u32) -> Value {
    if depth > 12 {
        return Value::String("[truncated]".to_string());
    }
    match value {
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|entry| redact_at_depth(entry, depth + 1))
                .collect(),
        ),
        Value::Object(map) => Value::Object(
            map.iter()
                .filter(|(key, _)| !secret_key_pattern().is_match(key))
                .map(|(key, entry)| (key.clone(), redact_at_depth(entry, depth + 1)))
                .collect(),
        ),
        other => other.clone(),
    }
}

fn safe_error_message(message: &str) -> String {
    static BEARER_RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    let re = BEARER_RE.get_or_init(|| Regex::new(r"(?i)Bearer\s+\S+").unwrap());
    re.replace_all(message, "Bearer [redacted]").to_string()
}

pub struct CreateHearthMcpServerOptions {
    /// MCP server name registered with the SDK, e.g. `"hearth"`.
    pub name: String,
    pub version: Option<String>,
    /// Every tool is registered as `${tool_prefix}status`, `${tool_prefix}logs`, etc.
    pub tool_prefix: String,
    /// Require a schema-enforced `confirm: true` argument on the manage tool (start/stop/restart)
    /// — the safer default: an MCP host that ignores prose advice still can't invoke it
    /// accidentally, because the tool's own JSON schema demands the field.
    pub require_confirm: bool,
    /// The service ids to start with. Refreshed from `HearthMcpClient::service_ids` on every
    /// `tools/list` and whenever a call names an id not in the list.
    pub known_service_ids: Vec<String>,
}

impl Default for CreateHearthMcpServerOptions {
    fn default() -> Self {
        Self {
            name: "hearth".to_string(),
            version: None,
            tool_prefix: String::new(),
            require_confirm: true,
            known_service_ids: Vec::new(),
        }
    }
}

#[derive(Clone)]
pub struct HearthMcpServer {
    client: Arc<dyn HearthMcpClient>,
    options: Arc<CreateHearthMcpServerOptions>,
    service_ids: Arc<RwLock<Vec<String>>>,
}

pub fn create_hearth_mcp_server(
    client: Arc<dyn HearthMcpClient>,
    options: CreateHearthMcpServerOptions,
) -> HearthMcpServer {
    let service_ids = Arc::new(RwLock::new(options.known_service_ids.clone()));
    HearthMcpServer {
        client,
        options: Arc::new(options),
        service_ids,
    }
}

fn require_only_keys(value: &JsonObject, allowed: &[&str]) -> Result<(), String> {
    let unexpected: Vec<&str> = value
        .keys()
        .map(String::as_str)
        .filter(|key| !allowed.contains(key))
        .collect();
    if unexpected.is_empty() {
        Ok(())
    } else {
        let suffix = if unexpected.len() > 1 { "s" } else { "" };
        Err(format!(
            "unexpected argument{suffix}: {}",
            unexpected.join(", ")
        ))
    }
}

fn optional_string(value: Option<&Value>, name: &str) -> Result<Option<String>, String> {
    match value {
        None => Ok(None),
        Some(Value::String(s)) if !s.is_empty() => Ok(Some(s.clone())),
        _ => Err(format!("{name} must be a non-empty string")),
    }
}

fn required_string(value: Option<&Value>, name: &str) -> Result<String, String> {
    optional_string(value, name)?.ok_or_else(|| format!("{name} is required"))
}

fn optional_integer(
    value: Option<&Value>,
    name: &str,
    minimum: i64,
    maximum: Option<i64>,
) -> Result<Option<i64>, String> {
    let range_error = || {
        let range = match maximum {
            Some(max) => format!("between {minimum} and {max}"),
            None => format!("at least {minimum}"),
        };
        format!("{name} must be an integer {range}")
    };
    // JavaScript's `Number.isSafeInteger` — what every MCP host's JSON layer produces: an
    // integral float (`1000.0`, from a host that serializes numbers as floats) is accepted, and
    // anything beyond 2^53 is rejected as unrepresentable.
    const MAX_SAFE_INTEGER: i64 = 9_007_199_254_740_991;
    match value {
        None => Ok(None),
        Some(Value::Number(number)) => {
            let candidate = match number.as_i64() {
                Some(int) => Some(int),
                None => number
                    .as_f64()
                    .filter(|f| f.fract() == 0.0)
                    .map(|f| f as i64),
            };
            match candidate {
                Some(int)
                    if int.abs() <= MAX_SAFE_INTEGER
                        && int >= minimum
                        && maximum.is_none_or(|max| int <= max) =>
                {
                    Ok(Some(int))
                }
                _ => Err(range_error()),
            }
        }
        _ => Err(range_error()),
    }
}

impl HearthMcpServer {
    fn known_service_ids(&self) -> Vec<String> {
        self.service_ids.read().unwrap().clone()
    }

    /// Re-reads the daemon's current service ids (after a `manager reload`), keeping the old list
    /// when the client can't answer.
    async fn refresh_service_ids(&self) {
        if let Some(ids) = self.client.service_ids().await {
            *self.service_ids.write().unwrap() = ids;
        }
    }

    fn require_service(&self, value: Option<&Value>) -> Result<String, String> {
        let service = required_string(value, "service")?;
        let known = self.known_service_ids();
        if known.iter().any(|id| id == &service) {
            Ok(service)
        } else {
            Err(format!(
                "unknown service: {service}. Known services: {}",
                known.join(", ")
            ))
        }
    }

    fn parse_status(&self, arguments: &JsonObject) -> Result<StatusArguments, String> {
        require_only_keys(arguments, &["service"])?;
        Ok(StatusArguments {
            service: arguments
                .get("service")
                .map(|value| self.require_service(Some(value)))
                .transpose()?,
        })
    }

    fn parse_logs(&self, arguments: &JsonObject) -> Result<LogsArguments, String> {
        require_only_keys(arguments, &["service", "cursor", "generation", "limit"])?;
        Ok(LogsArguments {
            service: self.require_service(arguments.get("service"))?,
            cursor: optional_integer(arguments.get("cursor"), "cursor", 0, None)?.map(|v| v as u64),
            generation: optional_integer(arguments.get("generation"), "generation", 0, None)?
                .map(|v| v as u64),
            limit: optional_integer(arguments.get("limit"), "limit", 1, Some(64 * 1024))?
                .map(|v| v as u64),
        })
    }

    fn parse_trace(&self, arguments: &JsonObject) -> Result<TraceArguments, String> {
        require_only_keys(arguments, &["operationId"])?;
        Ok(TraceArguments {
            operation_id: required_string(arguments.get("operationId"), "operationId")?,
        })
    }

    fn parse_events(&self, arguments: &JsonObject) -> Result<EventsArguments, String> {
        require_only_keys(arguments, &["after", "epoch"])?;
        Ok(EventsArguments {
            after: optional_integer(arguments.get("after"), "after", 0, None)?.map(|v| v as u64),
            epoch: optional_string(arguments.get("epoch"), "epoch")?,
        })
    }

    fn parse_manage(&self, arguments: &JsonObject) -> Result<ManageArguments, String> {
        let allowed: &[&str] = if self.options.require_confirm {
            &["service", "action", "confirm", "killUnowned"]
        } else {
            &["service", "action", "killUnowned"]
        };
        require_only_keys(arguments, allowed)?;
        if self.options.require_confirm && arguments.get("confirm") != Some(&Value::Bool(true)) {
            return Err("manage requires confirm=true (explicit user approval) — never call this speculatively".to_string());
        }
        let service = self.require_service(arguments.get("service"))?;
        let action = match arguments.get("action").and_then(Value::as_str) {
            Some("start") => ServiceOperationKind::Start,
            Some("stop") => ServiceOperationKind::Stop,
            Some("restart") => ServiceOperationKind::Restart,
            _ => return Err("action must be one of: start, stop, restart".to_string()),
        };
        let kill_unowned = match arguments.get("killUnowned") {
            None | Some(Value::Bool(false)) => false,
            Some(Value::Bool(true)) => true,
            _ => return Err("killUnowned must be a boolean".to_string()),
        };
        if kill_unowned && action != ServiceOperationKind::Start {
            return Err("killUnowned only applies to action=start".to_string());
        }
        Ok(ManageArguments {
            service,
            action,
            kill_unowned,
        })
    }

    /// The daemon-lifecycle tools take no arguments beyond the confirm gate — one daemon per
    /// project root means there is nothing to select.
    fn parse_daemon_lifecycle(&self, tool: &str, arguments: &JsonObject) -> Result<(), String> {
        let allowed: &[&str] = if self.options.require_confirm {
            &["confirm"]
        } else {
            &[]
        };
        require_only_keys(arguments, allowed)?;
        if self.options.require_confirm && arguments.get("confirm") != Some(&Value::Bool(true)) {
            return Err(format!("{tool} requires confirm=true (explicit user approval) — never call this speculatively"));
        }
        Ok(())
    }

    async fn dispatch(&self, tool: &str, arguments: JsonObject) -> Result<Value, String> {
        if matches!(tool, "status" | "logs" | "manage") {
            if let Some(Value::String(service)) = arguments.get("service") {
                if !self.known_service_ids().contains(service) {
                    self.refresh_service_ids().await;
                }
            }
        }
        match tool {
            "status" => self.client.status(self.parse_status(&arguments)?).await,
            "logs" => self.client.logs(self.parse_logs(&arguments)?).await,
            "trace" => self.client.trace(self.parse_trace(&arguments)?).await,
            "events" => self.client.events(self.parse_events(&arguments)?).await,
            "manage" => self.client.manage(self.parse_manage(&arguments)?).await,
            "restart_daemon" => {
                self.parse_daemon_lifecycle("restart_daemon", &arguments)?;
                self.client.restart_daemon().await
            }
            "stop_daemon" => {
                self.parse_daemon_lifecycle("stop_daemon", &arguments)?;
                self.client.stop_daemon().await
            }
            "shared_list" => {
                require_only_keys(&arguments, &[])?;
                self.client.shared_list().await
            }
            "shared_status" => {
                require_only_keys(&arguments, &[])?;
                self.client.shared_status().await
            }
            "shared_connection" => {
                require_only_keys(&arguments, &["service"])?;
                self.client
                    .shared_connection(required_string(arguments.get("service"), "service")?)
                    .await
            }
            _ => Err(format!("unknown tool: {}{tool}", self.options.tool_prefix)),
        }
    }

    fn tool_definitions(&self) -> Vec<Tool> {
        let prefix = &self.options.tool_prefix;
        let require_confirm = self.options.require_confirm;
        let service_ids = &self.known_service_ids();
        let schema = |value: Value| -> Arc<JsonObject> {
            Arc::new(value.as_object().cloned().unwrap_or_default())
        };

        let mut manage_properties = json!({
            "service": { "type": "string", "enum": service_ids },
            "action": { "type": "string", "enum": ["start", "stop", "restart"] },
            "killUnowned": { "type": "boolean", "description": "Only for action=start: when the service's port is held by a process this manager does not own, kill that process and continue starting. Never set unless the user explicitly asked to reclaim the port." },
        });
        let mut manage_required = vec!["service", "action"];
        if require_confirm {
            manage_properties["confirm"] = json!({ "type": "boolean", "description": "Must be true; the user must have explicitly asked for this action" });
            manage_required.push("confirm");
        }
        let manage_description = if require_confirm {
            "Start/stop/restart a local dev service. Requires confirm=true (explicit user approval) — never call this speculatively."
        } else {
            "Start/stop/restart a local dev service. MCP hosts should require approval for this tool."
        };

        let mut restart_properties = json!({});
        let mut restart_required: Vec<&str> = Vec::new();
        if require_confirm {
            restart_properties["confirm"] = json!({ "type": "boolean", "description": "Must be true; the user must have explicitly asked for this action" });
            restart_required.push("confirm");
        }
        let restart_description = if require_confirm {
            "Restart this project's hearth daemon. Running services are left alone and re-adopted by the new daemon. Requires confirm=true (explicit user approval) — never call this speculatively."
        } else {
            "Restart this project's hearth daemon. Running services are left alone and re-adopted by the new daemon. MCP hosts should require approval for this tool."
        };

        let mut stop_daemon_properties = json!({});
        let mut stop_daemon_required: Vec<&str> = Vec::new();
        if require_confirm {
            stop_daemon_properties["confirm"] = json!({ "type": "boolean", "description": "Must be true; the user must have explicitly asked for this action" });
            stop_daemon_required.push("confirm");
        }
        let stop_daemon_description = if require_confirm {
            "Stop this project's hearth daemon AND every service it manages. Nothing keeps running afterwards — every tool call then fails until a new daemon is started by a client that can spawn one. Requires confirm=true (explicit user approval) — never call this speculatively."
        } else {
            "Stop this project's hearth daemon AND every service it manages. Nothing keeps running afterwards. MCP hosts should require approval for this tool."
        };

        vec![
            Tool::new(format!("{prefix}status"), "Read-only status of local dev services. No approval needed.", schema(json!({ "type": "object", "properties": { "service": { "type": "string", "enum": service_ids, "description": "Service id. Omit to list all." } }, "additionalProperties": false }))),
            Tool::new(
                format!("{prefix}logs"),
                "Read-only bounded log chunk for one service. No approval needed.",
                schema(json!({
                    "type": "object",
                    "properties": {
                        "service": { "type": "string", "enum": service_ids },
                        "cursor": { "type": "integer", "minimum": 0 },
                        "generation": { "type": "integer", "minimum": 0, "description": "Log cursor generation. 0 is valid: a service with no state row reports lifecycle generation 0." },
                        "limit": { "type": "integer", "minimum": 1, "maximum": 64 * 1024 },
                    },
                    "required": ["service"],
                    "additionalProperties": false,
                })),
            ),
            Tool::new(format!("{prefix}trace"), "Read-only trace/status of a start/stop/restart operation by id. No approval needed.", schema(json!({ "type": "object", "properties": { "operationId": { "type": "string" } }, "required": ["operationId"], "additionalProperties": false }))),
            Tool::new(format!("{prefix}events"), "Read-only recent manager events since a sequence number. Not a follow/SSE stream. No approval needed.", schema(json!({ "type": "object", "properties": { "after": { "type": "integer", "minimum": 0 }, "epoch": { "type": "string" } }, "additionalProperties": false }))),
            Tool::new(format!("{prefix}manage"), manage_description, schema(json!({ "type": "object", "properties": manage_properties, "required": manage_required, "additionalProperties": false }))),
            Tool::new(format!("{prefix}restart_daemon"), restart_description, schema(json!({ "type": "object", "properties": restart_properties, "required": restart_required, "additionalProperties": false }))),
            Tool::new(format!("{prefix}stop_daemon"), stop_daemon_description, schema(json!({ "type": "object", "properties": stop_daemon_properties, "required": stop_daemon_required, "additionalProperties": false }))),
            Tool::new(
                format!("{prefix}shared_list"),
                "Read-only: services and versions available from the shared-services registry (installed on this machine by smp on demand, shared across projects). No approval needed.",
                schema(json!({ "type": "object", "properties": {}, "additionalProperties": false })),
            ),
            Tool::new(
                format!("{prefix}shared_status"),
                "Read-only: shared service instances on this machine — ports, install state, and which projects are attached. No approval needed.",
                schema(json!({ "type": "object", "properties": {}, "additionalProperties": false })),
            ),
            Tool::new(
                format!("{prefix}shared_connection"),
                "Read-only: this project's connection info (url/env) for a shared service it has attached via `shared:` in hearth.yaml. Pass the service id (e.g. \"postgres\") or the instance id (\"postgres@16.4\"). No approval needed.",
                schema(json!({ "type": "object", "properties": { "service": { "type": "string" } }, "required": ["service"], "additionalProperties": false })),
            ),
        ]
    }
}

impl ServerHandler for HearthMcpServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build()).with_server_info(
            Implementation::new(
                self.options.name.clone(),
                self.options
                    .version
                    .clone()
                    .unwrap_or_else(|| "1.0.0".to_string()),
            ),
        )
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        self.refresh_service_ids().await;
        Ok(ListToolsResult::with_all_items(self.tool_definitions()))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        let arguments = request.arguments.unwrap_or_default();
        let tool = request.name.strip_prefix(self.options.tool_prefix.as_str());
        let dispatched = match tool {
            Some(tool) => self.dispatch(tool, arguments).await,
            None => Err(format!("unknown tool: {}", request.name)),
        };
        let result = match dispatched {
            Ok(value) => CallToolResult::success(vec![ContentBlock::text(
                serde_json::to_string_pretty(&redact_tool_result(tool.unwrap_or_default(), &value))
                    .unwrap(),
            )]),
            Err(message) => {
                CallToolResult::error(vec![ContentBlock::text(safe_error_message(&message))])
            }
        };
        Ok(result.into())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use rmcp::{ClientHandler, ServiceExt};
    use serde_json::json;

    use super::*;
    use crate::client::ManageArguments;
    use hearth_core::state::ServiceOperationKind;

    #[derive(Clone, Default)]
    struct NoopClientHandler;
    impl ClientHandler for NoopClientHandler {}

    #[derive(Default)]
    struct FakeClient {
        managed: Mutex<Option<ManageArguments>>,
        restarts: std::sync::atomic::AtomicU32,
        stops: std::sync::atomic::AtomicU32,
        /// What `service_ids` answers — `None` keeps the server's startup list.
        service_ids: Mutex<Option<Vec<String>>>,
    }

    #[async_trait::async_trait]
    impl HearthMcpClient for FakeClient {
        async fn status(&self, _arguments: StatusArguments) -> Result<Value, String> {
            Ok(json!({}))
        }
        async fn logs(&self, _arguments: LogsArguments) -> Result<Value, String> {
            Ok(json!({}))
        }
        async fn trace(&self, _arguments: TraceArguments) -> Result<Value, String> {
            Ok(json!({}))
        }
        async fn events(&self, _arguments: EventsArguments) -> Result<Value, String> {
            Ok(json!({}))
        }
        async fn manage(&self, arguments: ManageArguments) -> Result<Value, String> {
            *self.managed.lock().unwrap() = Some(arguments);
            Ok(json!({ "operation": "accepted" }))
        }
        async fn restart_daemon(&self) -> Result<Value, String> {
            self.restarts
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(json!({ "instanceId": "restarted-instance" }))
        }
        async fn stop_daemon(&self) -> Result<Value, String> {
            self.stops.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(json!({ "operation": "accepted" }))
        }
        async fn shared_connection(&self, service: String) -> Result<Value, String> {
            Ok(
                json!({ "service": service, "projectId": "p", "connection": { "env": { "AWS_SECRET_ACCESS_KEY": "minioadmin" } }, "token": "must-not-leak" }),
            )
        }
        async fn service_ids(&self) -> Option<Vec<String>> {
            self.service_ids.lock().unwrap().clone()
        }
    }

    fn base_options() -> CreateHearthMcpServerOptions {
        CreateHearthMcpServerOptions {
            name: "test-hearth".to_string(),
            tool_prefix: "local_services_".to_string(),
            known_service_ids: vec!["metadata".to_string(), "mongo".to_string()],
            ..Default::default()
        }
    }

    fn args(value: Value) -> JsonObject {
        value.as_object().cloned().unwrap_or_default()
    }

    type Connected = (
        rmcp::service::RunningService<RoleServer, HearthMcpServer>,
        rmcp::service::RunningService<rmcp::RoleClient, NoopClientHandler>,
    );

    async fn connect(
        client: Arc<dyn HearthMcpClient>,
        options: CreateHearthMcpServerOptions,
    ) -> Connected {
        let server = create_hearth_mcp_server(client, options);
        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        // The initialize handshake is a round trip: each side's `.serve()` blocks until it hears
        // from the other, so both must run concurrently rather than one after the other.
        let (server_result, client_result) =
            tokio::join!(server.serve(server_io), NoopClientHandler.serve(client_io));
        (
            server_result.expect("server should initialize"),
            client_result.expect("client should initialize"),
        )
    }

    #[tokio::test]
    async fn routes_focused_application_management_through_the_ordinary_manager_client() {
        let fake = Arc::new(FakeClient::default());
        let (server, client) = connect(fake.clone(), base_options()).await;
        let response = client
            .call_tool(
                CallToolRequestParams::new("local_services_manage").with_arguments(args(
                    json!({ "service": "metadata", "action": "restart", "confirm": true }),
                )),
            )
            .await
            .unwrap();
        assert_ne!(response.is_error, Some(true));
        let managed = fake.managed.lock().unwrap().clone().unwrap();
        assert_eq!(managed.service, "metadata");
        assert_eq!(managed.action, ServiceOperationKind::Restart);
        let _ = client.cancel().await;
        let _ = server.cancel().await;
    }

    #[tokio::test]
    async fn routes_focused_infrastructure_management_through_the_ordinary_manager_client() {
        let fake = Arc::new(FakeClient::default());
        let (server, client) = connect(fake.clone(), base_options()).await;
        let response = client
            .call_tool(
                CallToolRequestParams::new("local_services_manage").with_arguments(args(
                    json!({ "service": "mongo", "action": "restart", "confirm": true }),
                )),
            )
            .await
            .unwrap();
        assert_ne!(response.is_error, Some(true));
        let managed = fake.managed.lock().unwrap().clone().unwrap();
        assert_eq!(managed.service, "mongo");
        assert_eq!(managed.action, ServiceOperationKind::Restart);
        let _ = client.cancel().await;
        let _ = server.cancel().await;
    }

    /// `killUnowned` is the MCP echo of a user's "yes, kill the port-holder" — it must parse, pass
    /// the same `confirm: true` gate, and reach the client verbatim.
    #[tokio::test]
    async fn manage_accepts_kill_unowned_for_start() {
        let fake = Arc::new(FakeClient::default());
        let (server, client) = connect(fake.clone(), base_options()).await;
        let response = client
            .call_tool(CallToolRequestParams::new("local_services_manage").with_arguments(args(json!({ "service": "metadata", "action": "start", "confirm": true, "killUnowned": true }))))
            .await
            .unwrap();
        assert_ne!(response.is_error, Some(true));
        let managed = fake.managed.lock().unwrap().clone().unwrap();
        assert!(managed.kill_unowned);
        let _ = client.cancel().await;
        let _ = server.cancel().await;
    }

    /// Reclaim is a start-only operation — `killUnowned` on stop/restart is a usage error, and it
    /// must never reach the client.
    #[tokio::test]
    async fn manage_rejects_kill_unowned_for_stop_and_restart() {
        let fake = Arc::new(FakeClient::default());
        let (server, client) = connect(fake.clone(), base_options()).await;
        for action in ["stop", "restart"] {
            let response = client
                .call_tool(CallToolRequestParams::new("local_services_manage").with_arguments(args(json!({ "service": "metadata", "action": action, "confirm": true, "killUnowned": true }))))
                .await
                .unwrap();
            assert_eq!(
                response.is_error,
                Some(true),
                "killUnowned on action={action} must be rejected"
            );
        }
        assert!(fake.managed.lock().unwrap().is_none());
        let _ = client.cancel().await;
        let _ = server.cancel().await;
    }

    #[tokio::test]
    async fn rejects_unexpected_extra_arguments() {
        let fake = Arc::new(FakeClient::default());
        let (server, client) = connect(fake, base_options()).await;
        let response = client.call_tool(CallToolRequestParams::new("local_services_manage").with_arguments(args(json!({ "service": "metadata", "action": "restart", "confirm": true, "profile": "dev" })))).await.unwrap();
        assert_eq!(response.is_error, Some(true));
        let _ = client.cancel().await;
        let _ = server.cancel().await;
    }

    #[tokio::test]
    async fn rejects_an_unknown_service_id() {
        let fake = Arc::new(FakeClient::default());
        let (server, client) = connect(fake, base_options()).await;
        let response = client.call_tool(CallToolRequestParams::new("local_services_manage").with_arguments(args(json!({ "service": "not-a-real-service", "action": "restart", "confirm": true })))).await.unwrap();
        assert_eq!(response.is_error, Some(true));
        let _ = client.cancel().await;
        let _ = server.cancel().await;
    }

    #[tokio::test]
    async fn manage_requires_confirm_true_by_default() {
        let fake = Arc::new(FakeClient::default());
        let (server, client) = connect(fake.clone(), base_options()).await;
        let response = client
            .call_tool(
                CallToolRequestParams::new("local_services_manage")
                    .with_arguments(args(json!({ "service": "metadata", "action": "restart" }))),
            )
            .await
            .unwrap();
        assert_eq!(response.is_error, Some(true));
        assert!(fake.managed.lock().unwrap().is_none());
        let _ = client.cancel().await;
        let _ = server.cancel().await;
    }

    #[tokio::test]
    async fn manage_skips_the_confirm_requirement_when_disabled() {
        let fake = Arc::new(FakeClient::default());
        let options = CreateHearthMcpServerOptions {
            require_confirm: false,
            ..base_options()
        };
        let (server, client) = connect(fake.clone(), options).await;
        let response = client
            .call_tool(
                CallToolRequestParams::new("local_services_manage")
                    .with_arguments(args(json!({ "service": "metadata", "action": "restart" }))),
            )
            .await
            .unwrap();
        assert_ne!(response.is_error, Some(true));
        let managed = fake.managed.lock().unwrap().clone().unwrap();
        assert_eq!(managed.service, "metadata");
        assert_eq!(managed.action, ServiceOperationKind::Restart);
        let _ = client.cancel().await;
        let _ = server.cancel().await;
    }

    #[tokio::test]
    async fn read_only_tools_need_no_confirm_and_are_registered_under_the_configured_prefix() {
        let fake = Arc::new(FakeClient::default());
        let (server, client) = connect(fake, base_options()).await;
        let tools = client.list_all_tools().await.unwrap();
        let names: Vec<String> = tools
            .into_iter()
            .map(|tool| tool.name.to_string())
            .collect();
        assert_eq!(
            names,
            vec![
                "local_services_status",
                "local_services_logs",
                "local_services_trace",
                "local_services_events",
                "local_services_manage",
                "local_services_restart_daemon",
                "local_services_stop_daemon",
                "local_services_shared_list",
                "local_services_shared_status",
                "local_services_shared_connection"
            ]
        );
        let response = client
            .call_tool(CallToolRequestParams::new("local_services_status"))
            .await
            .unwrap();
        assert_ne!(response.is_error, Some(true));
        let _ = client.cancel().await;
        let _ = server.cancel().await;
    }

    #[tokio::test]
    async fn restart_daemon_routes_through_the_client_and_returns_its_payload() {
        let fake = Arc::new(FakeClient::default());
        let (server, client) = connect(fake.clone(), base_options()).await;
        let response = client
            .call_tool(
                CallToolRequestParams::new("local_services_restart_daemon")
                    .with_arguments(args(json!({ "confirm": true }))),
            )
            .await
            .unwrap();
        assert_ne!(
            response.is_error,
            Some(true),
            "{:?}",
            response.content.first()
        );
        assert_eq!(fake.restarts.load(std::sync::atomic::Ordering::SeqCst), 1);
        let text = response.content[0].as_text().unwrap().text.clone();
        assert!(text.contains("restarted-instance"), "{text}");
        let _ = client.cancel().await;
        let _ = server.cancel().await;
    }

    #[tokio::test]
    async fn restart_daemon_requires_confirm_true_by_default() {
        let fake = Arc::new(FakeClient::default());
        let (server, client) = connect(fake.clone(), base_options()).await;
        let response = client
            .call_tool(CallToolRequestParams::new("local_services_restart_daemon"))
            .await
            .unwrap();
        assert_eq!(response.is_error, Some(true));
        assert_eq!(
            fake.restarts.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "an unconfirmed call must never reach the daemon"
        );
        let _ = client.cancel().await;
        let _ = server.cancel().await;
    }

    #[tokio::test]
    async fn restart_daemon_skips_the_confirm_requirement_when_disabled() {
        let fake = Arc::new(FakeClient::default());
        let options = CreateHearthMcpServerOptions {
            require_confirm: false,
            ..base_options()
        };
        let (server, client) = connect(fake.clone(), options).await;
        let response = client
            .call_tool(CallToolRequestParams::new("local_services_restart_daemon"))
            .await
            .unwrap();
        assert_ne!(
            response.is_error,
            Some(true),
            "{:?}",
            response.content.first()
        );
        assert_eq!(fake.restarts.load(std::sync::atomic::Ordering::SeqCst), 1);
        let _ = client.cancel().await;
        let _ = server.cancel().await;
    }

    /// `stop_daemon` is the destructive counterpart: daemon AND services go down, so it sits behind
    /// the same confirm gate and routes through the same client trait.
    #[tokio::test]
    async fn stop_daemon_routes_through_the_client() {
        let fake = Arc::new(FakeClient::default());
        let (server, client) = connect(fake.clone(), base_options()).await;
        let response = client
            .call_tool(
                CallToolRequestParams::new("local_services_stop_daemon")
                    .with_arguments(args(json!({ "confirm": true }))),
            )
            .await
            .unwrap();
        assert_ne!(
            response.is_error,
            Some(true),
            "{:?}",
            response.content.first()
        );
        assert_eq!(fake.stops.load(std::sync::atomic::Ordering::SeqCst), 1);
        let _ = client.cancel().await;
        let _ = server.cancel().await;
    }

    #[tokio::test]
    async fn stop_daemon_requires_confirm_true_by_default() {
        let fake = Arc::new(FakeClient::default());
        let (server, client) = connect(fake.clone(), base_options()).await;
        let response = client
            .call_tool(CallToolRequestParams::new("local_services_stop_daemon"))
            .await
            .unwrap();
        assert_eq!(response.is_error, Some(true));
        assert_eq!(
            fake.stops.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "an unconfirmed call must never reach the daemon"
        );
        let _ = client.cancel().await;
        let _ = server.cancel().await;
    }

    #[tokio::test]
    async fn stop_daemon_skips_the_confirm_requirement_when_disabled() {
        let fake = Arc::new(FakeClient::default());
        let options = CreateHearthMcpServerOptions {
            require_confirm: false,
            ..base_options()
        };
        let (server, client) = connect(fake.clone(), options).await;
        let response = client
            .call_tool(CallToolRequestParams::new("local_services_stop_daemon"))
            .await
            .unwrap();
        assert_ne!(
            response.is_error,
            Some(true),
            "{:?}",
            response.content.first()
        );
        assert_eq!(fake.stops.load(std::sync::atomic::Ordering::SeqCst), 1);
        let _ = client.cancel().await;
        let _ = server.cancel().await;
    }

    /// A service added by `manager reload` after the server started is accepted without an MCP
    /// server restart, and shows up in the tool schemas.
    #[tokio::test]
    async fn a_reloaded_catalog_is_picked_up_without_a_restart() {
        let fake = Arc::new(FakeClient::default());
        let (server, client) = connect(fake.clone(), base_options()).await;
        *fake.service_ids.lock().unwrap() = Some(vec![
            "metadata".to_string(),
            "mongo".to_string(),
            "search".to_string(),
        ]);
        let response = client
            .call_tool(
                CallToolRequestParams::new("local_services_manage").with_arguments(args(
                    json!({ "service": "search", "action": "start", "confirm": true }),
                )),
            )
            .await
            .unwrap();
        assert_ne!(
            response.is_error,
            Some(true),
            "{:?}",
            response.content.first()
        );
        assert_eq!(
            fake.managed.lock().unwrap().clone().unwrap().service,
            "search"
        );
        let tools = client.list_all_tools().await.unwrap();
        let manage = tools
            .iter()
            .find(|tool| tool.name == "local_services_manage")
            .unwrap();
        assert!(serde_json::to_string(&manage.input_schema)
            .unwrap()
            .contains("search"));
        let _ = client.cancel().await;
        let _ = server.cancel().await;
    }

    /// `shared_connection` exists to hand the agent a shared service's env — its `connection`
    /// subtree is not redacted, while the rest of the reply still is.
    #[tokio::test]
    async fn shared_connection_keeps_its_connection_env_but_redacts_everything_else() {
        let fake = Arc::new(FakeClient::default());
        let (server, client) = connect(fake, base_options()).await;
        let response = client
            .call_tool(
                CallToolRequestParams::new("local_services_shared_connection")
                    .with_arguments(args(json!({ "service": "minio" }))),
            )
            .await
            .unwrap();
        let text = response.content[0].as_text().unwrap().text.clone();
        assert!(text.contains("AWS_SECRET_ACCESS_KEY"), "{text}");
        assert!(!text.contains("must-not-leak"), "{text}");
        let _ = client.cancel().await;
        let _ = server.cancel().await;
    }
}
