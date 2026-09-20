//! Port of `src/bin/lsd.ts` — the generic declarative-config binary every consumer (a desktop app's
//! sidecar, a `local-services.yaml`-only project) shells out to. Phase 5 of the Rust-rewrite plan.
//! `lsd daemon --root <path>` is the daemon entrypoint this binary spawns detached; every other
//! subcommand delegates to `ls_cli::main`.
use std::path::{Path, PathBuf};

/// Mirrors `ls_cli::main`'s own `--root` extraction exactly (a leading `--root <path>`, nothing
/// more lenient) so this binary and `ls_cli::main` always agree on which project root a given
/// invocation means.
fn extract_root(argv: &[String]) -> (PathBuf, Vec<String>) {
    if argv.first().map(String::as_str) == Some("--root") {
        let root = argv.get(1).cloned().unwrap_or_else(|| std::env::current_dir().unwrap().to_string_lossy().to_string());
        (PathBuf::from(root), argv.get(2..).unwrap_or(&[]).to_vec())
    } else {
        (std::env::current_dir().unwrap(), argv.to_vec())
    }
}

fn report_config_error(root: &Path, errors: &[String]) {
    eprintln!("lsd: could not load a service catalog for {}", root.display());
    for error in errors {
        eprintln!("  - {error}");
    }
}

async fn run_daemon_subcommand(argv: &[String]) -> i32 {
    let (root, rest) = extract_root(argv);
    if !rest.is_empty() {
        eprintln!("usage: lsd daemon --root <path>");
        return 2;
    }
    let loaded = match ls_core::config_file::load_catalog(&root) {
        Ok(loaded) => loaded,
        Err(error) => {
            report_config_error(&root, &error.errors);
            return 1;
        }
    };
    let runtime_directory = ls_core::paths::resolve_runtime_directory(&root, loaded.catalog.runtime_directory.as_deref());
    let base_environment = ls_core::env::resolve_base_environment(ls_core::env::BaseEnvironmentOptions { root: Some(root.clone()), ..Default::default() });
    let supervisor = ls_core::supervisor::default_supervisor_options(root.clone(), Some(runtime_directory.clone()), Some(base_environment));
    ls_core::daemon::run_daemon(
        ls_core::manager::LocalServicesManagerOptions {
            runtime_directory: Some(runtime_directory),
            root: Some(root),
            catalog: loaded.catalog,
            event_capacity: None,
            log_tail_bytes: None,
            log_max_bytes: None,
            log_rotation_count: None,
            supervisor: Some(supervisor),
        },
        false,
    )
    .await;
    0
}

/// Re-invokes this same binary as `lsd daemon --root <root>`, detached (own process group, stdio
/// discarded) — mirrors `Bun.spawn([...], {detached:true}).unref()`. Rust has no `.unref()`
/// equivalent because that's a Node/Bun event-loop concept; the actual effect it achieves (the
/// parent can exit without waiting for or killing the child) falls out for free here since we
/// simply never call `.wait()` on the spawned child and let its `Child` handle drop.
fn spawn_daemon(root: &Path) {
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("lsd"));
    let root_arg = root.to_string_lossy().to_string();
    let mut command = std::process::Command::new(exe);
    command.args(["daemon", "--root", &root_arg]).current_dir(root).stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let _ = command.spawn();
}

/// Runs an MCP server over stdio for `root`'s catalog — the Rust on-ramp for an MCP host (an
/// editor/agent tool) that would otherwise need its own hand-rolled `createLocalServicesMcpServer`
/// wrapper. `require_confirm` stays at its safe default (`true`); `tool_prefix` is fixed at
/// `local_services_` to match the convention every real TS consumer already independently chose.
async fn run_mcp_subcommand(root: PathBuf, catalog: ls_core::catalog::ServiceCatalog) -> i32 {
    use rmcp::ServiceExt;
    let known_service_ids = catalog.services.iter().map(|s| s.id.clone()).collect();
    let options = ls_cli::LocalctlOptions { catalog, spawn_daemon: Box::new(spawn_daemon), doctor_checks: None };
    let client = ls_mcp::ManagerApiClient::new(root, options);
    let server = ls_mcp::create_local_services_mcp_server(
        std::sync::Arc::new(client),
        ls_mcp::CreateLocalServicesMcpServerOptions { name: "local-services".to_string(), tool_prefix: "local_services_".to_string(), known_service_ids, ..Default::default() },
    );
    let running = match server.serve(rmcp::transport::stdio()).await {
        Ok(running) => running,
        Err(error) => {
            eprintln!("lsd mcp: failed to start: {error}");
            return 1;
        }
    };
    match running.waiting().await {
        Ok(_) => 0,
        Err(error) => {
            eprintln!("lsd mcp: {error}");
            1
        }
    }
}

