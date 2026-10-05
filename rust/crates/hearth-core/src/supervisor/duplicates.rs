//! Duplicate-instance reaping for daemon and service restarts.
//!
//! A restart replaces one instance. Other processes that are the same instance — another
//! `hearth daemon --root <this project>` (or `hearth smp` for the shared root), or another
//! process running a service's command from that service's directory — are stopped first.
//! Only those pids are signalled, never their process group: a daemon's services live in
//! their own groups and must keep running, and an unrelated job must not be pulled in by a
//! shared pgid. `lstart` is checked again at signal time so a recycled pid is left alone.
//! If the process table cannot be read, the restart fails instead of starting a second copy.

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use regex::Regex;
use std::sync::OnceLock;
use tokio::io::AsyncReadExt;
use tokio::process::Command;

use super::fingerprint::normalize_observed_command_fingerprint;
use super::types::{CommandMatch, ProcessSignal};

const PS_TIMEOUT: Duration = Duration::from_secs(5);
const TERM_GRACE: Duration = Duration::from_secs(5);
const KILL_GRACE: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveProcess {
    pub pid: i64,
    pub start_identity: String,
    pub command: String,
}

/// Which daemon a restart is replacing. A project restart must not touch `hearth smp`, and an
/// smp restart must not touch a project daemon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DaemonRole {
    Project(PathBuf),
    Smp(PathBuf),
}

pub fn daemon_role(root: &Path) -> DaemonRole {
    let shared = crate::shared::shared_root();
    if paths_equal(root, &shared) {
        DaemonRole::Smp(shared)
    } else {
        DaemonRole::Project(root.to_path_buf())
    }
}

fn ps_command_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^(\d+)\s+(.{24})\s+(.+)$").unwrap())
}

/// Parses `ps -axww -o pid=,lstart=,command=`. A line that does not match is skipped — one
/// odd row must not hide every other process — but the caller treats a failed `ps` as unknown.
pub fn parse_ps_command_rows(stdout: &str) -> Vec<LiveProcess> {
    stdout
        .split('\n')
        .filter_map(|line| {
            let trimmed = line.trim_start();
            let caps = ps_command_re().captures(trimmed)?;
            let pid = caps[1].parse().ok()?;
            Some(LiveProcess {
                pid,
                start_identity: caps[2].trim().to_string(),
                command: caps[3].trim().to_string(),
            })
        })
        .collect()
}

/// `lsof -Fn` cwd records, keyed by pid. The first cwd for a pid wins.
pub fn parse_lsof_cwd_map(stdout: &str) -> HashMap<i64, PathBuf> {
    let mut map = HashMap::new();
    let mut pid = None;
    let mut at_cwd = false;
    for line in stdout.lines() {
        if let Some(rest) = line.strip_prefix('p') {
            if let Ok(parsed) = rest.parse::<i64>() {
                pid = Some(parsed);
                at_cwd = false;
                continue;
            }
        }
        if let Some(fd) = line.strip_prefix('f') {
            at_cwd = fd == "cwd";
            continue;
        }
        if at_cwd {
            if let (Some(pid), Some(path)) = (pid, line.strip_prefix('n')) {
                map.entry(pid).or_insert_with(|| PathBuf::from(path));
            }
            at_cwd = false;
        }
    }
    map
}

pub fn select_duplicate_daemons(
    processes: &[LiveProcess],
    role: &DaemonRole,
    self_pid: i64,
) -> Vec<LiveProcess> {
    processes
        .iter()
        .filter(|process| {
            process.pid > 1 && process.pid != self_pid && is_duplicate_daemon(process, role)
        })
        .cloned()
        .collect()
}

fn is_duplicate_daemon(process: &LiveProcess, role: &DaemonRole) -> bool {
    match role {
        DaemonRole::Project(root) => project_daemon_root(&process.command)
            .is_some_and(|candidate| paths_equal(Path::new(candidate), root)),
        DaemonRole::Smp(root) => {
            is_smp_daemon(&process.command)
                || project_daemon_root(&process.command)
                    .is_some_and(|candidate| paths_equal(Path::new(candidate), root))
        }
    }
}

