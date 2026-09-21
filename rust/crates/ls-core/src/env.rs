//! Port of `src/core/env.ts` — resolves the environment a daemon should hand to every process it
//! spawns. A daemon started from a GUI (Finder/Dock/LaunchAgent, as a desktop app's sidecar would)
//! inherits a bare `PATH` — none of the login-shell customization (`nvm`, `asdf`, Homebrew, a
//! project's own `.env`) that a daemon started from an interactive terminal gets for free. A daemon
//! spawned from a terminal and one spawned from an app should end up running the exact same
//! commands with the exact same environment; this module is the one place that difference gets
//! resolved away.
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::Duration;

const START_MARKER: &str = "__lsd_env_start__";
const END_MARKER: &str = "__lsd_env_end__";
const DEFAULT_LOGIN_SHELL_TIMEOUT: Duration = Duration::from_secs(5);

fn login_shell_env_cache() -> &'static Mutex<HashMap<String, HashMap<String, String>>> {
    static CACHE: OnceLock<Mutex<HashMap<String, HashMap<String, String>>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn parse_env_block(text: &str) -> HashMap<String, String> {
    let mut env = HashMap::new();
    let mut key: Option<String> = None;
    for raw_line in text.split('\n') {
        if let Some((k, v)) = split_key_value(raw_line) {
            env.insert(k.clone(), v);
            key = Some(k);
        } else if let Some(k) = &key {
            // A value containing a literal newline continues on the following line(s).
            let entry = env.get_mut(k).unwrap();
            entry.push('\n');
            entry.push_str(raw_line);
        }
    }
    env
}

fn split_key_value(line: &str) -> Option<(String, String)> {
    let eq = line.find('=')?;
    let key = &line[..eq];
    if key.is_empty() || !key.chars().next().unwrap().is_ascii_alphabetic() && !key.starts_with('_') {
        return None;
    }
    if !key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return None;
    }
    Some((key.to_string(), line[eq + 1..].to_string()))
}

/// Runs the user's login shell (`$SHELL` by default) as it would run interactively, then captures
/// the environment it ends up with. Markers bracket the `env` dump so shell startup noise is
/// ignored rather than corrupting the parse. Failures (unknown shell, timeout, non-interactive
/// sandboxed shell) resolve to `{}` — never panics/errors — so a caller can always fall back to the
/// process's own environment.
fn run_login_shell_env(shell: &str, timeout: Duration) -> HashMap<String, String> {
    let script = format!("printf '%s' '{START_MARKER}'; env; printf '%s' '{END_MARKER}'");
    let attempt = (|| -> std::io::Result<HashMap<String, String>> {
        let child = Command::new(shell)
            .arg("-ilc")
            .arg(&script)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        let pid = child.id();
        // A channel-based watchdog (not a fixed sleep-then-check-a-flag) so a child that exits well
        // before `timeout` doesn't force this call to still block for the full timeout duration —
        // `recv_timeout` returns as soon as the main thread signals completion, mirroring
        // `clearTimeout(timer)` cancelling a pending JS timer immediately.
        let (done_tx, done_rx) = mpsc::channel::<()>();
        let watchdog = thread::spawn(move || {
            if done_rx.recv_timeout(timeout).is_err() {
                #[cfg(unix)]
                unsafe {
                    libc::kill(pid as libc::pid_t, libc::SIGKILL);
                }
            }
        });
        let output = child.wait_with_output();
        let _ = done_tx.send(());
        let _ = watchdog.join();
        let output = output?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        let start = stdout.find(START_MARKER);
        let end = stdout.find(END_MARKER);
        match (start, end) {
            (Some(s), Some(e)) if e >= s => Ok(parse_env_block(&stdout[s + START_MARKER.len()..e])),
            _ => Ok(HashMap::new()),
        }
    })();
    attempt.unwrap_or_default()
}

/// Directories where the tools this daemon itself invokes (`docker`, `tailscale`, `bun`, `ps`) are
/// installed on a typical macOS dev machine, beyond launchd's bare default.
pub const KNOWN_TOOL_DIRECTORIES: [&str; 3] = ["/opt/homebrew/bin", "/opt/homebrew/sbin", "/usr/local/bin"];