/// `--help`/`--version` must work from ANY directory. Every other subcommand needs a catalog, but
/// these two don't — and answering "how do I use this?" with "there is no config file here" is a
/// bad first experience for someone who just installed the binary.
fn print_help() {
    println!(
        "lsd — local dev services daemon, CLI, TUI and MCP server

usage: lsd [--root <path>] <command> [options]

  status [target] [--json]              current state of one service, a group, or all
  start|stop|restart <target> [--wait]  lifecycle actions (start brings up dependencies)
  logs <service> [--tail N] [--follow]  read a service's log
  operation get|watch <id> [--json]     inspect one operation
  doctor [--json]                       environment and catalog diagnostics
  cleanup                               remove stale runtime state
  manager ensure|status|stop|reload     daemon lifecycle
  daemon --root <path>                  run the daemon in the foreground (spawned internally)
  tui                                   interactive terminal UI
  mcp                                   serve the MCP tool surface over stdio
  mcp install [--name N] [--key K] <config-file>...
                                        register this binary in an MCP client config
  skill install --dest <path>           write the generic MCP skill doc

Every command except --help/--version resolves a catalog from --root (default: cwd):
local-services.yaml, .yml, .json, or .config.ts."
    );
}

async fn run_cli(argv: &[String]) -> i32 {
    let (root, rest) = extract_root(argv);
    match rest.first().map(String::as_str) {
        Some("--help") | Some("-h") | Some("help") | None => {
            print_help();
            return 0;
        }
        Some("--version") | Some("-V") => {
            println!("lsd {}", env!("CARGO_PKG_VERSION"));
            return 0;
        }
        _ => {}
    }
    let loaded = match ls_core::config_file::load_catalog(&root) {
        Ok(loaded) => loaded,
        Err(error) => {
            report_config_error(&root, &error.errors);
            return 1;
        }
    };
    // `tui` has no equivalent of the TS source's injectable `LocalctlRuntime.tui` handler — `ls-cli`
    // can't depend on `ls-tui` (that would be circular, since `ls-tui` itself depends on `ls-cli`
    // for its HTTP client), so this binary intercepts the subcommand itself before it ever reaches
    // `ls_cli::main` (whose own `tui` case exists only for a build that never wires one in at all).
    if rest.first().map(String::as_str) == Some("tui") {
        if rest.len() > 1 {
            eprintln!("usage: lsd tui");
            return 2;
        }
        return ls_tui::run_tui(ls_tui::RunTuiOptions {
            root: root.clone(),
            catalog: loaded.catalog,
            spawn_daemon: Box::new(spawn_daemon),
            refresh_interval: std::time::Duration::from_secs(10),
            service_kind: None,
        })
        .await;
    }
    // `mcp` has no TS equivalent in `lsd.ts`/`localctl.ts` either — every existing TS consumer
    // (infra's, viclass's own `src/mcp.ts`) hand-authors an ~20-line wrapper around
    // `createLocalServicesMcpServer` + a stdio transport instead. That duplication is only
    // reasonable while every consumer is a Bun/TS project that can just write one; a generic,
    // non-Bun `lsd` consumer has no equivalent on-ramp without this, so it earns a real subcommand
    // here rather than asking every future caller to hand-roll the same wrapper again.
    //
    // Only the *bare* `mcp` (serve-over-stdio) form is intercepted here — it alone needs `ls-mcp`/
    // `rmcp`, which `ls-cli` can't depend on any more than it can depend on `ls-tui` (same
    // circular-dependency reasoning as `tui` above). `mcp install ...` needs neither and is plain
    // file-manipulation logic, so it's implemented in `ls_cli::main` itself (alongside `skill
    // install`) and falls through to it below.
    if rest.first().map(String::as_str) == Some("mcp") && rest.len() == 1 {
        return run_mcp_subcommand(root, loaded.catalog).await;
    }
    let options = ls_cli::LocalctlOptions { catalog: loaded.catalog, spawn_daemon: Box::new(spawn_daemon), doctor_checks: None };
    let mut out = |s: &str| println!("{s}");
    let mut err = |s: &str| eprintln!("{s}");
    let mut io = ls_cli::Io { out: &mut out, err: &mut err };
    // The *full*, un-stripped argv is passed through — `ls_cli::main` does its own `--root`
    // extraction identically, exactly mirroring how the TS `runCli` passes its original argv to
    // `main()` rather than the root-stripped remainder computed just above for `load_catalog`.
    ls_cli::main(&options, argv, &mut io).await
}

#[tokio::main]
async fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let code = if argv.first().map(String::as_str) == Some("daemon") { run_daemon_subcommand(&argv[1..]).await } else { run_cli(&argv).await };
    std::process::exit(code);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_root_recognizes_a_leading_root_flag() {
        let argv: Vec<String> = ["--root", "/tmp/project", "status"].iter().map(|s| s.to_string()).collect();
        let (root, rest) = extract_root(&argv);
        assert_eq!(root, PathBuf::from("/tmp/project"));
        assert_eq!(rest, vec!["status".to_string()]);
    }

    #[test]
    fn extract_root_defaults_to_cwd_without_the_flag() {
        let argv: Vec<String> = ["status"].iter().map(|s| s.to_string()).collect();
        let (root, rest) = extract_root(&argv);
        assert_eq!(root, std::env::current_dir().unwrap());
        assert_eq!(rest, vec!["status".to_string()]);
    }
}