pub fn command_fingerprint_matches(command: &str, fingerprints: &[String]) -> bool {
    let observed = normalize_observed_command_fingerprint(command);
    fingerprints
        .iter()
        .any(|fingerprint| fingerprint == &observed)
}

/// First token of a `ps` command line. A command with no spaces is itself.
pub fn command_argv0(command: &str) -> Option<&str> {
    let command = command.trim();
    if command.is_empty() {
        return None;
    }
    Some(
        command
            .split_once(char::is_whitespace)
            .map(|(exe, _)| exe)
            .unwrap_or(command),
    )
}

/// Interpreters and the hearth binary run many different commands. Matching them by argv0
/// would reap a daemon, a TUI, or another script in the same directory.
fn dedicated_server_binary(exe: &str) -> bool {
    let name = file_name(exe);
    if is_hearth_binary_name(name) || name == "hearthd" {
        return false;
    }
    !matches!(
        name,
        "node"
            | "nodejs"
            | "python"
            | "python3"
            | "java"
            | "ruby"
            | "perl"
            | "sh"
            | "bash"
            | "zsh"
            | "bun"
            | "deno"
    )
}

/// True when `command`'s executable is one of `executables`. Both sides must be absolute
/// paths of a dedicated server binary. A bare `node`, or `hearth`, does not count.
pub fn executable_matches(command: &str, executables: &[String]) -> bool {
    if executables.is_empty() {
        return false;
    }
    let Some(exe) = command_argv0(command) else {
        return false;
    };
    if !exe.starts_with('/') || !dedicated_server_binary(exe) {
        return false;
    }
    executables.iter().any(|expected| {
        expected.starts_with('/')
            && dedicated_server_binary(expected)
            && paths_equal(Path::new(exe), Path::new(expected))
    })
}

pub fn paths_equal(left: &Path, right: &Path) -> bool {
    if normalize_lexical(left) == normalize_lexical(right) {
        return true;
    }
    match (left.canonicalize(), right.canonicalize()) {
        (Ok(left), Ok(right)) => left == right,
        _ => false,
    }
}

fn normalize_lexical(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

fn file_name(token: &str) -> &str {
    token.rsplit(['/', '\\']).next().unwrap_or(token)
}

/// `hearth` or the versioned install name `hearth-<digits>.<digits>...`. `hearthd` and
/// `hearth-mcp` are different programs.
fn is_hearth_binary_name(name: &str) -> bool {
    if name == "hearth" {
        return true;
    }
    let Some(rest) = name.strip_prefix("hearth-") else {
        return false;
    };
    let mut any = false;
    for part in rest.split('.') {
        if part.is_empty() || !part.chars().all(|c| c.is_ascii_digit()) {
            return false;
        }
        any = true;
    }
    any
}

fn split_exe(command: &str) -> Option<(&str, &str)> {
    let command = command.trim();
    let (exe, rest) = command.split_once(char::is_whitespace)?;
    if exe.is_empty() {
        return None;
    }
    Some((exe, rest.trim_start()))
}

fn strip_token<'a>(input: &'a str, token: &str) -> Option<&'a str> {
    let input = input.trim_start();
    let rest = input.strip_prefix(token)?;
    if rest.is_empty() {
        return Some("");
    }
    if rest.starts_with(|c: char| c.is_whitespace()) {
        return Some(rest.trim_start());
    }
    None
}

/// The `--root` argument of a hearth daemon, preserving spaces inside the path.
/// Accepts `hearth daemon --root <path>` and `hearth --root <path> daemon`.
fn project_daemon_root(command: &str) -> Option<&str> {
    let (exe, rest) = split_exe(command)?;
    if !is_hearth_binary_name(file_name(exe)) {
        return None;
    }
    if let Some(after_daemon) = strip_token(rest, "daemon") {
        let path = strip_token(after_daemon, "--root")?;
        return (!path.is_empty()).then_some(path);
    }
    let after_root = strip_token(rest, "--root")?.trim_end();
    let path = after_root.strip_suffix("daemon")?;
    if path.is_empty() || !path.ends_with(|c: char| c.is_whitespace()) {
        return None;
    }
    let path = path.trim();
    (!path.is_empty()).then_some(path)
}

