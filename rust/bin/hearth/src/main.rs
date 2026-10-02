//! `hearth`: the daemon, the CLI, the TUI, and the MCP server in one binary.
//! `hearth daemon` and `hearth smp` run a manager in the foreground (and are what `ensure` spawns
//! detached); `tui` and bare `mcp` are intercepted here because `hearth-cli` cannot depend on
//! `hearth-tui`/`hearth-mcp`; every other subcommand delegates to `hearth_cli::main`.
use std::path::{Path, PathBuf};

fn report_config_error(root: &Path, errors: &[String]) {
    eprintln!(
        "hearth: could not load a service catalog for {}",
        root.display()
    );
    for error in errors {
        eprintln!("  - {error}");
    }
}

/// Runs one manager in the foreground until it shuts down. A bootstrap failure exits non-zero:
/// the daemon is spawned by `ensure()`, and exiting 0 after failing to start would make a broken
/// daemon indistinguishable from a healthy one.
async fn run_manager(
    root: PathBuf,
    runtime_directory: PathBuf,
    catalog: hearth_core::catalog::ServiceCatalog,
    shared: Option<std::sync::Arc<hearth_core::shared::SharedContext>>,
) -> i32 {
    let base_environment =
        hearth_core::env::resolve_base_environment(hearth_core::env::BaseEnvironmentOptions {
            root: Some(root.clone()),
            ..Default::default()
        });
    let supervisor = hearth_core::supervisor::default_supervisor_options(
        root.clone(),
        Some(runtime_directory.clone()),
        Some(base_environment),
    );
    let started = hearth_core::daemon::run_daemon(
        hearth_core::manager::HearthManagerOptions {
            runtime_directory: Some(runtime_directory),
            root: Some(root),
            catalog,
            event_capacity: None,
            log_tail_bytes: None,
            log_max_bytes: None,
            log_rotation_count: None,
            supervisor: Some(supervisor),
            shared,
        },
        hearth_core::manager::ShutdownMode::LeaveServices,
    )
    .await;
    if started {
        0
    } else {
        1
    }
}

async fn run_daemon_subcommand(root: PathBuf, rest: &[String]) -> i32 {
    if !rest.is_empty() {
        eprintln!("usage: hearth daemon --root <path>");
        return 2;
    }
    let loaded = match hearth_core::config_file::load_catalog(&root) {
        Ok(loaded) => loaded,
        Err(error) => {
            report_config_error(&root, &error.errors);
            return 1;
        }
    };
    let runtime_directory = hearth_core::paths::resolve_runtime_directory(
        &root,
        loaded.catalog.runtime_directory.as_deref(),
    );
    run_manager(root, runtime_directory, loaded.catalog, None).await
}

/// The machine-global shared-services manager, rooted at `~/.hearth/shared`, whose catalog is
/// synthesized from `registry.json` rather than a `hearth.yaml`. Everything else (lock, token,
/// state.json, HTTP+SSE surface, `ensure`/`discover`) is identical to a project daemon.
async fn run_smp_subcommand(rest: &[String]) -> i32 {
    if !rest.is_empty() {
        eprintln!("usage: hearth smp");
        return 2;
    }
    let root = hearth_core::shared::shared_root();
    // Dev/test escape hatch: point smp at a local registry file instead of the pinned GitHub URL.
    let catalog_url = std::env::var("HEARTH_SHARED_CATALOG_URL").ok();
    let ctx = match hearth_core::shared::SharedContext::open(root.clone(), catalog_url) {
        Ok(ctx) => ctx,
        Err(error) => {
            eprintln!("hearth smp: cannot open {}: {}", root.display(), error);
            return 1;
        }
    };
    let catalog = match hearth_core::shared::synthesize::synthesize_catalog(
        &ctx.root,
        &ctx.registry.list(),
    ) {
        Ok(catalog) => catalog,
        Err(error) => {
            eprintln!("hearth smp: cannot synthesize catalog: {error}");
            return 1;
        }
    };
    let runtime_directory = ctx.runtime_directory();
    run_manager(root, runtime_directory, catalog, Some(ctx)).await
}

/// Re-invokes this binary with `args`, detached: its own process group, stdio discarded, never
/// waited on — the child outlives this process.
fn spawn_detached(args: &[&str], cwd: Option<&Path>) {
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("hearth"));
    let mut command = std::process::Command::new(exe);
    command
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let _ = command.spawn();
}

