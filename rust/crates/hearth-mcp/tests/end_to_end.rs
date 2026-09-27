//! Real end-to-end test: a real bootstrapped `HearthManager`, a real `nc -lk`-backed TCP
//! service, driven entirely over the real MCP tool surface (a real in-process client/server pair
//! connected over a `tokio::io::duplex` transport, exactly like `mcp-server.test.ts`'s
//! `InMemoryTransport`) — the same category of capstone test that closed out every earlier phase.
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use hearth_core::catalog::{CommandSpec, ReadinessSpec, ServiceCatalog, ServiceCommand, ServiceDefinition, ServiceKind, ServiceProfiles, ServiceRunProfile, StartFailurePolicy};
use hearth_core::manager::{bootstrap, HearthManagerOptions};
use hearth_cli::LocalctlOptions;
use hearth_mcp::{create_hearth_mcp_server, CreateHearthMcpServerOptions, ManagerApiClient};
use rmcp::model::CallToolRequestParams;
use rmcp::{ClientHandler, ServiceExt};
use serde_json::Value;

#[derive(Clone, Default)]
struct NoopClientHandler;
impl ClientHandler for NoopClientHandler {}

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
        disabled: false,
        profiles: ServiceProfiles {
            run: ServiceRunProfile::Verified { command: ServiceCommand { command: CommandSpec::Shell { shell: format!("exec nc -lk {port}"), exec: Some(true) }, cwd: "/tmp".to_string(), environment: None, container_name: None, docker_stop_command: None }, readiness: ReadinessSpec::Tcp { port }, readiness_timeout_ms: Some(5_000), preparation: None, preparation_command: None },
            build: None,
        },
        ports: None,
        urls: Some(vec![hearth_core::catalog::ServiceUrl { url: "http://127.0.0.1:18090/".into(), label: Some("app".into()), requires_running: None }]),
        artifact: None,
    }
}

fn args(value: Value) -> rmcp::model::JsonObject {
    value.as_object().cloned().unwrap_or_default()
}

#[tokio::test]
async fn full_tool_lifecycle_over_a_real_bootstrapped_manager() {
    let dir = tempfile::tempdir().unwrap();
    let port = free_port();
    let catalog = ServiceCatalog { services: vec![tcp_service("api", port)], groups: HashMap::new(), group_tree: Vec::new(), compose_file: None, runtime_directory: Some(dir.path().to_string_lossy().to_string()), start_failure_policy: StartFailurePolicy::StopOnFirstFailureKeepStarted, private_file_guard: Some(false) };
    let manager = bootstrap(HearthManagerOptions { runtime_directory: None, root: Some(PathBuf::from("/tmp")), catalog: catalog.clone(), event_capacity: None, log_tail_bytes: None, log_max_bytes: None, log_rotation_count: None, supervisor: None, shared: None }).await.unwrap();

    let options = LocalctlOptions { catalog, spawn_daemon: Box::new(|_| panic!("a running manager should never need spawning")) };
    let client = ManagerApiClient::new(PathBuf::from("/tmp"), options);
    let server = create_hearth_mcp_server(Arc::new(client), CreateHearthMcpServerOptions { name: "hearth".to_string(), tool_prefix: String::new(), known_service_ids: vec!["api".to_string()], ..Default::default() });

    let (client_io, server_io) = tokio::io::duplex(128 * 1024);
    let (server_result, client_result) = tokio::join!(server.serve(server_io), NoopClientHandler.serve(client_io));
    let server_running = server_result.unwrap();
    let mcp_client = client_result.unwrap();

    // status — fresh catalog, nothing started yet.
    let status = mcp_client.call_tool(CallToolRequestParams::new("status")).await.unwrap();
    assert_ne!(status.is_error, Some(true));
    let status_text = status.content[0].as_text().unwrap().text.clone();
    let status_json: Value = serde_json::from_str(&status_text).unwrap();
    assert_eq!(status_json["services"]["services"][0]["actualState"], "stopped");
    // `urls` rides along on `status`, so an agent can tell a user where a service lives.
    assert_eq!(status_json["urls"]["urls"][0]["url"], "http://127.0.0.1:18090/");
    assert_eq!(status_json["urls"]["urls"][0]["label"], "app");

    // manage start — waits for the real nc-backed process to answer real TCP readiness, then
    // returns the post-start status inline (mirroring `ManagerApiClient.manage`'s own status()
    // call at the end).
    let manage = mcp_client.call_tool(CallToolRequestParams::new("manage").with_arguments(args(serde_json::json!({ "service": "api", "action": "start", "confirm": true })))).await.unwrap();
    assert_ne!(manage.is_error, Some(true), "{:?}", manage.content[0].as_text());
    let manage_text = manage.content[0].as_text().unwrap().text.clone();
    let manage_json: Value = serde_json::from_str(&manage_text).unwrap();
    let ready_service = &manage_json["services"]["services"][0];
    assert_eq!(ready_service["actualState"], "ready");
    assert!(ready_service["identity"]["pid"].as_i64().unwrap() > 0);

    // logs — real, bounded log chunk for the started service.
    let logs = mcp_client.call_tool(CallToolRequestParams::new("logs").with_arguments(args(serde_json::json!({ "service": "api" })))).await.unwrap();
    assert_ne!(logs.is_error, Some(true));

    // events — real recent manager events since sequence 0.
    let events = mcp_client.call_tool(CallToolRequestParams::new("events").with_arguments(args(serde_json::json!({ "after": 0 })))).await.unwrap();
    assert_ne!(events.is_error, Some(true));
    let events_text = events.content[0].as_text().unwrap().text.clone();
    let events_json: Value = serde_json::from_str(&events_text).unwrap();
    assert!(events_json["events"].as_array().unwrap().iter().any(|event| event["type"] == "service.lifecycle"));

    // trace — a made-up operation id should surface as a tool-level error, not a protocol error
    // (the caller's client should still get the daemon's own "operation not found" message).
    let trace = mcp_client.call_tool(CallToolRequestParams::new("trace").with_arguments(args(serde_json::json!({ "operationId": "does-not-exist" })))).await.unwrap();
    assert_eq!(trace.is_error, Some(true));

    // manage stop — real process should really exit.
    let stop = mcp_client.call_tool(CallToolRequestParams::new("manage").with_arguments(args(serde_json::json!({ "service": "api", "action": "stop", "confirm": true })))).await.unwrap();
    assert_ne!(stop.is_error, Some(true), "{:?}", stop.content[0].as_text());
    let stop_text = stop.content[0].as_text().unwrap().text.clone();
    let stop_json: Value = serde_json::from_str(&stop_text).unwrap();
    assert_eq!(stop_json["services"]["services"][0]["actualState"], "stopped");

    let _ = mcp_client.cancel().await;
    let _ = server_running.cancel().await;
    manager.close().await;
}
