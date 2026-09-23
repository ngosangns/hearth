//! Port of the `createHearthMcpServer` half of `src/mcp/mcp-server.ts` — manual
//! `ServerHandler` implementation (not the `#[tool_router]`/`#[tool]` declarative macros) because
//! the tool set here is only known at construction time: tool names carry a runtime-configurable
//! prefix, the `manage`/`status`/`logs` schemas embed an `enum` of the caller's actual
//! `knownServiceIds`, and the `manage` tool's required-fields list depends on `requireConfirm`. None
//! of that fits a compile-time-generated schema, so this overrides `get_info`/`list_tools`/
//! `call_tool` directly and leaves every other `ServerHandler` method (resources, prompts,
//! subscriptions, tasks, discover — none of which this server implements) at its provided default,
//! exactly mirroring how the TS source's `Server` only ever registers `ListToolsRequestSchema` and
//! `CallToolRequestSchema` handlers and lets the SDK answer everything else.
use std::sync::Arc;

use regex::Regex;
use rmcp::model::{CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation, JsonObject, ListToolsResult, PaginatedRequestParams, ServerCapabilities, ServerConfig, Tool};
use rmcp::service::RequestContext;
use rmcp::{ErrorData as McpError, RoleServer, ServerHandler};
use serde_json::{json, Value};

use crate::client::{EventsArguments, HearthMcpClient, LogsArguments, ManageAction, ManageArguments, StatusArguments, TraceArguments};

fn secret_key_pattern() -> &'static Regex {
    use std::sync::OnceLock;
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)authorization|token|ownership(?:key|proof)|secret|password|api[_-]?key").unwrap())
}

/// Strips anything that looks like a credential out of a value before it reaches the model —
/// recurses through arrays/objects, dropping any object key matching `SECRET_KEY_PATTERN`.
fn redact(value: &Value) -> Value {
    redact_at_depth(value, 0)
}

