//! Real end-to-end test of the actual compiled `hearth` binary — the closest Rust equivalent to
//! `test/bin/hearth.test.ts`, the one true subprocess-level test in the TS source, and also the
//! natural place to re-validate the Phase 0 signing/Gatekeeper spike against the *real* binary
//! (not just a hello-world compile) before any real consumer shells out to it.
use std::path::Path;
use std::process::Command;
use std::time::Duration;

fn lsd_bin() -> &'static str {
    env!("CARGO_BIN_EXE_hearth")
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
    std::fs::write(root.join("hearth.yaml"), config).unwrap();
}

fn run_lsd(root: &Path, args: &[&str]) -> (i32, String, String) {
    let output = Command::new(lsd_bin())
        .arg("--root")
        .arg(root)
        .args(args)
        .output()
        .expect("failed to spawn hearth");
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout).to_string(),
        String::from_utf8_lossy(&output.stderr).to_string(),
    )
}

#[test]
fn ad_hoc_signed_binary_runs_without_being_killed() {
    // Phase 0 answered GO for a trivial hello-world binary; this reconfirms it for the actual
    // multi-thousand-line `hearth` binary, ad-hoc signed exactly the way a real packaging step would.
    let status = Command::new("codesign")
        .args(["--force", "--sign", "-", lsd_bin()])
        .status();
    if let Ok(status) = status {
        assert!(status.success(), "ad-hoc codesign should succeed");
    }
    let output = Command::new(lsd_bin())
        .arg("--help-does-not-exist-but-should-still-run")
        .output()
        .expect("the ad-hoc-signed binary must still execute, not be killed on launch");
    // We don't care what it prints for a bogus command (a usage error is expected) — only that the
    // OS actually ran it and it exited normally rather than being terminated by a signal.
    assert!(
        output.status.code().is_some(),
        "process should exit normally, not be killed: {output:?}"
    );
}

#[test]
fn full_lifecycle_over_the_real_compiled_binary() {
    let dir = tempfile::tempdir().unwrap();
    let port = free_port();
    write_config(dir.path(), port);

    // `manager ensure --json` — this is the connection contract a client uses to find the daemon.
    // depends on: it must spawn a detached daemon and print {instanceId, port, token,
    // protocolVersion, runtimeDirectory, root} as its sole stdout.
    let (code, stdout, stderr) = run_lsd(dir.path(), &["manager", "ensure", "--json"]);
    assert_eq!(code, 0, "stderr: {stderr}");
    let ensure_response: serde_json::Value = serde_json::from_str(stdout.trim())
        .expect("manager ensure --json must print exactly one JSON object");
    assert!(ensure_response["port"].as_u64().unwrap() > 0);
    assert!(ensure_response["token"].as_str().unwrap().len() > 10);
    assert_eq!(
        ensure_response["protocolVersion"].as_u64(),
        Some(u64::from(hearth_core::state::PROTOCOL_VERSION))
    );

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
    let pid = status_response["services"][0]["pid"]
        .as_i64()
        .expect("a ready service should report a pid");
    assert!(pid > 0);

    // `logs api` — should return without error (content is incidental for this test).
    let (code, _, stderr) = run_lsd(dir.path(), &["logs", "api", "--tail", "5"]);
    assert_eq!(code, 0, "stderr: {stderr}");

    // `manager stop --json` — a graceful, stop-services shutdown of the whole daemon.
    let (code, stdout, stderr) = run_lsd(dir.path(), &["manager", "stop", "--json"]);
    assert_eq!(code, 0, "stdout: {stdout}, stderr: {stderr}");

    // Confirm a fresh `manager ensure` starts an entirely new instance, proving the old one is
    // really gone rather than just unresponsive.
    //
    // POLLED, not a fixed sleep: shutdown is asynchronous in a detached process, and a fixed wait
    // is a bet on how long that takes. Under load (a full `cargo test --workspace`, or another
    // suite running alongside it) the old daemon was still alive when the single 500ms wait
    // expired, `ensure` correctly reused it, and the instance id matched — failing the test for a
    // reason that was never about the code under test. `ensure` is idempotent, so retrying it is
    // safe: it returns the live daemon until that daemon exits, then starts a new one. The
    // assertion is unchanged in strength — it still demands a genuinely different instance.
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    let second_ensure = loop {
        let (code, stdout, stderr) = run_lsd(dir.path(), &["manager", "ensure", "--json"]);
        assert_eq!(code, 0, "stderr: {stderr}");
        let response: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
        if response["instanceId"] != ensure_response["instanceId"] {
            break response;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the stopped daemon was still serving after 20s; `manager stop` did not take effect"
        );
        std::thread::sleep(Duration::from_millis(100));
    };
    assert_ne!(
        second_ensure["instanceId"], ensure_response["instanceId"],
        "expected a fresh daemon instance after the previous one was stopped"
    );

    // Final cleanup so this test doesn't leave a daemon running.
    let _ = run_lsd(dir.path(), &["manager", "stop", "--json"]);
}