/// `hearth daemon --root <root>`, detached.
fn spawn_daemon(root: &Path) {
    spawn_detached(&["daemon", "--root", &root.to_string_lossy()], Some(root));
}

/// `hearth smp`, detached. smp has one fixed root, so the requested one is ignored.
fn spawn_smp(_root: &Path) {
    spawn_detached(&["smp"], None);
}

/// Runs an MCP server over stdio for `root`'s catalog. `require_confirm` stays at its safe default
/// (`true`); `tool_prefix` is fixed at `local_services_`, the name consumers' skill docs use.
async fn run_mcp_subcommand(root: PathBuf, catalog: hearth_core::catalog::ServiceCatalog) -> i32 {
    use rmcp::ServiceExt;
    let known_service_ids = catalog.services.iter().map(|s| s.id.clone()).collect();
    let options = hearth_cli::LocalctlOptions {
        catalog,
        spawn_daemon: Box::new(spawn_daemon),
    };
    let client = hearth_mcp::ManagerApiClient::new(root, options);
    let server = hearth_mcp::create_hearth_mcp_server(
        std::sync::Arc::new(client),
        hearth_mcp::CreateHearthMcpServerOptions {
            name: "hearth".to_string(),
            tool_prefix: "local_services_".to_string(),
            known_service_ids,
            ..Default::default()
        },
    );
    let running = match server.serve(rmcp::transport::stdio()).await {
        Ok(running) => running,
        Err(error) => {
            eprintln!("hearth mcp: failed to start: {error}");
            return 1;
        }
    };
    match running.waiting().await {
        Ok(_) => 0,
        Err(error) => {
            eprintln!("hearth mcp: {error}");
            1
        }
    }
}

/// `--help`/`--version` must work from ANY directory. `tui`, `shared`, and `update` also run
/// without a project catalog. Answering "how do I use this?" with "there is no config file
/// here" is a bad first experience for someone who just installed the binary.
fn print_help() {
    println!(
        "hearth — local dev services daemon, CLI, TUI, and MCP server

usage: hearth [--root <path>] <command> [options]

  status [target] [--json]              current state of one service, a group, or all
  start|stop|restart <target> [--wait]  lifecycle actions
  logs <service> [--tail N] [--follow]  read a service's log
  urls [target] [--json]                where each service can be reached (live URLs)
  operation get|watch <id> [--json]     inspect one operation
  doctor [--json]                       environment and catalog diagnostics
  cleanup                               remove stale runtime state
  manager ensure|status|stop|restart|reload
                                        daemon lifecycle (restart keeps services running)
  daemon --root <path>                  run the daemon in the foreground (spawned internally)
  smp                                   run the machine-global shared-services daemon
  shared list|installed|status          inspect the shared service registry and smp
  shared attach|detach|probe <id>       attach this project to a shared service (used by hearth.yaml `shared:`)
  shared install|start|stop|remove <id> manage a shared service instance
  tui                                   terminal UI for workspaces and shared services
  update [--check] [--json] [--force]   install the latest stable GitHub release
  mcp                                   serve the MCP tool surface over stdio
  mcp install [--name N] [--key K] <config-file>...
                                        register this binary in an MCP client config
  skill install --dest <path>           write the generic MCP skill doc

tui, shared, and update do not need a hearth.yaml in the current directory.
hearth --root <project> tui adopts that project when it has a catalog.
Every other command resolves a catalog from --root (default: cwd):
hearth.yaml, .yml, or .json."
    );
}