fn is_smp_daemon(command: &str) -> bool {
    let Some((exe, rest)) = split_exe(command) else {
        return false;
    };
    is_hearth_binary_name(file_name(exe)) && rest == "smp"
}

/// True when `pid` is a live process. A zombie is not live: `kill(pid, 0)` still succeeds for
/// one, and init reaps it, so it must not block the new daemon from starting.
pub async fn pid_is_live(pid: i64) -> Result<bool, String> {
    if pid <= 0 {
        return Ok(false);
    }
    let (code, stat) = run_ps(&["ps", "-o", "stat=", "-p", &pid.to_string()]).await?;
    if code != 0 {
        return Ok(false);
    }
    let stat = stat.trim();
    Ok(!stat.is_empty() && !stat.starts_with('Z'))
}

/// Stops every other daemon process for `root`. Signals the pid only.
pub async fn reap_duplicate_daemons(root: &Path) -> Result<(), String> {
    let role = daemon_role(root);
    let self_pid = i64::from(std::process::id());
    let processes = list_live_processes().await?;
    let targets = select_duplicate_daemons(&processes, &role, self_pid);
    reap_checked(&role, targets).await
}

async fn reap_checked(role: &DaemonRole, mut targets: Vec<LiveProcess>) -> Result<(), String> {
    if targets.is_empty() {
        return Ok(());
    }
    signal_still_matching(role, &targets, ProcessSignal::Sigterm).await?;
    wait_until_gone(role, &mut targets, TERM_GRACE).await?;
    if targets.is_empty() {
        return Ok(());
    }
    signal_still_matching(role, &targets, ProcessSignal::Sigkill).await?;
    wait_until_gone(role, &mut targets, KILL_GRACE).await?;
    if targets.is_empty() {
        return Ok(());
    }
    let pids: Vec<String> = targets
        .iter()
        .map(|process| process.pid.to_string())
        .collect();
    Err(format!(
        "duplicate hearth daemon still running (pid {})",
        pids.join(", ")
    ))
}

async fn signal_still_matching(
    role: &DaemonRole,
    targets: &[LiveProcess],
    signal: ProcessSignal,
) -> Result<(), String> {
    for target in targets {
        if still_duplicate(role, target).await? {
            send_signal(target.pid, signal);
        }
    }
    Ok(())
}

