//! Real end-to-end test of the actual compiled `lsd` binary — the closest Rust equivalent to
//! `test/bin/lsd.test.ts`, the one true subprocess-level test in the TS source, and also the
//! natural place to re-validate the Phase 0 signing/Gatekeeper spike against the *real* binary
//! (not just a hello-world compile) before any real consumer shells out to it.
use std::path::Path;
use std::process::Command;
use std::time::Duration;

fn lsd_bin() -> &'static str {
    env!("CARGO_BIN_EXE_lsd")
}

fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    port
}

fn write_config(root: &Path, port: u16) {
    let config = format!(
        r#"version: 1
services:
  api:
    run: {{ shell: "exec nc -lk {port}", exec: true }}
    readiness: {{ kind: tcp, port: {port} }}
"#
    );
    std::fs::write(root.join("local-services.yaml"), config).unwrap();
}

fn run_lsd(root: &Path, args: &[&str]) -> (i32, String, String) {
    let output = Command::new(lsd_bin()).arg("--root").arg(root).args(args).output().expect("failed to spawn lsd");
    (output.status.code().unwrap_or(-1), String::from_utf8_lossy(&output.stdout).to_string(), String::from_utf8_lossy(&output.stderr).to_string())
}

#[test]
fn ad_hoc_signed_binary_runs_without_being_killed() {
    // Phase 0 answered GO for a trivial hello-world binary; this reconfirms it for the actual
    // multi-thousand-line `lsd` binary, ad-hoc signed exactly the way a real packaging step would.
    let status = Command::new("codesign").args(["--force", "--sign", "-", lsd_bin()]).status();
    if let Ok(status) = status {
        assert!(status.success(), "ad-hoc codesign should succeed");
    }
    let output = Command::new(lsd_bin()).arg("--help-does-not-exist-but-should-still-run").output().expect("the ad-hoc-signed binary must still execute, not be killed on launch");
    // We don't care what it prints for a bogus command (a usage error is expected) — only that the
    // OS actually ran it and it exited normally rather than being terminated by a signal.
    assert!(output.status.code().is_some(), "process should exit normally, not be killed: {output:?}");
}

#[test]
fn full_lifecycle_over_the_real_compiled_binary() {
    let dir = tempfile::tempdir().unwrap();
    let port = free_port();
    write_config(dir.path(), port);

    // `manager ensure --json` — this is exactly the contract apps/macos's SidecarLocator.swift
    // depends on: it must spawn a detached daemon and print {instanceId, port, token,
    // protocolVersion, runtimeDirectory, root} as its sole stdout.
    let (code, stdout, stderr) = run_lsd(dir.path(), &["manager", "ensure", "--json"]);
    assert_eq!(code, 0, "stderr: {stderr}");
    let ensure_response: serde_json::Value = serde_json::from_str(stdout.trim()).expect("manager ensure --json must print exactly one JSON object");
    assert!(ensure_response["port"].as_u64().unwrap() > 0);
    assert!(ensure_response["token"].as_str().unwrap().len() > 10);
    assert_eq!(ensure_response["protocolVersion"], 1);

    // `start api --wait --json` — waits for the real nc-backed service to become ready.
    let (code, stdout, stderr) = run_lsd(dir.path(), &["start", "api", "--wait", "--json"]);
    assert_eq!(code, 0, "stdout: {stdout}, stderr: {stderr}");
    let start_response: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(start_response["operations"][0]["status"], "succeeded");

    // `status --json` — the service should now report ready with a real pid.
    let (code, stdout, _) = run_lsd(dir.path(), &["status", "--json"]);
    assert_eq!(code, 0);
    let status_response: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(status_response["services"][0]["state"], "ready");
    let pid = status_response["services"][0]["pid"].as_i64().expect("a ready service should report a pid");
    assert!(pid > 0);

    // `logs api` — should return without error (content is incidental for this test).
    let (code, _, stderr) = run_lsd(dir.path(), &["logs", "api", "--tail", "5"]);
    assert_eq!(code, 0, "stderr: {stderr}");

    // `manager stop --json` — a graceful, stop-services shutdown of the whole daemon.
    let (code, stdout, stderr) = run_lsd(dir.path(), &["manager", "stop", "--json"]);
    assert_eq!(code, 0, "stdout: {stdout}, stderr: {stderr}");

    // Give the detached daemon a moment to actually finish shutting down, then confirm a fresh
    // `manager ensure` starts an entirely new instance (proving the old one is really gone, not
    // just unresponsive).
    std::thread::sleep(Duration::from_millis(500));
    let (code, stdout, stderr) = run_lsd(dir.path(), &["manager", "ensure", "--json"]);
    assert_eq!(code, 0, "stderr: {stderr}");
    let second_ensure: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_ne!(second_ensure["instanceId"], ensure_response["instanceId"], "expected a fresh daemon instance after the previous one was stopped");

    // Final cleanup so this test doesn't leave a daemon running.
    let _ = run_lsd(dir.path(), &["manager", "stop", "--json"]);
}