async fn run_cli(root: PathBuf, rest: Vec<String>) -> i32 {
    match rest.first().map(String::as_str) {
        Some("--help") | Some("-h") | Some("help") | None => {
            print_help();
            return 0;
        }
        Some("--version") | Some("-V") => {
            println!("hearth {}", env!("CARGO_PKG_VERSION"));
            return 0;
        }
        _ => {}
    }
    // `shared` manages the machine-global smp daemon — it deliberately does NOT require a
    // project `hearth.yaml` (the project root, when a command needs one for attach/probe identity,
    // is just the cwd).
    if rest.first().map(String::as_str) == Some("shared") {
        return hearth_cli::shared::run(
            &root,
            &rest[1..],
            &mut hearth_cli::Io {
                out: &mut |s: &str| println!("{s}"),
                err: &mut |s: &str| eprintln!("{s}"),
                confirm: None,
            },
            std::sync::Arc::new(spawn_smp),
        )
        .await;
    }
    // Self-update talks to GitHub, not to a project daemon, and must work in a directory
    // that has no hearth.yaml.
    if rest.first().map(String::as_str) == Some("update") {
        let mut out = |s: &str| println!("{s}");
        let mut err = |s: &str| eprintln!("{s}");
        return hearth_cli::update::run(
            &rest[1..],
            env!("CARGO_PKG_VERSION"),
            &mut hearth_cli::Io {
                out: &mut out,
                err: &mut err,
                confirm: None,
            },
        )
        .await;
    }
    // The terminal UI is an app shell: it does not need a hearth.yaml in the current directory.
    if rest.first().map(String::as_str) == Some("tui") {
        if rest.len() > 1 {
            eprintln!("usage: hearth tui");
            return 2;
        }
        return hearth_tui::run_shell(hearth_tui::ShellOptions {
            initial_root: root,
            spawn_daemon: std::sync::Arc::new(|path: &Path| spawn_daemon(path)),
            spawn_smp: std::sync::Arc::new(|path: &Path| spawn_smp(path)),
            refresh_interval: std::time::Duration::from_secs(2),
        })
        .await;
    }
    let loaded = match hearth_core::config_file::load_catalog(&root) {
        Ok(loaded) => loaded,
        Err(error) => {
            report_config_error(&root, &error.errors);
            return 1;
        }
    };
    // Only the bare `mcp` (serve over stdio) needs `hearth-mcp`; `hearth-cli` cannot depend on it.
    // `mcp install` is plain file editing and falls through to `hearth_cli::main`.
    if rest.first().map(String::as_str) == Some("mcp") && rest.len() == 1 {
        return run_mcp_subcommand(root, loaded.catalog).await;
    }
    let options = hearth_cli::LocalctlOptions {
        catalog: loaded.catalog,
        spawn_daemon: Box::new(spawn_daemon),
    };
    let mut out = |s: &str| println!("{s}");
    let mut err = |s: &str| eprintln!("{s}");
    // The port-conflict "kill the holder?" prompt, wired only on an interactive stdin. Scripts,
    // agents and piped calls get no prompt at all (`confirm: None`), so they never kill an unowned
    // process and a plain start still goes through.
    let mut confirm = |prompt: &str| -> bool {
        eprint!("{prompt}");
        let mut line = String::new();
        if std::io::stdin().read_line(&mut line).is_err() {
            return false;
        }
        matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes")
    };
    let interactive = {
        use std::io::IsTerminal;
        std::io::stdin().is_terminal()
    };
    let confirm: Option<&mut dyn FnMut(&str) -> bool> = if interactive {
        Some(&mut confirm)
    } else {
        None
    };
    let mut io = hearth_cli::Io {
        out: &mut out,
        err: &mut err,
        confirm,
    };
    hearth_cli::main(&options, &root, &rest, &mut io).await
}

fn main() {
    // Before the runtime exists, so no other thread can be reading the environment concurrently.
    // Needed because a GUI-spawned `hearth` (and the daemon it spawns) inherits launchd's bare PATH,
    // under which `docker`, `tailscale` and `bun` cannot be found — see
    // `hearth_core::env::with_known_tool_directories`.
    let path = hearth_core::env::with_known_tool_directories(
        std::env::var_os("PATH").as_deref(),
        std::env::var_os("HOME").as_deref(),
    );
    std::env::set_var("PATH", path);

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    let code = runtime.block_on(async {
        let argv: Vec<String> = std::env::args().skip(1).collect();
        // `smp` has its own fixed root, so it is dispatched before `--root` is parsed.
        if argv.first().map(String::as_str) == Some("smp") {
            return run_smp_subcommand(&argv[1..]).await;
        }
        // `hearth daemon --root <path>` is the spawned form; `--root <path> daemon` works too.
        let (argv, daemon) = match argv.first().map(String::as_str) {
            Some("daemon") => (argv[1..].to_vec(), true),
            _ => (argv, false),
        };
        let (root, rest) = match hearth_cli::parse_root(&argv) {
            Ok(parsed) => parsed,
            Err(error) => {
                eprintln!("{}", error.message);
                return error.exit_code;
            }
        };
        if daemon {
            run_daemon_subcommand(root, &rest).await
        } else if rest.first().map(String::as_str) == Some("daemon") {
            run_daemon_subcommand(root, &rest[1..]).await
        } else {
            run_cli(root, rest).await
        }
    });
    std::process::exit(code);
}