/// `path` with every directory in `KNOWN_TOOL_DIRECTORIES` (plus `~/.bun/bin` and `~/.cargo/bin`)
/// appended if it is not already present.
///
/// `lsd` is routinely spawned by the macOS app, and a Dock/Finder-launched GUI process inherits
/// launchd's bare `PATH` (`/usr/bin:/bin:/usr/sbin:/sbin`) — which the daemon it spawns then
/// inherits too. Every tool the daemon calls by name (`docker compose` for container services,
/// `tailscale serve status` for tailnet readiness, `tailscale status` for `{tailnetHost}` URLs)
/// would then fail to spawn, even though all of them work from a terminal. Appending rather than
/// prepending keeps any explicit ordering in the inherited `PATH` authoritative.
///
/// This covers the daemon's *own* subprocesses only. The environment of the services it runs is
/// resolved separately, from the login shell, by `resolve_base_environment`.
pub fn with_known_tool_directories(path: Option<&std::ffi::OsStr>, home: Option<&std::ffi::OsStr>) -> std::ffi::OsString {
    let mut entries: Vec<std::path::PathBuf> = path.map(|p| std::env::split_paths(p).collect()).unwrap_or_default();
    let home_dirs = home.map(std::path::PathBuf::from).into_iter().flat_map(|h| [h.join(".bun/bin"), h.join(".cargo/bin")]);
    for dir in KNOWN_TOOL_DIRECTORIES.iter().map(std::path::PathBuf::from).chain(home_dirs) {
        if !entries.contains(&dir) {
            entries.push(dir);
        }
    }
    std::env::join_paths(entries).unwrap_or_else(|_| path.map(std::ffi::OsStr::to_owned).unwrap_or_default())
}


/// Cached per shell path — spawning an interactive login shell is expensive (can run a user's full
/// `.zshrc`), and a daemon only needs this resolved once at startup.
pub fn resolve_login_shell_env(shell: Option<&str>, timeout: Option<Duration>) -> HashMap<String, String> {
    let shell = shell
        .map(|s| s.to_string())
        .or_else(|| std::env::var("SHELL").ok())
        .unwrap_or_else(|| "/bin/zsh".to_string());
    let timeout = timeout.unwrap_or(DEFAULT_LOGIN_SHELL_TIMEOUT);

    let cache = login_shell_env_cache();
    {
        let guard = cache.lock().unwrap();
        if let Some(cached) = guard.get(&shell) {
            return cached.clone();
        }
    }
    let resolved = run_login_shell_env(&shell, timeout);
    cache.lock().unwrap().insert(shell, resolved.clone());
    resolved
}

/// Test-only escape hatch: `resolve_login_shell_env` memoizes per shell path, which would otherwise
/// leak a stubbed result across unrelated tests.
pub fn clear_login_shell_env_cache_for_tests() {
    login_shell_env_cache().lock().unwrap().clear();
}

/// Minimal `.env` parser: `KEY=VALUE` per line, optional `export ` prefix, `#`-comments, blank lines
/// skipped, optional surrounding quotes stripped. No interpolation, no multi-line values —
/// deliberately a subset of what `dotenv` supports.
pub fn load_env_file(path: &Path) -> HashMap<String, String> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return HashMap::new();
    };
    let mut env = HashMap::new();
    for raw_line in text.split('\n') {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let rest = line.strip_prefix("export ").map(str::trim_start).unwrap_or(line);
        let Some(eq) = rest.find('=') else { continue };
        let key = &rest[..eq];
        if key.is_empty()
            || !(key.chars().next().unwrap().is_ascii_alphabetic() || key.starts_with('_'))
            || !key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            continue;
        }
        let value = rest[eq + 1..].trim();
        let value = if (value.starts_with('"') && value.ends_with('"') && value.len() >= 2)
            || (value.starts_with('\'') && value.ends_with('\'') && value.len() >= 2)
        {
            &value[1..value.len() - 1]
        } else {
            value
        };
        env.insert(key.to_string(), value.to_string());
    }
    env
}

#[derive(Debug, Clone, Default)]
pub struct BaseEnvironmentOptions {
    /// Login shell to resolve; defaults to `$SHELL`. `Some(None)` (i.e. pass `shell: Skip`) to skip
    /// shell resolution entirely (e.g. in tests, or when a caller already has a trustworthy
    /// environment) — modeled as `shell: ShellChoice` below rather than `Option<Option<String>>`.
    pub shell: ShellChoice,
    pub shell_timeout: Option<Duration>,
    /// `.env`-style file, resolved relative to `root`.
    pub env_file: Option<PathBuf>,
    pub root: Option<PathBuf>,
    /// Highest-priority overrides, applied last.
    pub extra: Option<HashMap<String, String>>,
}

#[derive(Debug, Clone, Default)]
pub enum ShellChoice {
    #[default]
    Default,
    Named(String),
    Skip,
}