/// `manager restart` is the one daemon lifecycle action that must NOT take the services down with
/// it: the daemon process is replaced, and the service it was running is re-adopted by the new one
/// from its persisted identity. Proven against the real compiled binary — a fresh `instanceId`, and
/// the *same* pid still answering `status` afterwards.
#[test]
fn manager_restart_replaces_the_daemon_and_keeps_services_running() {
    let dir = tempfile::tempdir().unwrap();
    let port = free_port();
    write_config(dir.path(), port);

    let (code, stdout, stderr) = run_lsd(dir.path(), &["manager", "ensure", "--json"]);
    assert_eq!(code, 0, "stderr: {stderr}");
    let first: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();

    let (code, stdout, stderr) = run_lsd(dir.path(), &["start", "api", "--wait", "--json"]);
    assert_eq!(code, 0, "stdout: {stdout}, stderr: {stderr}");
    let (code, stdout, stderr) = run_lsd(dir.path(), &["status", "--json"]);
    assert_eq!(code, 0, "stderr: {stderr}");
    let before: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(before["services"][0]["state"], "ready");
    let pid = before["services"][0]["pid"]
        .as_i64()
        .expect("a ready service should report a pid");

    // `manager restart --json` prints the same payload `manager ensure --json` does — the new
    // daemon's connection, which is what a client reconnects with.
    let (code, stdout, stderr) = run_lsd(dir.path(), &["manager", "restart", "--json"]);
    assert_eq!(code, 0, "stdout: {stdout}, stderr: {stderr}");
    let restarted: serde_json::Value = serde_json::from_str(stdout.trim())
        .expect("manager restart --json must print exactly one JSON object");
    assert_ne!(
        restarted["instanceId"], first["instanceId"],
        "restart must produce a new daemon instance"
    );
    assert!(restarted["port"].as_u64().unwrap() > 0);
    assert!(restarted["token"].as_str().unwrap().len() > 10);

    let (code, stdout, stderr) = run_lsd(dir.path(), &["status", "--json"]);
    assert_eq!(code, 0, "stderr: {stderr}");
    let after: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(
        after["services"][0]["state"], "ready",
        "the service must be re-adopted, not stopped"
    );
    assert_eq!(
        after["services"][0]["pid"].as_i64().unwrap(),
        pid,
        "the same process must still be serving"
    );

    let _ = run_lsd(dir.path(), &["manager", "stop", "--json"]);
}

#[test]
fn missing_config_file_reports_a_clear_error() {
    let dir = tempfile::tempdir().unwrap();
    let (code, _, stderr) = run_lsd(dir.path(), &["status"]);
    assert_eq!(code, 1);
    assert!(
        stderr.contains("could not load a service catalog"),
        "{stderr}"
    );
}

/// Real end-to-end test of `hearth mcp`: spawns the actual compiled binary as a child process over
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
    // actually answer, so ensure one's up first.
    let (code, _, stderr) = run_lsd(dir.path(), &["manager", "ensure", "--json"]);
    assert_eq!(code, 0, "stderr: {stderr}");

    let mut command = tokio::process::Command::new(lsd_bin());
    command.arg("--root").arg(dir.path()).arg("mcp");
    let transport = TokioChildProcess::new(command).expect("should spawn `hearth mcp`");
    let client = NoopClientHandler
        .serve(transport)
        .await
        .expect("mcp client should initialize");

    let tools = client
        .list_all_tools()
        .await
        .expect("list_tools should succeed");
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
        .expect("status tool call should succeed");
    assert_ne!(
        response.is_error,
        Some(true),
        "{:?}",
        response.content.first()
    );
    let text = response.content[0].as_text().unwrap().text.clone();
    let status: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(status["services"]["services"][0]["serviceId"], "api");

    let _ = client.cancel().await;
    let _ = run_lsd(dir.path(), &["manager", "stop", "--json"]);
}