fn redact_at_depth(value: &Value, depth: u32) -> Value {
    if depth > 12 {
        return Value::String("[truncated]".to_string());
    }
    match value {
        Value::Array(items) => Value::Array(items.iter().map(|entry| redact_at_depth(entry, depth + 1)).collect()),
        Value::Object(map) => Value::Object(map.iter().filter(|(key, _)| !secret_key_pattern().is_match(key)).map(|(key, entry)| (key.clone(), redact_at_depth(entry, depth + 1))).collect()),
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
    pub known_service_ids: Vec<String>,
}

impl Default for CreateHearthMcpServerOptions {
    fn default() -> Self {
        Self { name: "hearth".to_string(), version: None, tool_prefix: String::new(), require_confirm: true, known_service_ids: Vec::new() }
    }
}

#[derive(Clone)]
pub struct HearthMcpServer {
    client: Arc<dyn HearthMcpClient>,
    options: Arc<CreateHearthMcpServerOptions>,
}

pub fn create_hearth_mcp_server(client: Arc<dyn HearthMcpClient>, options: CreateHearthMcpServerOptions) -> HearthMcpServer {
    HearthMcpServer { client, options: Arc::new(options) }
}

fn require_only_keys(value: &JsonObject, allowed: &[&str]) -> Result<(), String> {
    let unexpected: Vec<&str> = value.keys().map(String::as_str).filter(|key| !allowed.contains(key)).collect();
    if unexpected.is_empty() {
        Ok(())
    } else {
        let suffix = if unexpected.len() > 1 { "s" } else { "" };
        Err(format!("unexpected argument{suffix}: {}", unexpected.join(", ")))
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

fn optional_integer(value: Option<&Value>, name: &str, minimum: i64, maximum: Option<i64>) -> Result<Option<i64>, String> {
    let range_error = || {
        let range = match maximum {
            Some(max) => format!("between {minimum} and {max}"),
            None => format!("at least {minimum}"),
        };
        format!("{name} must be an integer {range}")
    };
    // Matches JavaScript's `Number.isSafeInteger`, which is what the TS server validates with —
    // and what every MCP host's JSON layer ultimately produces. Two ways this used to diverge:
    // a host that serializes numbers as floats sent `1000.0`, which `as_i64()` rejects even though
    // it is an integer; and a value beyond 2^53 was accepted here while the TS server rejected it
    // as unrepresentable. Both made the same tool call succeed on one implementation and fail on
    // the other.
    const MAX_SAFE_INTEGER: i64 = 9_007_199_254_740_991;
    match value {
        None => Ok(None),
        Some(Value::Number(number)) => {
            let candidate = match number.as_i64() {
                Some(int) => Some(int),
                None => number.as_f64().filter(|f| f.fract() == 0.0).map(|f| f as i64),
            };
            match candidate {
                Some(int) if int.abs() <= MAX_SAFE_INTEGER && int >= minimum && maximum.is_none_or(|max| int <= max) => Ok(Some(int)),
                _ => Err(range_error()),
            }
        }
        _ => Err(range_error()),
    }
}

impl HearthMcpServer {
    fn require_service(&self, value: Option<&Value>) -> Result<String, String> {
        let service = required_string(value, "service")?;
        if self.options.known_service_ids.iter().any(|id| id == &service) {
            Ok(service)
        } else {
            Err(format!("unknown service: {service}. Known services: {}", self.options.known_service_ids.join(", ")))
        }
    }

    fn parse_status(&self, arguments: &JsonObject) -> Result<StatusArguments, String> {
        require_only_keys(arguments, &["service"])?;
        Ok(StatusArguments { service: arguments.get("service").map(|value| self.require_service(Some(value))).transpose()? })
    }

    fn parse_logs(&self, arguments: &JsonObject) -> Result<LogsArguments, String> {
        require_only_keys(arguments, &["service", "cursor", "generation", "limit"])?;
        Ok(LogsArguments {
            service: self.require_service(arguments.get("service"))?,
            cursor: optional_integer(arguments.get("cursor"), "cursor", 0, None)?.map(|v| v as u64),
            generation: optional_integer(arguments.get("generation"), "generation", 1, None)?.map(|v| v as u64),
            limit: optional_integer(arguments.get("limit"), "limit", 1, Some(64 * 1024))?.map(|v| v as u64),
        })
    }

    fn parse_trace(&self, arguments: &JsonObject) -> Result<TraceArguments, String> {
        require_only_keys(arguments, &["operationId"])?;
        Ok(TraceArguments { operation_id: required_string(arguments.get("operationId"), "operationId")? })
    }

    fn parse_events(&self, arguments: &JsonObject) -> Result<EventsArguments, String> {
        require_only_keys(arguments, &["after", "epoch"])?;
        Ok(EventsArguments { after: optional_integer(arguments.get("after"), "after", 0, None)?.map(|v| v as u64), epoch: optional_string(arguments.get("epoch"), "epoch")? })
    }

    fn parse_manage(&self, arguments: &JsonObject) -> Result<ManageArguments, String> {
        let allowed: &[&str] = if self.options.require_confirm { &["service", "action", "confirm", "killUnowned"] } else { &["service", "action", "killUnowned"] };
        require_only_keys(arguments, allowed)?;
        if self.options.require_confirm && arguments.get("confirm") != Some(&Value::Bool(true)) {
            return Err("manage requires confirm=true (explicit user approval) — never call this speculatively".to_string());
        }
        let service = self.require_service(arguments.get("service"))?;
        let action = match arguments.get("action").and_then(Value::as_str) {
            Some("start") => ManageAction::Start,
            Some("stop") => ManageAction::Stop,
            Some("restart") => ManageAction::Restart,
            _ => return Err("action must be one of: start, stop, restart".to_string()),
        };
        let kill_unowned = match arguments.get("killUnowned") {
            None | Some(Value::Bool(false)) => false,
            Some(Value::Bool(true)) => true,
            _ => return Err("killUnowned must be a boolean".to_string()),
        };
        if kill_unowned && action != ManageAction::Start {
            return Err("killUnowned only applies to action=start".to_string());
        }
        Ok(ManageArguments { service, action, kill_unowned })
    }

    /// The daemon-lifecycle tools take no arguments beyond the confirm gate — one daemon per
    /// project root means there is nothing to select.
    fn parse_daemon_lifecycle(&self, tool: &str, arguments: &JsonObject) -> Result<(), String> {
        let allowed: &[&str] = if self.options.require_confirm { &["confirm"] } else { &[] };
        require_only_keys(arguments, allowed)?;
        if self.options.require_confirm && arguments.get("confirm") != Some(&Value::Bool(true)) {
            return Err(format!("{tool} requires confirm=true (explicit user approval) — never call this speculatively"));
        }
        Ok(())
    }

    async fn dispatch(&self, name: &str, arguments: JsonObject) -> Result<Value, String> {
        let Some(tool) = name.strip_prefix(self.options.tool_prefix.as_str()) else { return Err(format!("unknown tool: {name}")) };
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
            _ => Err(format!("unknown tool: {name}")),
        }
    }

    fn tool_definitions(&self) -> Vec<Tool> {
        let prefix = &self.options.tool_prefix;
        let require_confirm = self.options.require_confirm;
        let service_ids = &self.options.known_service_ids;
        let schema = |value: Value| -> Arc<JsonObject> { Arc::new(value.as_object().cloned().unwrap_or_default()) };

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
        let manage_description = if require_confirm { "Start/stop/restart a local dev service. Requires confirm=true (explicit user approval) — never call this speculatively." } else { "Start/stop/restart a local dev service. MCP hosts should require approval for this tool." };

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
                        "generation": { "type": "integer", "minimum": 1 },
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
        ]
    }
}

impl ServerHandler for HearthMcpServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build()).with_server_info(Implementation::new(self.options.name.clone(), self.options.version.clone().unwrap_or_else(|| "1.0.0".to_string())))
    }

    async fn list_tools(&self, _request: Option<PaginatedRequestParams>, _context: RequestContext<RoleServer>) -> Result<ListToolsResult, McpError> {
        Ok(ListToolsResult::with_all_items(self.tool_definitions()))
    }

    async fn call_tool(&self, request: CallToolRequestParams, _context: RequestContext<RoleServer>) -> Result<CallToolResponse, McpError> {
        let arguments = request.arguments.unwrap_or_default();
        let result = match self.dispatch(&request.name, arguments).await {
            Ok(value) => CallToolResult::success(vec![ContentBlock::text(serde_json::to_string_pretty(&redact(&value)).unwrap())]),
            Err(message) => CallToolResult::error(vec![ContentBlock::text(safe_error_message(&message))]),
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

    #[derive(Clone, Default)]
    struct NoopClientHandler;
    impl ClientHandler for NoopClientHandler {}

    #[derive(Default)]
    struct FakeClient {
        managed: Mutex<Option<ManageArguments>>,
        restarts: std::sync::atomic::AtomicU32,
        stops: std::sync::atomic::AtomicU32,
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
            self.restarts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(json!({ "instanceId": "restarted-instance" }))
        }
        async fn stop_daemon(&self) -> Result<Value, String> {
            self.stops.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(json!({ "operation": "accepted" }))
        }
    }

    fn base_options() -> CreateHearthMcpServerOptions {
        CreateHearthMcpServerOptions { name: "test-hearth".to_string(), tool_prefix: "local_services_".to_string(), known_service_ids: vec!["metadata".to_string(), "mongo".to_string()], ..Default::default() }
    }

    fn args(value: Value) -> JsonObject {
        value.as_object().cloned().unwrap_or_default()
    }

    type Connected = (rmcp::service::RunningService<RoleServer, HearthMcpServer>, rmcp::service::RunningService<rmcp::RoleClient, NoopClientHandler>);

    async fn connect(client: Arc<dyn HearthMcpClient>, options: CreateHearthMcpServerOptions) -> Connected {
        let server = create_hearth_mcp_server(client, options);
        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        // The initialize handshake is a round trip: each side's `.serve()` blocks until it hears
        // from the other, so both must run concurrently rather than one after the other.
        let (server_result, client_result) = tokio::join!(server.serve(server_io), NoopClientHandler.serve(client_io));
        (server_result.expect("server should initialize"), client_result.expect("client should initialize"))
    }

    #[tokio::test]
    async fn routes_focused_application_management_through_the_ordinary_manager_client() {
        let fake = Arc::new(FakeClient::default());
        let (server, client) = connect(fake.clone(), base_options()).await;
        let response = client.call_tool(CallToolRequestParams::new("local_services_manage").with_arguments(args(json!({ "service": "metadata", "action": "restart", "confirm": true })))).await.unwrap();
        assert_ne!(response.is_error, Some(true));
        let managed = fake.managed.lock().unwrap().clone().unwrap();
        assert_eq!(managed.service, "metadata");
        assert_eq!(managed.action, ManageAction::Restart);
        let _ = client.cancel().await;
        let _ = server.cancel().await;
    }

    #[tokio::test]
    async fn routes_focused_infrastructure_management_through_the_ordinary_manager_client() {
        let fake = Arc::new(FakeClient::default());
        let (server, client) = connect(fake.clone(), base_options()).await;
        let response = client.call_tool(CallToolRequestParams::new("local_services_manage").with_arguments(args(json!({ "service": "mongo", "action": "restart", "confirm": true })))).await.unwrap();
        assert_ne!(response.is_error, Some(true));
        let managed = fake.managed.lock().unwrap().clone().unwrap();
        assert_eq!(managed.service, "mongo");
        assert_eq!(managed.action, ManageAction::Restart);
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
            assert_eq!(response.is_error, Some(true), "killUnowned on action={action} must be rejected");
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
        let response = client.call_tool(CallToolRequestParams::new("local_services_manage").with_arguments(args(json!({ "service": "metadata", "action": "restart" })))).await.unwrap();
        assert_eq!(response.is_error, Some(true));
        assert!(fake.managed.lock().unwrap().is_none());
        let _ = client.cancel().await;
        let _ = server.cancel().await;
    }

    #[tokio::test]
    async fn manage_skips_the_confirm_requirement_when_disabled() {
        let fake = Arc::new(FakeClient::default());
        let options = CreateHearthMcpServerOptions { require_confirm: false, ..base_options() };
        let (server, client) = connect(fake.clone(), options).await;
        let response = client.call_tool(CallToolRequestParams::new("local_services_manage").with_arguments(args(json!({ "service": "metadata", "action": "restart" })))).await.unwrap();
        assert_ne!(response.is_error, Some(true));
        let managed = fake.managed.lock().unwrap().clone().unwrap();
        assert_eq!(managed.service, "metadata");
        assert_eq!(managed.action, ManageAction::Restart);
        let _ = client.cancel().await;
        let _ = server.cancel().await;
    }

    #[tokio::test]
    async fn read_only_tools_need_no_confirm_and_are_registered_under_the_configured_prefix() {
        let fake = Arc::new(FakeClient::default());
        let (server, client) = connect(fake, base_options()).await;
        let tools = client.list_all_tools().await.unwrap();
        let names: Vec<String> = tools.into_iter().map(|tool| tool.name.to_string()).collect();
        assert_eq!(names, vec!["local_services_status", "local_services_logs", "local_services_trace", "local_services_events", "local_services_manage", "local_services_restart_daemon", "local_services_stop_daemon"]);
        let response = client.call_tool(CallToolRequestParams::new("local_services_status")).await.unwrap();
        assert_ne!(response.is_error, Some(true));
        let _ = client.cancel().await;
        let _ = server.cancel().await;
    }

    #[tokio::test]
    async fn restart_daemon_routes_through_the_client_and_returns_its_payload() {
        let fake = Arc::new(FakeClient::default());
        let (server, client) = connect(fake.clone(), base_options()).await;
        let response = client.call_tool(CallToolRequestParams::new("local_services_restart_daemon").with_arguments(args(json!({ "confirm": true })))).await.unwrap();
        assert_ne!(response.is_error, Some(true), "{:?}", response.content.first());
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
        let response = client.call_tool(CallToolRequestParams::new("local_services_restart_daemon")).await.unwrap();
        assert_eq!(response.is_error, Some(true));
        assert_eq!(fake.restarts.load(std::sync::atomic::Ordering::SeqCst), 0, "an unconfirmed call must never reach the daemon");
        let _ = client.cancel().await;
        let _ = server.cancel().await;
    }

    #[tokio::test]
    async fn restart_daemon_skips_the_confirm_requirement_when_disabled() {
        let fake = Arc::new(FakeClient::default());
        let options = CreateHearthMcpServerOptions { require_confirm: false, ..base_options() };
        let (server, client) = connect(fake.clone(), options).await;
        let response = client.call_tool(CallToolRequestParams::new("local_services_restart_daemon")).await.unwrap();
        assert_ne!(response.is_error, Some(true), "{:?}", response.content.first());
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
        let response = client.call_tool(CallToolRequestParams::new("local_services_stop_daemon").with_arguments(args(json!({ "confirm": true })))).await.unwrap();
        assert_ne!(response.is_error, Some(true), "{:?}", response.content.first());
        assert_eq!(fake.stops.load(std::sync::atomic::Ordering::SeqCst), 1);
        let _ = client.cancel().await;
        let _ = server.cancel().await;
    }

    #[tokio::test]
    async fn stop_daemon_requires_confirm_true_by_default() {
        let fake = Arc::new(FakeClient::default());
        let (server, client) = connect(fake.clone(), base_options()).await;
        let response = client.call_tool(CallToolRequestParams::new("local_services_stop_daemon")).await.unwrap();
        assert_eq!(response.is_error, Some(true));
        assert_eq!(fake.stops.load(std::sync::atomic::Ordering::SeqCst), 0, "an unconfirmed call must never reach the daemon");
        let _ = client.cancel().await;
        let _ = server.cancel().await;
    }

    #[tokio::test]
    async fn stop_daemon_skips_the_confirm_requirement_when_disabled() {
        let fake = Arc::new(FakeClient::default());
        let options = CreateHearthMcpServerOptions { require_confirm: false, ..base_options() };
        let (server, client) = connect(fake.clone(), options).await;
        let response = client.call_tool(CallToolRequestParams::new("local_services_stop_daemon")).await.unwrap();
        assert_ne!(response.is_error, Some(true), "{:?}", response.content.first());
        assert_eq!(fake.stops.load(std::sync::atomic::Ordering::SeqCst), 1);
        let _ = client.cancel().await;
        let _ = server.cancel().await;
    }
}