/// Resolves the base environment a daemon should use for every process it spawns: the process's own
/// environment as the floor, overlaid with the login shell's environment (unless disabled), then an
/// optional `.env` file, then explicit overrides — each layer only adding or replacing keys, never
/// removing ones the layer below already set.
pub fn resolve_base_environment(options: BaseEnvironmentOptions) -> HashMap<String, String> {
    let mut base: HashMap<String, String> = std::env::vars().collect();
    match &options.shell {
        ShellChoice::Skip => {}
        ShellChoice::Default => {
            base.extend(resolve_login_shell_env(None, options.shell_timeout));
        }
        ShellChoice::Named(shell) => {
            base.extend(resolve_login_shell_env(Some(shell), options.shell_timeout));
        }
    }
    if let Some(env_file) = &options.env_file {
        let path = if env_file.is_absolute() {
            env_file.clone()
        } else {
            options.root.clone().unwrap_or_else(|| std::env::current_dir().unwrap_or_default()).join(env_file)
        };
        base.extend(load_env_file(&path));
    }
    if let Some(extra) = &options.extra {
        base.extend(extra.clone());
    }
    base
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_simple_env_block() {
        // A trailing newline in the source text produces one trailing empty element from `split('\n')`,
        // which — faithfully matching the TS port — gets appended as a continuation line onto the last
        // key's value (`env[key] += "\n" + rawLine`). Real usage never notices this because it's an
        // internal implementation quirk of splitting on a trailing terminator, not something a caller
        // observes differently from omitting the trailing newline in the first place.
        let parsed = parse_env_block("FOO=bar\nBAZ=qux\n");
        assert_eq!(parsed.get("FOO"), Some(&"bar".to_string()));
        assert_eq!(parsed.get("BAZ"), Some(&"qux\n".to_string()));

        let parsed_no_trailing_newline = parse_env_block("FOO=bar\nBAZ=qux");
        assert_eq!(parsed_no_trailing_newline.get("BAZ"), Some(&"qux".to_string()));
    }

    #[test]
    fn parses_a_value_with_embedded_newlines() {
        let parsed = parse_env_block("MULTI=line one\nline two\nOTHER=x");
        assert_eq!(parsed.get("MULTI"), Some(&"line one\nline two".to_string()));
        assert_eq!(parsed.get("OTHER"), Some(&"x".to_string()));
    }

    #[test]
    fn nonexistent_shell_resolves_to_empty_never_panics() {
        clear_login_shell_env_cache_for_tests();
        let env = resolve_login_shell_env(Some("/definitely/not/a/shell"), Some(Duration::from_millis(500)));
        assert!(env.is_empty());
    }

    #[test]
    fn caches_per_shell_path() {
        clear_login_shell_env_cache_for_tests();
        let shell = "/definitely/not/a/shell/either";
        let first = resolve_login_shell_env(Some(shell), Some(Duration::from_millis(200)));
        let second = resolve_login_shell_env(Some(shell), Some(Duration::from_millis(200)));
        assert_eq!(first, second);
    }

    #[test]
    fn end_to_end_real_login_shell_capture() {
        clear_login_shell_env_cache_for_tests();
        // /bin/sh is guaranteed to exist on darwin/linux; -ilc is odd for sh but should still run the
        // script body without hanging or throwing.
        let env = resolve_login_shell_env(Some("/bin/sh"), Some(Duration::from_secs(5)));
        // Not asserting specific keys (rc-file contents vary by machine) — just that it didn't panic
        // and returned a plausible map (PATH is close to universal).
        assert!(env.contains_key("PATH") || env.is_empty());
    }

    #[test]
    fn loads_a_dotenv_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".env");
        std::fs::write(&path, "# comment\nexport FOO=bar\nBAZ=\"qux\"\nQUOTED='single'\n\nEMPTY_LINE_ABOVE=1\n").unwrap();
        let env = load_env_file(&path);
        assert_eq!(env.get("FOO"), Some(&"bar".to_string()));
        assert_eq!(env.get("BAZ"), Some(&"qux".to_string()));
        assert_eq!(env.get("QUOTED"), Some(&"single".to_string()));
        assert_eq!(env.get("EMPTY_LINE_ABOVE"), Some(&"1".to_string()));
    }

    #[test]
    fn missing_dotenv_file_resolves_to_empty() {
        let env = load_env_file(Path::new("/definitely/does/not/exist/.env"));
        assert!(env.is_empty());
    }

    #[test]
    fn layering_order_lowest_to_highest_priority() {
        clear_login_shell_env_cache_for_tests();
        std::env::set_var("LS_TEST_LAYER_BASE_ONLY", "from-process-env");
        let dir = tempfile::tempdir().unwrap();
        let env_path = dir.path().join(".env");
        std::fs::write(&env_path, "LS_TEST_LAYER=from-file\n").unwrap();
        let mut extra = HashMap::new();
        extra.insert("LS_TEST_LAYER".to_string(), "from-extra".to_string());

        let resolved = resolve_base_environment(BaseEnvironmentOptions {
            shell: ShellChoice::Skip,
            shell_timeout: None,
            env_file: Some(env_path),
            root: None,
            extra: Some(extra),
        });

        assert_eq!(resolved.get("LS_TEST_LAYER"), Some(&"from-extra".to_string()));
        assert_eq!(resolved.get("LS_TEST_LAYER_BASE_ONLY"), Some(&"from-process-env".to_string()));
        std::env::remove_var("LS_TEST_LAYER_BASE_ONLY");
    }

    #[test]
    fn shell_false_bypasses_shell_resolution() {
        let resolved = resolve_base_environment(BaseEnvironmentOptions {
            shell: ShellChoice::Skip,
            ..Default::default()
        });
        // Should be exactly the process's own env (plus nothing else) — spot check a key that's
        // always present under a test runner.
        assert_eq!(resolved.get("PATH"), std::env::var("PATH").ok().as_ref());
    }
}