/// Real end-to-end test of `hearth mcp install`: the whole point of resolving `std::env::current_exe()`
/// inside `hearth_cli`'s `mcp_install_command` is that it names *this actual compiled binary*, not some
/// dev-time cargo artifact path or a symlink — the only way to prove that is to run the real
/// binary and check what it wrote about itself.
#[test]
fn mcp_install_writes_the_real_compiled_binarys_own_path() {
    let dir = tempfile::tempdir().unwrap();
    write_config(dir.path(), free_port());
    let config_path = dir.path().join(".mcp.json");
    let (code, stdout, stderr) = run_lsd(
        dir.path(),
        &["mcp", "install", config_path.to_str().unwrap()],
    );
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(
        stdout.contains("installed mcp server \"hearth\""),
        "{stdout}"
    );

    let written: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&config_path).unwrap()).unwrap();
    let entry = &written["mcpServers"]["hearth"];
    assert_eq!(entry["command"], serde_json::json!(lsd_bin()));
    let expected_root = std::fs::canonicalize(dir.path())
        .unwrap()
        .to_string_lossy()
        .to_string();
    assert_eq!(
        entry["args"],
        serde_json::json!(["--root", expected_root, "mcp"])
    );

    // Merging again (as a re-install would) must not disturb an unrelated sibling entry.
    let mut existing: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&config_path).unwrap()).unwrap();
    existing["mcpServers"]["other"] =
        serde_json::json!({ "command": "node", "args": ["other.mjs"] });
    std::fs::write(&config_path, existing.to_string()).unwrap();
    let (code, _, stderr) = run_lsd(
        dir.path(),
        &["mcp", "install", config_path.to_str().unwrap()],
    );
    assert_eq!(code, 0, "stderr: {stderr}");
    let written: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&config_path).unwrap()).unwrap();
    assert_eq!(
        written["mcpServers"]["other"]["command"],
        serde_json::json!("node")
    );
}

/// The compiled binary must install a skill pack (SKILL.md plus executable scripts), not a
/// single markdown file. `--dest` is the skill directory.
#[test]
fn skill_install_writes_the_real_binarys_skill_pack() {
    let dir = tempfile::tempdir().unwrap();
    write_config(dir.path(), free_port());
    let dest = ".agent/skills/local-dev";
    let (code, stdout, stderr) = run_lsd(dir.path(), &["skill", "install", "--dest", dest]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(stdout.contains("installed hearth skill"), "{stdout}");

    let skill_dir = dir.path().join(dest);
    let written = std::fs::read_to_string(skill_dir.join("SKILL.md")).unwrap();
    assert!(written.contains("local_services_manage"), "{written}");
    assert!(written.contains("scripts/manage.sh"), "{written}");
    for script in [
        "hearth.sh",
        "manage.sh",
        "status.sh",
        "shared-connection.sh",
    ] {
        let path = skill_dir.join("scripts").join(script);
        assert!(path.is_file(), "missing skill script {}", path.display());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(skill_dir.join("scripts/hearth.sh"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o111, 0o111, "hearth.sh should be executable");
    }
}

/// `--help` and `--version` must work from a directory with no catalog — every other subcommand
/// resolves one, and answering "how do I use this?" with "no config file found here" is a bad first
/// experience for someone who has just installed the binary. Regression test: these used to fail.
#[test]
fn help_and_version_work_outside_a_project() {
    let empty = tempfile::tempdir().unwrap();
    for args in [vec!["--help"], vec!["-h"], vec![]] {
        let output = std::process::Command::new(lsd_bin())
            .args(&args)
            .current_dir(empty.path())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "hearth {args:?} failed outside a project: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.contains("usage: hearth"),
            "hearth {args:?} printed no usage: {stdout}"
        );
    }
    let version = std::process::Command::new(lsd_bin())
        .arg("--version")
        .current_dir(empty.path())
        .output()
        .unwrap();
    assert!(version.status.success());
    assert!(String::from_utf8_lossy(&version.stdout).starts_with("hearth "));
}