async fn wait_until_gone(
    role: &DaemonRole,
    targets: &mut Vec<LiveProcess>,
    grace: Duration,
) -> Result<(), String> {
    let deadline = tokio::time::Instant::now() + grace;
    loop {
        let mut still = Vec::new();
        for target in targets.iter() {
            if still_duplicate(role, target).await? {
                still.push(target.clone());
            }
        }
        *targets = still;
        if targets.is_empty() || tokio::time::Instant::now() >= deadline {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// The same process we selected: same pid, same `lstart`, same daemon command. A zombie, a
/// reused pid, or a `ps` row that no longer matches is not signalled and does not fail the reap.
async fn still_duplicate(role: &DaemonRole, target: &LiveProcess) -> Result<bool, String> {
    let pid = target.pid.to_string();
    let (code, stdout) = run_ps(&["ps", "-ww", "-o", "pid=,lstart=,command=", "-p", &pid]).await?;
    if code != 0 {
        return Ok(false);
    }
    let Some(row) = parse_ps_command_rows(&stdout)
        .into_iter()
        .find(|row| row.pid == target.pid)
    else {
        return Ok(false);
    };
    if row.start_identity != target.start_identity || !is_duplicate_daemon(&row, role) {
        return Ok(false);
    }
    let (stat_code, stat) = run_ps(&["ps", "-o", "stat=", "-p", &pid]).await?;
    if stat_code != 0 {
        return Ok(false);
    }
    let stat = stat.trim();
    Ok(!stat.is_empty() && !stat.starts_with('Z'))
}

async fn list_live_processes() -> Result<Vec<LiveProcess>, String> {
    let (code, stdout) = run_ps(&["ps", "-axww", "-o", "pid=,lstart=,command="]).await?;
    if code != 0 {
        return Err(
            "could not list processes; refusing to start beside an unchecked duplicate daemon"
                .to_string(),
        );
    }
    Ok(parse_ps_command_rows(&stdout))
}

/// Host processes in `project_root/cwd` that are this service: the observed command hashes to
/// one of `fingerprints`, or its absolute executable is one of `executables` (a title rewrite
/// keeps the binary and replaces the arguments). `None` means the table could not be read, or
/// a match was still alive but its cwd could not be checked — the caller must not start
/// another copy.
pub async fn matching_service_processes(
    project_root: &Path,
    fingerprints: &[String],
    executables: &[String],
    cwd: &str,
) -> Option<Vec<CommandMatch>> {
    if fingerprints.is_empty() && executables.is_empty() {
        return Some(Vec::new());
    }
    let (code, stdout) = run_ps(&["ps", "-axww", "-o", "pid=,lstart=,command="])
        .await
        .ok()?;
    if code != 0 {
        return None;
    }
    let self_pid = i64::from(std::process::id());
    let candidates: Vec<LiveProcess> = parse_ps_command_rows(&stdout)
        .into_iter()
        .filter(|process| {
            process.pid > 1
                && process.pid != self_pid
                && (command_fingerprint_matches(&process.command, fingerprints)
                    || executable_matches(&process.command, executables))
        })
        .collect();
    if candidates.is_empty() {
        return Some(Vec::new());
    }
    let spec = candidates
        .iter()
        .map(|process| process.pid.to_string())
        .collect::<Vec<_>>()
        .join(",");
    let (lsof_code, lsof_out) = run_ps(&["lsof", "-nP", "-d", "cwd", "-a", "-p", &spec, "-Fn"])
        .await
        .ok()?;
    if lsof_code == -1 {
        return None;
    }
    let cwds = parse_lsof_cwd_map(&lsof_out);
    let expected = project_root.join(cwd);
    let mut matches = Vec::new();
    for candidate in candidates {
        match cwds.get(&candidate.pid) {
            Some(actual) if paths_equal(actual, &expected) => matches.push(CommandMatch {
                pid: candidate.pid,
                start_identity: candidate.start_identity,
            }),
            Some(_) => {}
            None => {
                // Gone between the two probes is not a duplicate. Still alive with no cwd is
                // unknown — killing it would be a guess, and ignoring it could leave a second copy.
                let (code, stdout) = run_ps(&[
                    "ps",
                    "-ww",
                    "-o",
                    "pid=,lstart=,command=",
                    "-p",
                    &candidate.pid.to_string(),
                ])
                .await
                .ok()?;
                if code == 0
                    && parse_ps_command_rows(&stdout).iter().any(|row| {
                        row.pid == candidate.pid && row.start_identity == candidate.start_identity
                    })
                {
                    return None;
                }
            }
        }
    }
    Some(matches)
}

fn send_signal(pid: i64, signal: ProcessSignal) {
    if pid <= 1 {
        return;
    }
    #[cfg(unix)]
    {
        let nix_signal = match signal {
            ProcessSignal::Sigterm => nix::sys::signal::Signal::SIGTERM,
            ProcessSignal::Sigkill => nix::sys::signal::Signal::SIGKILL,
        };
        let _ = nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid as i32), nix_signal);
    }
    #[cfg(not(unix))]
    {
        let _ = (pid, signal);
    }
}

/// `(exit code, stdout)`. `Ok((-1, _))` is not used: a spawn failure or a timeout is `Err`,
/// because the caller must not read that as "no such process".
async fn run_ps(argv: &[&str]) -> Result<(i32, String), String> {
    if argv.is_empty() {
        return Err("could not list processes".to_string());
    }
    let mut cmd = Command::new(argv[0]);
    cmd.args(&argv[1..])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    #[cfg(unix)]
    cmd.process_group(0);
    let mut child = cmd
        .spawn()
        .map_err(|_| "could not list processes".to_string())?;
    let stdout = child.stdout.take();
    let read = async move {
        let mut buf = Vec::new();
        if let Some(mut stdout) = stdout {
            let _ = stdout.read_to_end(&mut buf).await;
        }
        buf
    };
    match tokio::time::timeout(PS_TIMEOUT, async { tokio::join!(read, child.wait()) }).await {
        Ok((out, Ok(status))) => Ok((
            status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out).into_owned(),
        )),
        Ok((_, Err(_))) => Err("could not list processes".to_string()),
        Err(_) => {
            if let Some(pid) = child.id() {
                send_signal(pid as i64, ProcessSignal::Sigkill);
            }
            let _ = child.kill().await;
            Err("could not list processes".to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(pid: i64, command: &str) -> LiveProcess {
        LiveProcess {
            pid,
            start_identity: "Mon Oct  5 09:00:44 2026".to_string(),
            command: command.to_string(),
        }
    }

    fn project() -> DaemonRole {
        DaemonRole::Project(PathBuf::from("/Users/x/proj"))
    }

    #[test]
    fn project_daemon_matches_either_argument_order_and_a_path_with_spaces() {
        let rows = [
            row(
                10,
                "/Users/x/.local/share/hearth/bin/hearth-0.18.3 daemon --root /Users/x/proj",
            ),
            row(11, "/Users/x/.local/bin/hearth --root /Users/x/proj daemon"),
            row(
                12,
                "/Users/x/.local/bin/hearth daemon --root /Users/x/my proj",
            ),
            row(
                13,
                "/Users/x/.local/bin/hearth --root /Users/x/my proj daemon",
            ),
        ];
        let role = project();
        let spaced = DaemonRole::Project(PathBuf::from("/Users/x/my proj"));
        assert_eq!(select_duplicate_daemons(&rows, &role, 0).len(), 2);
        assert_eq!(select_duplicate_daemons(&rows, &spaced, 0).len(), 2);
    }

    #[test]
    fn a_longer_path_or_another_hearth_command_is_not_this_daemon() {
        let rows = [
            row(10, "/Users/x/.local/bin/hearth daemon --root /Users/x/proj-other"),
            row(11, "/Users/x/.local/bin/hearth daemon --root /Users/x/proj/nested"),
            row(12, "/Users/x/.local/bin/hearth --root /Users/x/proj manager restart"),
            row(13, "/Users/x/.local/bin/hearth tui"),
            row(14, "/Users/x/.local/bin/hearth smp"),
            row(15, "/Users/x/.local/bin/hearth shared attach postgres@16"),
            row(16, "/usr/sbin/distnoted daemon"),
            row(17, "/System/Library/PrivateFrameworks/SkyLight.framework/Resources/WindowServer -daemon"),
            row(18, "/usr/bin/hearthd daemon --root /Users/x/proj"),
            row(19, "/usr/bin/hearth-mcp daemon --root /Users/x/proj"),
        ];
        assert!(select_duplicate_daemons(&rows, &project(), 0).is_empty());
    }

    #[test]
    fn the_restarting_process_and_pid_one_are_never_selected() {
        let rows = [
            row(1, "/sbin/launchd"),
            row(50, "/Users/x/.local/bin/hearth daemon --root /Users/x/proj"),
        ];
        assert!(select_duplicate_daemons(&rows, &project(), 50).is_empty());
    }

    #[test]
    fn smp_restart_selects_hearth_smp_and_a_daemon_rooted_at_the_shared_directory() {
        let shared = PathBuf::from("/Users/x/.hearth/shared");
        let rows = [
            row(10, "/Users/x/.local/bin/hearth smp"),
            row(11, "/Users/x/.local/share/hearth/bin/hearth-0.18.3 smp"),
            row(
                12,
                &format!(
                    "/Users/x/.local/bin/hearth daemon --root {}",
                    shared.display()
                ),
            ),
            row(13, "/Users/x/.local/bin/hearth daemon --root /Users/x/proj"),
            row(14, "/Users/x/.local/bin/hearth shared list"),
        ];
        let selected = select_duplicate_daemons(&rows, &DaemonRole::Smp(shared), 0);
        let pids: Vec<i64> = selected.iter().map(|process| process.pid).collect();
        assert_eq!(pids, vec![10, 11, 12]);
    }

    #[test]
    fn a_project_restart_does_not_select_smp() {
        let rows = [row(10, "/Users/x/.local/bin/hearth smp")];
        assert!(select_duplicate_daemons(&rows, &project(), 0).is_empty());
    }

    #[test]
    fn ps_rows_keep_the_command_after_the_fixed_lstart_field() {
        let stdout = "    1 Mon Oct  5 09:00:44 2026     /sbin/launchd\n  346 Mon Oct  5 09:01:13 2026     /usr/libexec/logd\n";
        let rows = parse_ps_command_rows(stdout);
        assert_eq!(rows[0].pid, 1);
        assert_eq!(rows[0].start_identity, "Mon Oct  5 09:00:44 2026");
        assert_eq!(rows[0].command, "/sbin/launchd");
        assert_eq!(rows[1].command, "/usr/libexec/logd");
    }

    #[test]
    fn lsof_cwd_map_reads_the_first_cwd_per_pid() {
        let stdout = "p10\nfcwd\nn/tmp/proj\np11\nfcwd\nn/tmp/other\n";
        let map = parse_lsof_cwd_map(stdout);
        assert_eq!(
            map.get(&10).map(PathBuf::as_path),
            Some(Path::new("/tmp/proj"))
        );
        assert_eq!(
            map.get(&11).map(PathBuf::as_path),
            Some(Path::new("/tmp/other"))
        );
    }

    #[test]
    fn lexical_path_equality_ignores_a_trailing_slash_and_dot_segments() {
        assert!(paths_equal(
            Path::new("/Users/x/proj"),
            Path::new("/Users/x/proj/")
        ));
        assert!(paths_equal(
            Path::new("/Users/x/proj"),
            Path::new("/Users/x/other/../proj")
        ));
        assert!(!paths_equal(
            Path::new("/Users/x/proj"),
            Path::new("/Users/x/proj-other")
        ));
    }

    #[tokio::test]
    async fn a_live_process_table_parses_and_does_not_select_an_unused_root() {
        let rows = list_live_processes().await.expect("ps must be readable");
        assert!(
            rows.iter().any(|row| row.pid == 1),
            "pid 1 must parse out of ps"
        );
        let selected = select_duplicate_daemons(
            &rows,
            &DaemonRole::Project(PathBuf::from("/tmp/hearth-no-such-root-9f3a")),
            i64::from(std::process::id()),
        );
        assert!(selected.is_empty());
    }

    #[test]
    fn a_rewritten_title_matches_the_same_absolute_executable_only() {
        let exe = "/Users/x/.hearth/shared/installs/redis/8.2.10/bin/redis-server";
        let rewritten = format!("{exe} 127.0.0.1:43886");
        assert!(executable_matches(&rewritten, &[exe.to_string()]));
        assert!(executable_matches(
            &format!("{exe}/../redis-server 127.0.0.1:43886"),
            &[exe.to_string()]
        ));
        assert!(!executable_matches(
            &rewritten,
            &["/other/bin/redis-server".to_string()]
        ));
        assert!(!executable_matches(
            "redis-server 127.0.0.1:43886",
            &[exe.to_string()]
        ));
        assert!(!executable_matches(&rewritten, &[]));
        assert!(!executable_matches(&rewritten, &["node".to_string()]));
        assert!(!executable_matches(
            "/usr/bin/node server.js",
            &["/usr/bin/node".to_string()]
        ));
        assert!(!executable_matches(
            "/Users/x/.local/bin/hearth daemon --root /Users/x/proj",
            &["/Users/x/.local/bin/hearth".to_string()]
        ));
    }
}