#[test]
fn missing_config_file_reports_a_clear_error() {
    let dir = tempfile::tempdir().unwrap();
    let (code, _, stderr) = run_lsd(dir.path(), &["status"]);
    assert_eq!(code, 1);
    assert!(stderr.contains("could not load a service catalog"), "{stderr}");
}

/// Real end-to-end test of `lsd mcp`: spawns the actual compiled binary as a child process over
/// stdio (`rmcp`'s `TokioChildProcess`, the same transport a real MCP host like an editor/agent tool
/// uses), lists tools, and calls `status` against a real bootstrapped daemon it also spawns via
/// `manager ensure`. This is the Rust equivalent of infra's/viclass's own hand-rolled `src/mcp.ts`
/// wrapper — proving a non-Bun MCP host can get the exact same tool surface from this binary alone.
#[tokio::test]
async fn mcp_subcommand_serves_the_real_tool_surface_over_stdio() {
    use rmcp::model::CallToolRequestParams;
    use rmcp::transport::TokioChildProcess;
    use rmcp::{ClientHandler, ServiceExt};

    #[derive(Clone, Default)]
    struct NoopClientHandler;
    impl ClientHandler for NoopClientHandler {}

    let dir = tempfile::tempdir().unwrap();
    let port = free_port();
    write_config(dir.path(), port);

    // A running daemon isn't required for `mcp` to start serving tools — but `status` needs one to
    // actually answer, so ensure one's up first (same contract SidecarLocator/apps/macos rely on).
    let (code, _, stderr) = run_lsd(dir.path(), &["manager", "ensure", "--json"]);
    assert_eq!(code, 0, "stderr: {stderr}");

    let mut command = tokio::process::Command::new(lsd_bin());
    command.arg("--root").arg(dir.path()).arg("mcp");
    let transport = TokioChildProcess::new(command).expect("should spawn `lsd mcp`");
    let client = NoopClientHandler.serve(transport).await.expect("mcp client should initialize");

    let tools = client.list_all_tools().await.expect("list_tools should succeed");
    let names: Vec<String> = tools.into_iter().map(|tool| tool.name.to_string()).collect();
    assert_eq!(names, vec!["local_services_status", "local_services_logs", "local_services_trace", "local_services_events", "local_services_manage"]);

    let response = client.call_tool(CallToolRequestParams::new("local_services_status")).await.expect("status tool call should succeed");
    assert_ne!(response.is_error, Some(true), "{:?}", response.content.first());
    let text = response.content[0].as_text().unwrap().text.clone();
    let status: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(status["services"]["services"][0]["serviceId"], "api");

    let _ = client.cancel().await;
    let _ = run_lsd(dir.path(), &["manager", "stop", "--json"]);
}
