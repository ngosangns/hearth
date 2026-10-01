//! `hearthd web` — a loopback browser GUI for the project daemons.
//!
//! The shape follows DeepSeek Harness's `dsh web`: one process binds `127.0.0.1`, prints a URL
//! carrying a fresh process token, and the browser trades that token for a cookie. The page is a
//! client. It never supervises processes itself.

mod http;
mod store;

use std::path::Path;
use std::sync::Arc;

pub use http::{serve, Running, ServeOptions};
pub use store::default_workspace_file;

/// Spawns `hearthd daemon` or `hearthd smp`. The binary supplies these so the GUI crate does not
/// know how the executable was launched.
pub type SpawnHook = Arc<dyn Fn(&Path) + Send + Sync>;

pub const DEFAULT_PORT: u16 = 4730;

const HELP: &str = "\
hearthd web — browser GUI for workspaces and shared services

usage: hearthd web [--port <port>] [--no-open] [--host 127.0.0.1]

  --port <port>   listen port (default 4730). 0 lets the OS pick a free port
  --no-open       do not open the GUI in the default browser
  --host <host>   must be 127.0.0.1. Binding all interfaces is rejected

The printed URL carries a process token. The browser trades it for a cookie and
then loads the GUI. The server binds loopback only. It talks to each project
daemon, and to the shared-services daemon.

Workspaces are stored in ~/Library/Application Support/HearthApp/workspaces.json.
A workspace stays untrusted until you confirm
it, and an untrusted workspace does not start a daemon.
";

#[derive(Debug)]
pub enum WebArgs {
    Help,
    Run { port: u16, open_browser: bool },
}

pub fn parse_web_args(args: &[String]) -> Result<WebArgs, String> {
    let mut port = DEFAULT_PORT;
    let mut open_browser = true;
    let mut seen_port = false;
    let mut seen_host = false;
    let mut seen_open = false;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--help" | "-h" => {
                if args.len() != 1 {
                    return Err("usage: hearthd web [--port <port>] [--no-open]".to_string());
                }
                return Ok(WebArgs::Help);
            }
            "--no-open" => {
                if seen_open {
                    return Err("duplicate flag: --no-open".to_string());
                }
                seen_open = true;
                open_browser = false;
            }
            "--port" => {
                if seen_port {
                    return Err("duplicate flag: --port".to_string());
                }
                seen_port = true;
                index += 1;
                let Some(value) = args.get(index) else {
                    return Err("--port requires a number".to_string());
                };
                match value.parse::<u16>() {
                    Ok(parsed) => port = parsed,
                    Err(_) => return Err("--port must be a number from 0 to 65535".to_string()),
                }
            }
            "--host" => {
                if seen_host {
                    return Err("duplicate flag: --host".to_string());
                }
                seen_host = true;
                index += 1;
                let Some(value) = args.get(index) else {
                    return Err("--host requires a value".to_string());
                };
                if value == "0.0.0.0" {
                    return Err("--host 0.0.0.0 is not supported. It would expose the GUI, and through it every project daemon, to the network.".to_string());
                }
                if value != "127.0.0.1" {
                    return Err("--host must be 127.0.0.1".to_string());
                }
            }
            other => return Err(format!("unknown argument: {other}")),
        }
        index += 1;
    }
    Ok(WebArgs::Run { port, open_browser })
}

pub async fn run(
    root: &Path,
    args: &[String],
    spawn_daemon: SpawnHook,
    spawn_smp: SpawnHook,
) -> i32 {
    let parsed = match parse_web_args(args) {
        Ok(parsed) => parsed,
        Err(message) => {
            eprintln!("hearth web: {message}");
            return 2;
        }
    };
    let WebArgs::Run { port, open_browser } = parsed else {
        println!("{HELP}");
        return 0;
    };
    let running = match serve(ServeOptions {
        port,
        workspace_file: default_workspace_file(),
        adopt_root: Some(root.to_path_buf()),
        spawn_daemon,
        spawn_smp,
    })
    .await
    {
        Ok(running) => running,
        Err(error) => {
            eprintln!("hearth web: {error}");
            return 1;
        }
    };
    println!("hearth web: {}", running.url);
    let _ = std::io::Write::flush(&mut std::io::stdout());
    if open_browser && ssh_session() {
        eprintln!("hearth web: SSH session, not opening a browser.");
    } else if open_browser {
        open_in_browser(&running.url);
    }
    let _ = tokio::signal::ctrl_c().await;
    running.shutdown().await;
    0
}

fn ssh_session() -> bool {
    std::env::var_os("SSH_CONNECTION").is_some() || std::env::var_os("SSH_TTY").is_some()
}

fn open_in_browser(url: &str) {
    let program = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    if std::process::Command::new(program)
        .arg(url)
        .spawn()
        .is_err()
    {
        eprintln!("hearth web: could not open a browser. Open the URL above.");
    }
}
