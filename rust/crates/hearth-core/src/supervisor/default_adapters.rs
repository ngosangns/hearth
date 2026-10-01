//! Default (production) adapters: the real `ps`/`lsof`/`docker`/`tailscale` shell-outs and
//! `tokio::process::Command`-based spawning that make a `ProcessSupervisor` actually run real
//! processes and containers, as opposed to the fakes the engine's own unit tests use. Every `ps`
//! call the supervisor makes lives here.
//!
//! Known simplification: `forward_stream` decodes each read chunk with `String::from_utf8_lossy`
//! independently rather than with a stateful streaming UTF-8 decoder — a multi-byte UTF-8
//! character split exactly across a pipe-read boundary can render as a replacement character in
//! forwarded log output. Cosmetic only (log text, not a wire protocol).
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use regex::Regex;
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use crate::catalog::{is_container_command, CommandSpec, ServiceCommand, ServiceId};
use crate::paths::{logs_dir, raw_log_path, resolve_runtime_directory};
use crate::state::ProcessIdentity;

use super::fingerprint::{
    command_argv, is_shell_wrapper_command, normalize_observed_command_fingerprint,
};
use super::process_tree::{
    build_process_tree, parse_ps_alive_rows, parse_ps_tree_rows, ProcessTreeSnapshot,
};
use super::types::{
    DockerContainerRecord, Inspection, ManagedProcess, ObservedProcess, OnOutput, OutputSource,
    OutputTail, PortHolder, PosixProcessRecord, ProbeAdapter, ProcessAdapter, ProcessRecord,
    ProcessSignal, RunBuild, SpawnInput, SupervisorClock, SupervisorError, SupervisorOptions,
    SystemClock,
};

const RAW_LOG_POLL_MS: u64 = 200;
/// A raw capture file is only truncated once it reaches this size — see `drain_raw_log_once`.
const RAW_LOG_TRUNCATE_BYTES: u64 = 8 * 1024 * 1024;
/// How much of a raw capture file one drain forwards per read.
const RAW_LOG_READ_CHUNK: usize = 64 * 1024;
const EXEC_IDENTITY_SETTLE_MS: u64 = 1_000;
/// Upper bound on every observational shell-out (`ps`, `lsof`, `docker inspect`, `tailscale`).
/// These run under the per-service lock, so a wedged Docker Desktop or tailscaled must not block a
/// service's stop — or, via the sequential external-sync loop, every external service's polling.
const PROBE_COMMAND_TIMEOUT: Duration = Duration::from_secs(5);
/// One batched `docker inspect` for every started container per tick — see `ContainerWatcher`.
const CONTAINER_POLL_INTERVAL: Duration = Duration::from_secs(2);

fn to_nix_signal(signal: ProcessSignal) -> nix::sys::signal::Signal {
    match signal {
        ProcessSignal::Sigterm => nix::sys::signal::Signal::SIGTERM,
        ProcessSignal::Sigkill => nix::sys::signal::Signal::SIGKILL,
    }
}

fn send_signal(pid: i64, signal: ProcessSignal) {
    let _ = nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(pid as i32),
        to_nix_signal(signal),
    );
}

/// A negative pid is POSIX shorthand for "the whole process group". pgid 0 is the caller's own
/// group and `kill(-1, …)` signals every process the user owns, so both are refused outright.
fn send_signal_to_group(pgid: i64, signal: ProcessSignal) {
    if pgid <= 1 {
        return;
    }
    let _ = nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(-(pgid as i32)),
        to_nix_signal(signal),
    );
}

/// Runs an observational command and returns `(exit code, stdout)`; `-1` means it could not be
/// spawned or did not finish within `PROBE_COMMAND_TIMEOUT` (its process group is then killed).
async fn capture_command(argv: &[&str]) -> (i32, String) {
    let (code, stdout, _) = capture_command_output(argv).await;
    (code, stdout)
}

/// Same as `capture_command`, plus stderr. Docker inspect writes "Cannot connect to the Docker
/// daemon" and "No such container" there; dropping stderr makes both look like an empty answer.
async fn capture_command_output(argv: &[&str]) -> (i32, String, String) {
    if argv.is_empty() {
        return (-1, String::new(), String::new());
    }
    let mut cmd = Command::new(argv[0]);
    cmd.args(&argv[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    let Ok(mut child) = cmd.spawn() else {
        return (-1, String::new(), String::new());
    };
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let read_stdout = async move {
        let mut buf = Vec::new();
        if let Some(mut stdout) = stdout {
            let _ = stdout.read_to_end(&mut buf).await;
        }
        buf
    };
    let read_stderr = async move {
        let mut buf = Vec::new();
        if let Some(mut stderr) = stderr {
            let _ = stderr.read_to_end(&mut buf).await;
        }
        buf
    };
    let finished = tokio::time::timeout(PROBE_COMMAND_TIMEOUT, async {
        tokio::join!(read_stdout, read_stderr, child.wait())
    })
    .await;
    match finished {
        Ok((out, err, Ok(status))) => (
            status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out).into_owned(),
            String::from_utf8_lossy(&err).into_owned(),
        ),
        Ok((_, _, Err(_))) => (-1, String::new(), String::new()),
        Err(_) => {
            // `id()` is only `Some` while the child is unreaped, which is what keeps its pid (and so
            // its pgid) from having been recycled — never signal a group we can't vouch for.
            if let Some(pid) = child.id() {
                send_signal_to_group(pid as i64, ProcessSignal::Sigkill);
            }
            let _ = child.kill().await;
            (-1, String::new(), String::new())
        }
    }
}

/// `docker inspect` exit 1 is two different answers. "No such container" means the container is
/// gone. Anything else (daemon socket down, empty stderr, a timeout) means the probe could not
/// be asked — callers must treat that as unknown, never as gone.
fn docker_inspect_answered(code: i32, stderr: &str) -> bool {
    if code == 0 {
        return true;
    }
    if code == -1 {
        return false;
    }
    let lower = stderr.to_ascii_lowercase();
    lower.contains("no such object") || lower.contains("no such container")
}

fn ps_observed_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^(\d+)\s+(\d+)\s+(.{24})\s+(.+)$").unwrap())
}

/// `pid`'s live record plus its raw `command=` text — the fingerprint is a hash, so anything shown
/// to a person (the port-holder kill prompt) needs the text itself. `Err` means the probe itself
/// failed (spawn/timeout — exit `-1`, or unparseable output): the caller must not read it as
/// "process gone".
async fn observed_system_process_with_command(
    pid: i64,
) -> Result<Option<(PosixProcessRecord, String)>, ()> {
    let pid_str = pid.to_string();
    let (code, stdout) = capture_command(&[
        "ps", "-o", "pid=", "-o", "pgid=", "-o", "lstart=", "-o", "command=", "-p", &pid_str,
    ])
    .await;
    if code == -1 {
        return Err(());
    }
    if code != 0 {
        return Ok(None);
    }
    let caps = ps_observed_re().captures(stdout.trim()).ok_or(())?;
    let observed_pid: i64 = caps[1].parse().map_err(|_| ())?;
    let pgid: i64 = caps[2].parse().map_err(|_| ())?;
    let start_identity = caps[3].trim().to_string();
    let command_line = caps[4].trim().to_string();
    let command_fingerprint = normalize_observed_command_fingerprint(&caps[4]);
    Ok(Some((
        PosixProcessRecord {
            pid: observed_pid,
            pgid,
            start_identity,
            command_fingerprint,
        },
        command_line,
    )))
}

async fn observed_system_process(pid: i64) -> Result<Option<ObservedProcess>, ()> {
    let Some((record, _)) = observed_system_process_with_command(pid).await? else {
        return Ok(None);
    };
    Ok(Some(ObservedProcess {
        record: ProcessRecord::Posix(record),
        alive: true,
    }))
}

/// Whether `ps` can inspect processes at all. Only a success is cached: `ps` does not disappear,
/// but a one-off failure (a timeout under load) must not refuse every later spawn.
async fn process_inspection_available() -> bool {
    static AVAILABLE: AtomicBool = AtomicBool::new(false);
    if AVAILABLE.load(Ordering::Relaxed) {
        return true;
    }
    let pid = std::process::id().to_string();
    let available = capture_command(&["ps", "-o", "pid=", "-p", &pid]).await.0 == 0;
    if available {
        AVAILABLE.store(true, Ordering::Relaxed);
    }
    available
}

/// Snapshot of `leader_pid`'s whole tree — see `ProcessAdapter::process_tree`.
/// A non-zero `ps` (spawn failure, timeout, or a real error) is `Unknown`, never an empty tree:
/// an empty tree is what a reused leader pid looks like, and signalling from it would hit the
/// wrong group.
async fn system_process_tree(leader_pid: i64, leader_start_identity: &str) -> ProcessTreeSnapshot {
    let (code, stdout) = capture_command(&["ps", "-Ao", "pid=,ppid=,pgid=,lstart="]).await;
    if code != 0 {
        return ProcessTreeSnapshot::Unknown;
    }
    let tree = build_process_tree(
        &parse_ps_tree_rows(&stdout),
        leader_pid,
        leader_start_identity,
    );
    if tree.is_empty() {
        ProcessTreeSnapshot::Absent
    } else {
        ProcessTreeSnapshot::Present(tree)
    }
}

/// `pid -> lstart` for every live process — see `ProcessAdapter::live_start_identities`.
/// `None` when `ps` could not be read. A non-zero exit is that failure, not "nobody is alive".
async fn system_live_start_identities() -> Option<HashMap<i64, String>> {
    let (code, stdout) = capture_command(&["ps", "-Ao", "pid=,lstart="]).await;
    (code == 0).then(|| parse_ps_alive_rows(&stdout))
}

async fn container_running(container_name: &str) -> Result<bool, ()> {
    let (code, stdout, stderr) = capture_command_output(&[
        "docker",
        "inspect",
        "-f",
        "{{.State.Running}}",
        container_name,
    ])
    .await;
    if !docker_inspect_answered(code, &stderr) {
        return Err(());
    }
    Ok(stdout.trim() == "true")
}

/// `Err` when the probe itself failed (spawn/timeout, or a daemon that cannot be reached): a
/// wedged Docker Desktop must not read as "container gone". "No such container" is `Ok(None)`.
async fn container_record(
    container_name: &str,
    command_fingerprint: &str,
) -> Result<Option<DockerContainerRecord>, ()> {
    let (code, stdout, stderr) = capture_command_output(&[
        "docker",
        "inspect",
        "-f",
        "{{.Id}}\t{{.State.Running}}\t{{.State.StartedAt}}",
        container_name,
    ])
    .await;
    if !docker_inspect_answered(code, &stderr) {
        return Err(());
    }
    let parts: Vec<&str> = stdout.trim().split('\t').collect();
    if parts.len() == 3 && parts[1] == "true" && !parts[0].is_empty() && !parts[2].is_empty() {
        Ok(Some(DockerContainerRecord {
            container_name: container_name.to_string(),
            container_id: parts[0].to_string(),
            container_started_at: parts[2].to_string(),
            command_fingerprint: command_fingerprint.to_string(),
        }))
    } else {
        Ok(None)
    }
}

/// `container id -> StartedAt` for each of `container_ids` that is currently running, from one
/// `docker inspect`. `None` when docker could not be asked at all (a missing id is simply absent
/// from the map, which is how a stopped container is reported).
async fn running_containers(container_ids: &[String]) -> Option<HashMap<String, String>> {
    let mut argv = vec![
        "docker",
        "inspect",
        "--type",
        "container",
        "-f",
        "{{.Id}}\t{{.State.Running}}\t{{.State.StartedAt}}",
    ];
    argv.extend(container_ids.iter().map(String::as_str));
    let (code, stdout, stderr) = capture_command_output(&argv).await;
    if !docker_inspect_answered(code, &stderr) {
        return None;
    }
    Some(
        stdout
            .lines()
            .filter_map(|line| {
                let parts: Vec<&str> = line.trim().split('\t').collect();
                (parts.len() == 3 && parts[1] == "true")
                    .then(|| (parts[0].to_string(), parts[2].to_string()))
            })
            .collect(),
    )
}

/// Reports when a started container stops being the instance that was started (stopped, removed,
/// or recreated under a new id). One poll loop per adapter checks every tracked container with a
/// single `docker inspect` every `CONTAINER_POLL_INTERVAL` — a per-container 500 ms poll meant ~20
/// docker CLI spawns a second for ten services — and exits once nothing is tracked. A watch whose
/// receiver was dropped is no longer tracked.
#[derive(Default)]
struct ContainerWatcher {
    state: Mutex<ContainerWatcherState>,
}

#[derive(Default)]
struct ContainerWatcherState {
    watches: Vec<(DockerContainerRecord, oneshot::Sender<i32>)>,
    polling: bool,
}

impl ContainerWatcher {
    fn watch(self: &Arc<Self>, record: DockerContainerRecord, exited: oneshot::Sender<i32>) {
        let start_polling = {
            let mut state = self.state.lock().unwrap();
            state.watches.push((record, exited));
            !std::mem::replace(&mut state.polling, true)
        };
        if start_polling {
            tokio::spawn(self.clone().poll());
        }
    }

    async fn poll(self: Arc<Self>) {
        loop {
            tokio::time::sleep(CONTAINER_POLL_INTERVAL).await;
            let ids: Vec<String> = {
                let mut state = self.state.lock().unwrap();
                state.watches.retain(|(_, exited)| !exited.is_closed());
                if state.watches.is_empty() {
                    state.polling = false;
                    return;
                }
                state
                    .watches
                    .iter()
                    .map(|(record, _)| record.container_id.clone())
                    .collect()
            };
            let Some(running) = running_containers(&ids).await else {
                continue;
            };
            // Only containers this tick actually asked about can be declared gone — a watch added
            // while `docker inspect` ran is absent from `running` without having exited.
            let gone: Vec<oneshot::Sender<i32>> = {
                let mut state = self.state.lock().unwrap();
                let (gone, kept): (Vec<_>, Vec<_>) = std::mem::take(&mut state.watches)
                    .into_iter()
                    .partition(|(record, _)| {
                        ids.contains(&record.container_id)
                            && running.get(&record.container_id)
                                != Some(&record.container_started_at)
                    });
                state.watches = kept;
                gone.into_iter().map(|(_, exited)| exited).collect()
            };
            for exited in gone {
                let _ = exited.send(0);
            }
        }
    }
}

fn same_container_instance(
    expected: &DockerContainerRecord,
    observed: Option<&DockerContainerRecord>,
) -> bool {
    observed
        .map(|o| {
            o.container_name == expected.container_name
                && o.container_id == expected.container_id
                && o.container_started_at == expected.container_started_at
        })
        .unwrap_or(false)
}

async fn tailnet_serving() -> bool {
    let (code, stdout) = capture_command(&["tailscale", "serve", "status", "--json"]).await;
    if code != 0 {
        return false;
    }
    serde_json::from_str::<serde_json::Value>(&stdout)
        .ok()
        .and_then(|v| {
            v.get("Web")
                .and_then(|w| w.as_object())
                .map(|o| !o.is_empty())
        })
        .unwrap_or(false)
}

async fn tcp_probe(port: u16) -> bool {
    let addr = format!("127.0.0.1:{port}");
    tokio::time::timeout(
        Duration::from_millis(250),
        tokio::net::TcpStream::connect(&addr),
    )
    .await
    .map(|r| r.is_ok())
    .unwrap_or(false)
}

/// The pids listening on `port` per `lsof`, each resolved through `ps` so a holder carries the same
/// lstart identity the pid-reuse guard compares before signalling, and its raw command line for the
/// "held by pid N (`cmd`)" refusal/prompt text.
///
/// `None` means "no capability" — `lsof` couldn't even spawn — so callers degrade to the generic
/// "an unowned process" wording and a kill request can't resolve a target (fails closed, not
/// blind). `Some(vec![])` means `lsof` ran and found nobody: between `port_in_use` and this call
/// the holder exited, which the reclaim loop treats as "nothing to signal, just re-poll the port".
async fn port_holders(port: u16) -> Option<Vec<PortHolder>> {
    let spec = format!("-iTCP:{port}");
    let (code, stdout) = capture_command(&["lsof", "-nP", "-t", &spec, "-sTCP:LISTEN"]).await;
    if code == -1 {
        return None;
    }
    let mut holders = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for pid in stdout
        .split_whitespace()
        .filter_map(|line| line.parse::<i64>().ok())
    {
        if !seen.insert(pid) {
            continue;
        }
        if let Ok(Some((record, command_line))) = observed_system_process_with_command(pid).await {
            holders.push(PortHolder {
                pid,
                pgid: record.pgid,
                start_identity: record.start_identity,
                command: command_line,
            });
        }
    }
    Some(holders)
}

async fn forward_stream<R: tokio::io::AsyncRead + Unpin>(
    stream: Option<R>,
    on_output: Option<OnOutput>,
) {
    let Some(mut stream) = stream else { return };
    let mut buf = [0u8; 8192];
    loop {
        match stream.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if let Some(cb) = &on_output {
                    let chunk = String::from_utf8_lossy(&buf[..n]);
                    if !chunk.is_empty() {
                        cb(&chunk);
                    }
                }
            }
        }
    }
}

/// SIGKILLs a still-running child's process group when the future driving it is dropped — how a
/// caller-side timeout (the engine bounds a `command` readiness probe by its readiness timeout)
/// actually ends the probe instead of leaving it running in the background.
struct KillGroupOnDrop(Option<i64>);

impl Drop for KillGroupOnDrop {
    fn drop(&mut self) {
        if let Some(pgid) = self.0.take() {
            send_signal_to_group(pgid, ProcessSignal::Sigkill);
        }
    }
}

/// Spawns `argv` (cwd/env applied, own process group), forwards both stdout and stderr to
/// `on_output`, and returns its exit code (`-1` if it couldn't even be spawned).
async fn run_command(
    argv: &[String],
    cwd: &Path,
    env: &HashMap<String, String>,
    on_output: Option<OnOutput>,
) -> i32 {
    if argv.is_empty() {
        return -1;
    }
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..])
        .current_dir(cwd)
        .env_clear()
        .envs(env)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(_) => return -1,
    };
    let mut kill_on_drop = KillGroupOnDrop(child.id().map(i64::from));
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let stdout_task = tokio::spawn(forward_stream(stdout, on_output.clone()));
    let stderr_task = tokio::spawn(forward_stream(stderr, on_output));
    let status = child.wait().await;
    // Reaped: its pid may now be recycled, so the group is no longer ours to signal.
    kill_on_drop.0 = None;
    let _ = stdout_task.await;
    let _ = stderr_task.await;
    status.ok().and_then(|s| s.code()).unwrap_or(-1)
}

async fn stop_unverified_child(child: &mut tokio::process::Child) {
    // The child was spawned with `process_group(0)`, so its pgid is its pid. Signalling only the
    // leader leaves grandchildren (the shell's own children) holding the port. SIGKILL after the
    // grace matches `run_command`'s drop guard.
    let Some(pid) = child.id() else { return };
    let pgid = pid as i64;
    send_signal_to_group(pgid, ProcessSignal::Sigterm);
    let _ = tokio::time::timeout(Duration::from_millis(500), child.wait()).await;
    if child.id().is_some() {
        send_signal_to_group(pgid, ProcessSignal::Sigkill);
        let _ = tokio::time::timeout(Duration::from_millis(500), child.wait()).await;
    }
}
/// Whether an observation of a just-spawned non-exec process can be trusted as its identity's
/// command fingerprint.
///
/// Right after spawning, the child may still be mid-`execve`, and macOS `ps` then reports a
/// parenthesized placeholder (`(sh)`) instead of the real command line. Storing that as the
/// fingerprint poisons the identity permanently: once exec completes, `ps` reports the real
/// command, `owns_identity` compares unequal, and the daemon disowns and orphans the service it
/// just started — after which the next start fails with `Port N is held by an unowned process`.
///
/// The OBSERVED value (not the expected one) is what must ultimately be stored: for an argv
/// command, `ps` reports the resolved binary path where `argv[0]` may have been a bare name, and
/// later `inspect` comparisons are made against `ps` output. So an observation is accepted when it
/// already equals what we spawned, or when it repeats identically across two polls — which a
/// transient mid-exec placeholder does not.
pub(crate) fn accepts_spawn_observation(
    current: &PosixProcessRecord,
    expected_fingerprint: &str,
    previous: Option<&PosixProcessRecord>,
) -> bool {
    if current.command_fingerprint == expected_fingerprint {
        return true;
    }
    match previous {
        Some(previous) => {
            previous.pid == current.pid
                && previous.pgid == current.pgid
                && previous.start_identity == current.start_identity
                && previous.command_fingerprint == current.command_fingerprint
        }
        None => false,
    }
}

/// Exec fingerprint settling loop — port of `observedStableExecProcess`. A shell wrapper's own
/// fingerprint (the `sh -c ...` line as `ps` shows it *before* exec) differs from the execed
/// program's; this polls until it observes the SAME non-wrapper fingerprint twice in a row, with a
/// settle-check delay between, before trusting it.
async fn observed_stable_exec_process(
    child: &mut tokio::process::Child,
    expected_fingerprint: &str,
) -> Result<PosixProcessRecord, SupervisorError> {
    let pid = child
        .id()
        .ok_or_else(|| SupervisorError("child has no pid".to_string()))? as i64;
    let mut candidate: Option<PosixProcessRecord> = None;
    for _ in 0..20 {
        tokio::time::sleep(Duration::from_millis(25)).await;
        let observed = observed_system_process_with_command(pid).await;
        let posix = match observed {
            Ok(Some((record, command_line))) if !is_shell_wrapper_command(&command_line) => {
                Some(record)
            }
            _ => None,
        };
        let Some(rec) = posix else {
            candidate = None;
            continue;
        };
        if rec.command_fingerprint == expected_fingerprint {
            candidate = None;
            continue;
        }
        let matches_candidate = candidate.as_ref() == Some(&rec);
        if !matches_candidate {
            candidate = Some(rec);
            continue;
        }
        tokio::time::sleep(Duration::from_millis(EXEC_IDENTITY_SETTLE_MS)).await;
        let settled = observed_system_process_with_command(pid).await;
        if let Ok(Some((settled_rec, command_line))) = settled {
            if !is_shell_wrapper_command(&command_line)
                && settled_rec.command_fingerprint != expected_fingerprint
                && Some(&settled_rec) == candidate.as_ref()
            {
                return Ok(settled_rec);
            }
        }
        candidate = None;
    }
    stop_unverified_child(child).await;
    Err(SupervisorError(
        "Unable to establish stable POSIX exec process identity".to_string(),
    ))
}

/// Non-exec shell identity. The first `ps` row of `sh -c` already hashes to the logical command,
/// and macOS `sh` then implicit-execs (dropping quotes) so that stored identity no longer matches
/// the live process. Wait until the same row is still there after `EXEC_IDENTITY_SETTLE_MS`. A
/// wrapper that never execs stays `sh -c` for the whole window and is accepted. A line that
/// changes restarts the window on the new row.
async fn observed_stable_shell_process(
    child: &mut tokio::process::Child,
) -> Result<PosixProcessRecord, SupervisorError> {
    let pid = child
        .id()
        .ok_or_else(|| SupervisorError("child has no pid".to_string()))? as i64;
    let mut candidate: Option<PosixProcessRecord> = None;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    while tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(25)).await;
        let rec = match observed_system_process(pid).await {
            Ok(Some(ObservedProcess {
                record: ProcessRecord::Posix(rec),
                ..
            })) => Some(rec),
            _ => None,
        };
        let Some(rec) = rec else {
            candidate = None;
            continue;
        };
        if candidate.as_ref() != Some(&rec) {
            candidate = Some(rec);
            continue;
        }
        tokio::time::sleep(Duration::from_millis(EXEC_IDENTITY_SETTLE_MS)).await;
        match observed_system_process(pid).await {
            Ok(Some(ObservedProcess {
                record: ProcessRecord::Posix(settled),
                ..
            })) if Some(&settled) == candidate.as_ref() => {
                return Ok(settled);
            }
            Ok(Some(ObservedProcess {
                record: ProcessRecord::Posix(settled),
                ..
            })) => candidate = Some(settled),
            _ => candidate = None,
        }
    }
    stop_unverified_child(child).await;
    Err(SupervisorError(
        "Unable to establish stable POSIX shell process identity".to_string(),
    ))
}

/// The longest prefix of `bytes` that does not end mid-way through a UTF-8 sequence, so a chunked
/// read never renders a split character as a replacement character.
fn utf8_complete_prefix(bytes: &[u8]) -> usize {
    match std::str::from_utf8(bytes) {
        Ok(_) => bytes.len(),
        Err(error) if error.error_len().is_none() => error.valid_up_to(),
        Err(_) => bytes.len(),
    }
}

fn drain_raw_log_once(path: &Path, offset: &AtomicU64, on_output: &OnOutput) {
    use std::io::{Read, Seek, SeekFrom};
    let _: std::io::Result<()> = (|| {
        let size = std::fs::metadata(path)?.len();
        let mut start = offset.load(Ordering::SeqCst);
        if size <= start {
            return Ok(());
        }
        let mut file = std::fs::File::open(path)?;
        file.seek(SeekFrom::Start(start))?;
        let mut buf = vec![0u8; RAW_LOG_READ_CHUNK];
        while start < size {
            let want = ((size - start) as usize).min(RAW_LOG_READ_CHUNK);
            file.read_exact(&mut buf[..want])?;
            // Always hold back a trailing partial character — the writer may be mid-character at
            // the current end of file too, and the next drain picks up from `start`.
            let end = utf8_complete_prefix(&buf[..want]).max(1);
            let text = String::from_utf8_lossy(&buf[..end]);
            if !text.is_empty() {
                on_output(&text);
            }
            start += end as u64;
            offset.store(start, Ordering::SeqCst);
            file.seek(SeekFrom::Start(start))?;
        }
        drop(file);
        // copytruncate, only once the file is big: the writer's fd is append-mode, so after
        // `set_len(0)` its next write lands at the new end. Anything it appends between the
        // `metadata` check and `set_len` is still lost — the size check narrows that window but
        // cannot close it — so truncation is kept rare rather than attempted on every quiet poll.
        if size >= RAW_LOG_TRUNCATE_BYTES {
            let write_file = std::fs::OpenOptions::new().write(true).open(path)?;
            if write_file.metadata()?.len() == size {
                write_file.set_len(0)?;
                offset.store(0, Ordering::SeqCst);
            }
        }
        Ok(())
    })();
}

/// `docker compose up` forwards only the compose CLI's own status lines — a container's real
/// stdout/stderr lives behind `docker logs`. This follows it, bounded by `since`/`tail` — a daemon
/// adopting a long-running external container must not replay days of retained output into the log
/// store on every re-attach. The returned stop handle cancels the follow task, which kills the
/// `docker logs` child; the task also ends on its own when the container stops, since
/// `docker logs --follow` exits with it.
fn tail_container_logs(
    container_name: String,
    since: Option<String>,
    tail: Option<u64>,
    env: HashMap<String, String>,
    on_output: OnOutput,
) -> OutputTail {
    let cancel = CancellationToken::new();
    let task_cancel = cancel.clone();
    let done = Arc::new(AtomicBool::new(false));
    let task_done = done.clone();
    tokio::spawn(async move {
        let mut cmd = Command::new("docker");
        cmd.arg("logs").arg("--follow");
        if let Some(since) = &since {
            cmd.arg("--since").arg(since);
        }
        if let Some(tail) = tail {
            cmd.arg("--tail").arg(tail.to_string());
        }
        cmd.arg(&container_name)
            .env_clear()
            .envs(&env)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0);
        let mut child = match cmd.spawn() {
            Ok(child) => child,
            Err(_) => return,
        };
        let stdout_task =
            tokio::spawn(forward_stream(child.stdout.take(), Some(on_output.clone())));
        let stderr_task = tokio::spawn(forward_stream(child.stderr.take(), Some(on_output)));
        tokio::select! {
            _ = task_cancel.cancelled() => {
                let _ = child.kill().await;
            }
            _ = child.wait() => {}
        }
        let _ = stdout_task.await;
        let _ = stderr_task.await;
        task_done.store(true, Ordering::SeqCst);
    });
    OutputTail::new(Box::new(move || cancel.cancel()), done)
}

/// Port of `tailFile` — polls a raw capture file every `RAW_LOG_POLL_MS`, forwarding new bytes and
/// truncating what it's read. The returned tail's `done` is the same flag `stop` sets — this
/// follower never finishes on its own (the raw file outlives whatever wrote to it).
fn tail_file(path: PathBuf, skip_backlog: bool, on_output: OnOutput) -> OutputTail {
    let stopped = Arc::new(AtomicBool::new(false));
    // Adopted processes keep the previous daemon's raw capture file: everything before this
    // offset was already forwarded, so re-reading it would replay old output into the log store.
    let offset = Arc::new(AtomicU64::new(if skip_backlog {
        std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0)
    } else {
        0
    }));
    let task_path = path.clone();
    let task_output = on_output.clone();
    let task_offset = offset.clone();
    let task_stopped = stopped.clone();
    let handle = tokio::spawn(async move {
        while !task_stopped.load(Ordering::SeqCst) {
            // Synchronous file I/O — kept off the async workers.
            let (path, offset, output) =
                (task_path.clone(), task_offset.clone(), task_output.clone());
            let _ =
                tokio::task::spawn_blocking(move || drain_raw_log_once(&path, &offset, &output))
                    .await;
            tokio::time::sleep(Duration::from_millis(RAW_LOG_POLL_MS)).await;
        }
    });
    let done = stopped.clone();
    OutputTail::new(
        Box::new(move || {
            stopped.store(true, Ordering::SeqCst);
            drain_raw_log_once(&path, &offset, &on_output);
            handle.abort();
        }),
        done,
    )
}

fn merged_environment(
    base: &HashMap<String, String>,
    command_env: &Option<HashMap<String, String>>,
) -> HashMap<String, String> {
    let mut env = base.clone();
    if let Some(extra) = command_env {
        env.extend(extra.clone());
    }
    env
}

pub struct DefaultProcessAdapter {
    root: PathBuf,
    runtime_directory: PathBuf,
    base_environment: HashMap<String, String>,
    containers: Arc<ContainerWatcher>,
}

#[async_trait]
impl ProcessAdapter for DefaultProcessAdapter {
    async fn spawn(
        &self,
        input: SpawnInput,
        on_output: OnOutput,
    ) -> Result<ManagedProcess, SupervisorError> {
        let env = merged_environment(&self.base_environment, &input.command.environment);

        if is_container_command(&input.command) {
            let (argv, _) = command_argv(&input.command.command);
            let cwd = self.root.join(&input.command.cwd);
            let code = run_command(&argv, &cwd, &env, Some(on_output)).await;
            if code != 0 {
                return Err(SupervisorError(format!(
                    "Docker service command exited with {code}"
                )));
            }
            let container_name = input.command.container_name.clone().unwrap();
            let record = container_record(&container_name, &input.command_fingerprint)
                .await
                .ok()
                .flatten()
                .ok_or_else(|| {
                    SupervisorError(format!(
                        "Docker container {container_name} is not running after start"
                    ))
                })?;
            let (tx, rx) = oneshot::channel();
            self.containers.watch(record.clone(), tx);
            return Ok(ManagedProcess {
                record: ProcessRecord::Docker(record),
                exited: rx,
            });
        }

        if !process_inspection_available().await {
            return Err(SupervisorError("POSIX process inspection is unavailable; refusing to start an unverified service process".to_string()));
        }

        // Managed dev processes must outlive the daemon that spawned them. Piping their
        // stdout/stderr straight into this daemon would mean the pipe's read end closes whenever
        // the daemon exits, earning the child a SIGPIPE on its next log write. Redirect to a plain
        // file instead; `attach_output`/`tail_file` reads that file separately.
        let (argv, exec) = command_argv(&input.command.command);
        tokio::fs::create_dir_all(logs_dir(&self.runtime_directory))
            .await
            .map_err(|e| SupervisorError(format!("failed to create logs directory: {e}")))?;
        let raw = raw_log_path(&self.runtime_directory, &input.service_id);
        std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&raw)
            .map_err(|e| SupervisorError(e.to_string()))?;
        let stdout_file = std::fs::OpenOptions::new()
            .append(true)
            .open(&raw)
            .map_err(|e| SupervisorError(e.to_string()))?;
        let stderr_file = stdout_file
            .try_clone()
            .map_err(|e| SupervisorError(e.to_string()))?;

        let cwd = self.root.join(&input.command.cwd);
        let mut cmd = Command::new(&argv[0]);
        cmd.args(&argv[1..])
            .current_dir(&cwd)
            .env_clear()
            .envs(&env)
            .stdout(Stdio::from(stdout_file))
            .stderr(Stdio::from(stderr_file))
            .process_group(0);
        let mut child = cmd
            .spawn()
            .map_err(|e| SupervisorError(format!("Service process failed to start: {e}")))?;
        let pid = child
            .id()
            .ok_or_else(|| SupervisorError("spawned child reported no pid".to_string()))?
            as i64;

        let record = if exec {
            observed_stable_exec_process(&mut child, &input.command_fingerprint).await?
        } else if matches!(input.command.command, CommandSpec::Shell { .. }) {
            observed_stable_shell_process(&mut child).await?
        } else {
            // A freshly spawned child can still be mid-`execve` when `ps` is asked about it, and
            // macOS then reports a placeholder command line — literally `(sh)` — instead of the
            // real one (reproducible: ~15% of spawns under load). Taking that first readable row
            // as the authoritative fingerprint poisons the identity for the rest of the service's
            // life: once exec completes `ps` reports the real command, `owns_identity` compares
            // unequal, and the daemon disowns and orphans the service it just started — after
            // which the next start fails with `Port N is held by an unowned process`.
            //
            // The OBSERVED value still has to be what gets stored (for an argv command `ps`
            // reports the resolved binary path where `argv[0]` may have been a bare name, and
            // later `inspect` comparisons are made against `ps` output), so this waits for a
            // trustworthy observation rather than substituting the expected fingerprint: either it
            // already equals what we spawned, or it repeats identically across two polls — which a
            // mid-exec placeholder never does.
            let mut observed = None;
            let mut previous: Option<PosixProcessRecord> = None;
            for attempt in 0..8 {
                if let Ok(Some(ObservedProcess {
                    record: ProcessRecord::Posix(current),
                    ..
                })) = observed_system_process(pid).await
                {
                    if accepts_spawn_observation(
                        &current,
                        &input.command_fingerprint,
                        previous.as_ref(),
                    ) {
                        observed = Some(current);
                        break;
                    }
                    previous = Some(current);
                } else {
                    previous = None;
                }
                if attempt < 7 {
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
            }
            match observed {
                Some(r) => r,
                None => {
                    stop_unverified_child(&mut child).await;
                    return Err(SupervisorError(
                        "Unable to establish POSIX process ownership identity after 8 inspections"
                            .to_string(),
                    ));
                }
            }
        };

        let (tx, rx) = oneshot::channel();
        tokio::spawn(async move {
            let status = child.wait().await;
            let code = status.ok().and_then(|s| s.code()).unwrap_or(-1);
            let _ = tx.send(code);
        });
        Ok(ManagedProcess {
            record: ProcessRecord::Posix(record),
            exited: rx,
        })
    }

    async fn inspect(&self, identity: &ProcessIdentity) -> Inspection {
        match identity {
            ProcessIdentity::Docker(id) => {
                match container_record(&id.container_name, &id.command_fingerprint).await {
                    Err(()) => Inspection::Unknown,
                    Ok(None) => Inspection::Gone,
                    Ok(Some(record)) => {
                        let expected = DockerContainerRecord {
                            container_name: id.container_name.clone(),
                            container_id: id.container_id.clone(),
                            container_started_at: id.container_started_at.clone(),
                            command_fingerprint: id.command_fingerprint.clone(),
                        };
                        if same_container_instance(&expected, Some(&record)) {
                            Inspection::Observed(ObservedProcess {
                                record: ProcessRecord::Docker(record),
                                alive: true,
                            })
                        } else {
                            Inspection::Gone
                        }
                    }
                }
            }
            ProcessIdentity::Posix(id) => {
                if id.pid == 0 {
                    return Inspection::Observed(ObservedProcess {
                        record: ProcessRecord::Posix(PosixProcessRecord {
                            pid: 0,
                            pgid: 0,
                            start_identity: String::new(),
                            command_fingerprint: id.command_fingerprint.clone(),
                        }),
                        alive: false,
                    });
                }
                match observed_system_process(id.pid).await {
                    Err(()) => Inspection::Unknown,
                    Ok(None) => Inspection::Gone,
                    Ok(Some(observed)) => Inspection::Observed(observed),
                }
            }
        }
    }

    async fn signal_group(&self, pgid: i64, signal: ProcessSignal) {
        send_signal_to_group(pgid, signal);
    }

    async fn process_tree(
        &self,
        leader_pid: i64,
        leader_start_identity: &str,
    ) -> ProcessTreeSnapshot {
        system_process_tree(leader_pid, leader_start_identity).await
    }

    async fn live_start_identities(&self) -> Option<HashMap<i64, String>> {
        system_live_start_identities().await
    }

    async fn signal_pid(&self, pid: i64, expected_start_identity: &str, signal: ProcessSignal) {
        if pid <= 1 {
            return;
        }
        // The lstart the caller resolved is compared again at signal time — a squatter that died
        // and had its pid recycled between resolve and kill must never take the signal.
        if let Ok(Some(ObservedProcess {
            record: ProcessRecord::Posix(record),
            alive: true,
        })) = observed_system_process(pid).await
        {
            if record.start_identity == expected_start_identity {
                send_signal(pid, signal);
            }
        }
    }

    async fn stop_container(
        &self,
        command: &ServiceCommand,
        on_output: OnOutput,
    ) -> Option<Result<(), SupervisorError>> {
        let default_stop = CommandSpec::Argv {
            argv: vec![
                "docker".to_string(),
                "compose".to_string(),
                "stop".to_string(),
            ],
        };
        let (argv, _) = command_argv(
            command
                .docker_stop_command
                .as_ref()
                .unwrap_or(&default_stop),
        );
        // Compose is cwd- and env-sensitive (`COMPOSE_FILE`, `COMPOSE_PROJECT_NAME`). Stop has to
        // use the same directory and merged environment the start used, or it targets a different project.
        let cwd = self.root.join(&command.cwd);
        let env = merged_environment(&self.base_environment, &command.environment);
        let code = run_command(&argv, &cwd, &env, Some(on_output)).await;
        Some(if code != 0 {
            Err(SupervisorError(format!(
                "Docker service stop exited with {code}"
            )))
        } else {
            Ok(())
        })
    }

    fn attach_output(
        &self,
        service_id: &ServiceId,
        source: OutputSource<'_>,
        on_output: OnOutput,
    ) -> Option<OutputTail> {
        match source {
            OutputSource::Process { skip_backlog } => Some(tail_file(
                raw_log_path(&self.runtime_directory, service_id),
                skip_backlog,
                on_output,
            )),
            OutputSource::Container {
                container_name,
                since,
                tail,
            } => Some(tail_container_logs(
                container_name.to_string(),
                since.map(str::to_string),
                tail,
                self.base_environment.clone(),
                on_output,
            )),
        }
    }
}

pub struct DefaultProbeAdapter {
    root: PathBuf,
    base_environment: HashMap<String, String>,
    http_client: reqwest::Client,
}

#[async_trait]
impl ProbeAdapter for DefaultProbeAdapter {
    async fn tcp(&self, port: u16) -> bool {
        tcp_probe(port).await
    }
    async fn http(&self, url: &str) -> bool {
        match tokio::time::timeout(Duration::from_millis(250), self.http_client.get(url).send())
            .await
        {
            Ok(Ok(response)) => response.status().is_success(),
            _ => false,
        }
    }
    async fn container(&self, container_name: &str) -> bool {
        container_running(container_name).await.unwrap_or(false)
    }
    async fn tailnet(&self) -> bool {
        tailnet_serving().await
    }
    async fn port_in_use(&self, port: u16) -> Option<bool> {
        Some(tcp_probe(port).await)
    }
    async fn port_holders(&self, port: u16) -> Option<Vec<PortHolder>> {
        port_holders(port).await
    }
    async fn command(&self, command: &CommandSpec, cwd: Option<&str>) -> Option<bool> {
        let (argv, _) = command_argv(command);
        let dir = self.root.join(cwd.unwrap_or("."));
        Some(run_command(&argv, &dir, &self.base_environment, None).await == 0)
    }
}

pub struct DefaultRunBuild {
    root: PathBuf,
    base_environment: HashMap<String, String>,
}

#[async_trait]
impl RunBuild for DefaultRunBuild {
    async fn run(
        &self,
        command: &ServiceCommand,
        on_output: OnOutput,
        cancel: CancellationToken,
    ) -> Result<(), SupervisorError> {
        if cancel.is_cancelled() {
            return Err(SupervisorError("Build cancelled".to_string()));
        }
        let (argv, _) = command_argv(&command.command);
        let env = merged_environment(&self.base_environment, &command.environment);
        let cwd = self.root.join(&command.cwd);
        let mut cmd = Command::new(&argv[0]);
        cmd.args(&argv[1..])
            .current_dir(&cwd)
            .env_clear()
            .envs(&env)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0);
        let mut child = cmd
            .spawn()
            .map_err(|e| SupervisorError(format!("Build command failed to start: {e}")))?;
        let pid = child.id().map(|p| p as i64);
        // If this future is dropped mid-cancel (a caller racing its own select against `run`),
        // the group is still killed. Cleared once the child has been reaped.
        let mut kill_on_drop = KillGroupOnDrop(pid);
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let stdout_task = tokio::spawn(forward_stream(stdout, Some(on_output.clone())));
        let stderr_task = tokio::spawn(forward_stream(stderr, Some(on_output)));

        tokio::select! {
            status = child.wait() => {
                kill_on_drop.0 = None;
                let _ = stdout_task.await;
                let _ = stderr_task.await;
                let code = status.ok().and_then(|s| s.code()).unwrap_or(-1);
                if code != 0 { Err(SupervisorError(format!("Build command exited with {code}"))) } else { Ok(()) }
            }
            _ = cancel.cancelled() => {
                if let Some(pid) = pid { send_signal_to_group(pid, ProcessSignal::Sigterm); }
                let _ = tokio::time::timeout(Duration::from_secs(5), child.wait()).await;
                if child.id().is_some() {
                    if let Some(pid) = pid { send_signal_to_group(pid, ProcessSignal::Sigkill); }
                    let _ = child.wait().await;
                }
                kill_on_drop.0 = None;
                let _ = stdout_task.await;
                let _ = stderr_task.await;
                Err(SupervisorError("Build cancelled".to_string()))
            }
        }
    }
}

/// `artifact:` installs for file-loaded catalogs — delegates to the same tarball machinery the
/// shared-services installer uses, scoped to the project's runtime directory.
pub struct DefaultArtifactInstaller {
    pub root: PathBuf,
    pub runtime_directory: PathBuf,
}

#[async_trait]
impl super::types::ArtifactInstaller for DefaultArtifactInstaller {
    async fn install(
        &self,
        service: &crate::catalog::ServiceDefinition,
        on_output: OnOutput,
    ) -> Result<(), SupervisorError> {
        let artifact = service
            .artifact
            .clone()
            .ok_or_else(|| SupervisorError(format!("{}: no artifact declared", service.id)))?;
        crate::shared::install::install_service_artifact(
            &self.root,
            &self.runtime_directory,
            service,
            &artifact,
            &|line| on_output(line),
        )
        .await
        .map(|_| ())
        .map_err(|e| SupervisorError(e.0))
    }
}

/// `base_environment` should come from `crate::env::resolve_base_environment` for a daemon that
/// might be launched from a GUI (bare `PATH`, no login-shell customization) — defaults to the
/// current process's own environment, i.e. whatever spawned the daemon.
pub fn default_supervisor_options(
    root: PathBuf,
    runtime_directory: Option<PathBuf>,
    base_environment: Option<HashMap<String, String>>,
) -> SupervisorOptions {
    let runtime_directory =
        runtime_directory.unwrap_or_else(|| resolve_runtime_directory(&root, None));
    let base_environment = base_environment.unwrap_or_else(|| std::env::vars().collect());
    let http_client = reqwest::Client::new();
    let clock: Arc<dyn SupervisorClock> = Arc::new(SystemClock);
    SupervisorOptions {
        process: Arc::new(DefaultProcessAdapter {
            root: root.clone(),
            runtime_directory: runtime_directory.clone(),
            base_environment: base_environment.clone(),
            containers: Arc::default(),
        }),
        run_build: Arc::new(DefaultRunBuild {
            root: root.clone(),
            base_environment: base_environment.clone(),
        }),
        artifact_installer: Some(Arc::new(DefaultArtifactInstaller {
            root: root.clone(),
            runtime_directory,
        })),
        probes: Arc::new(DefaultProbeAdapter {
            root,
            base_environment,
            http_client,
        }),
        preparation: None,
        clock,
        readiness_timeout_ms: 10_000,
        readiness_backoff_ms: 100,
        termination_grace_ms: 5_000,
        is_closing: Arc::new(|| false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn docker_inspect_distinguishes_a_missing_container_from_a_dead_daemon() {
        assert!(docker_inspect_answered(0, ""));
        assert!(docker_inspect_answered(1, "Error: No such container: api"));
        assert!(docker_inspect_answered(1, "Error: No such object: abc"));
        assert!(!docker_inspect_answered(-1, ""));
        assert!(!docker_inspect_answered(
            1,
            "Cannot connect to the Docker daemon at unix:///var/run/docker.sock"
        ));
        assert!(!docker_inspect_answered(1, ""));
    }
    use crate::catalog::{ServiceCatalog, ServiceDefinition, StartFailurePolicy};
    use crate::state::{
        ActualServiceState, DesiredServiceState, ServiceLifecycleState, ServiceReadiness,
    };
    use crate::supervisor::{Host, ProcessSupervisor};
    use std::collections::HashMap as Map;
    use std::sync::Mutex as StdMutex;

    /// A one-service in-memory `Host` for the real-adapter tests below.
    struct TestHost {
        instance_id: &'static str,
        catalog: Arc<ServiceCatalog>,
        states: StdMutex<Map<String, ServiceLifecycleState>>,
    }

    impl TestHost {
        fn new(instance_id: &'static str, service: ServiceDefinition) -> Arc<Self> {
            let catalog = ServiceCatalog {
                services: vec![service],
                groups: Map::new(),
                group_tree: Vec::new(),
                compose_file: None,
                runtime_directory: None,
                start_failure_policy: StartFailurePolicy::StopOnFirstFailureKeepStarted,
                private_file_guard: None,
            };
            Arc::new(Self {
                instance_id,
                catalog: Arc::new(catalog),
                states: StdMutex::new(Map::new()),
            })
        }
    }

    #[async_trait]
    impl Host for TestHost {
        fn instance_id(&self) -> String {
            self.instance_id.to_string()
        }
        fn catalog(&self) -> Arc<ServiceCatalog> {
            self.catalog.clone()
        }
        fn service_states(&self) -> Vec<ServiceLifecycleState> {
            let states = self.states.lock().unwrap();
            self.catalog
                .services
                .iter()
                .map(|s| {
                    states.get(&s.id).cloned().unwrap_or(ServiceLifecycleState {
                        service_id: s.id.clone(),
                        desired_state: DesiredServiceState::Stopped,
                        actual_state: ActualServiceState::Stopped,
                        readiness: ServiceReadiness::Unknown,
                        generation: 0,
                        identity: None,
                        readiness_kind: None,
                        readiness_detail: None,
                        created_at: "2024-01-01T00:00:00.000Z".to_string(),
                        updated_at: "2024-01-01T00:00:00.000Z".to_string(),
                        exited_at: None,
                        exit_code: None,
                        error: None,
                        current_operation_id: None,
                    })
                })
                .collect()
        }
        async fn set_service_state(&self, next: ServiceLifecycleState) {
            self.states
                .lock()
                .unwrap()
                .insert(next.service_id.clone(), next);
        }
        async fn append_log(&self, _service_id: &str, _data: &str) {}
        fn publish(&self, _event_type: &str, _data: serde_json::Value) {}
    }

    #[tokio::test]
    async fn tcp_probe_reports_true_for_a_listening_port_and_false_otherwise() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            loop {
                if listener.accept().await.is_err() {
                    break;
                }
            }
        });
        assert!(tcp_probe(port).await);
        // Port 1 is a privileged low port essentially never bound to on a dev machine.
        assert!(!tcp_probe(1).await);
    }

    #[tokio::test]
    async fn observed_system_process_finds_a_real_spawned_process() {
        let mut child = Command::new("sh")
            .args(["-c", "sleep 5"])
            .process_group(0)
            .spawn()
            .unwrap();
        let pid = child.id().unwrap() as i64;
        tokio::time::sleep(Duration::from_millis(50)).await;
        let observed = observed_system_process(pid).await;
        match observed {
            Ok(Some(ObservedProcess {
                record: ProcessRecord::Posix(rec),
                alive,
            })) => {
                assert!(alive);
                assert_eq!(rec.pid, pid);
            }
            _ => panic!("expected to observe the real spawned process"),
        }
        let _ = child.start_kill();
        let _ = child.wait().await;
    }

    #[tokio::test]
    async fn observed_system_process_returns_none_for_an_implausible_pid() {
        assert!(matches!(
            observed_system_process(i32::MAX as i64).await,
            Ok(None)
        ));
    }

    #[tokio::test]
    async fn run_command_captures_exit_code_and_forwards_output() {
        let output: Arc<std::sync::Mutex<String>> = Arc::new(std::sync::Mutex::new(String::new()));
        let output_for_cb = output.clone();
        let on_output: OnOutput =
            Arc::new(move |data: &str| output_for_cb.lock().unwrap().push_str(data));
        let code = run_command(
            &[
                "sh".to_string(),
                "-c".to_string(),
                "echo hello-from-run-command".to_string(),
            ],
            Path::new("/tmp"),
            &Map::new(),
            Some(on_output),
        )
        .await;
        assert_eq!(code, 0);
        assert!(output.lock().unwrap().contains("hello-from-run-command"));
    }

    #[tokio::test]
    async fn run_command_reports_a_nonzero_exit_code() {
        let code = run_command(
            &["sh".to_string(), "-c".to_string(), "exit 7".to_string()],
            Path::new("/tmp"),
            &Map::new(),
            None,
        )
        .await;
        assert_eq!(code, 7);
    }

    /// Real end-to-end test of the production `ProcessAdapter`: start a real service through a real
    /// `ProcessSupervisor` (using `DefaultProcessAdapter`/`DefaultProbeAdapter`), verify a real OS
    /// process comes up with `tcp` readiness against a real listening port, then stop it and verify
    /// the OS process is actually gone. Everything else in this crate's supervisor tests uses fakes;
    /// this is the one test proving the real adapters work end-to-end together.
    #[tokio::test]
    async fn real_process_supervisor_starts_and_stops_a_real_tcp_service() {
        use crate::catalog::{
            CommandSpec as Spec, ReadinessSpec, ServiceCommand as Cmd, ServiceKind,
            ServiceProfiles, ServiceRunProfile,
        };

        // A tiny real TCP server: `nc -l <port>` (or, portably, a short Python-free shell one-liner
        // using /dev/tcp is bash-only) — use `sh -c` piping to a listening `nc`, but simplest and
        // most portable across darwin/linux CI runners is spawning our own listener via a helper
        // Rust binary isn't available here, so instead drive the readiness against a port this test
        // process itself pre-binds and hands off — but the *service* must be the one bound to it to
        // be a meaningful end-to-end proof. `nc` (netcat) ships on macOS and most Linux dev images.
        let port = {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();
            drop(listener); // free it immediately so the real spawned service can bind it
            port
        };

        let service = ServiceDefinition {
            id: "nc-server".to_string(),
            label: None,
            kind: Some(ServiceKind::Application),
            ownership: None,
            disabled: false,
            profiles: ServiceProfiles {
                run: ServiceRunProfile::Verified {
                    command: Cmd {
                        command: Spec::Shell {
                            shell: format!("exec nc -lk {port}"),
                            exec: Some(true),
                        },
                        cwd: "/tmp".to_string(),
                        environment: None,
                        container_name: None,
                        docker_stop_command: None,
                    },
                    readiness: ReadinessSpec::Tcp { port },
                    readiness_timeout_ms: Some(5_000),
                    preparation: None,
                    preparation_command: None,
                },
                build: None,
            },
            ports: None,
            urls: None,
            artifact: None,
        };
        let host = TestHost::new("real-adapter-test", service);

        let runtime_dir = std::env::temp_dir().join(format!(
            "hearth-core-real-adapter-test-{}",
            uuid::Uuid::new_v4()
        ));
        let options =
            default_supervisor_options(PathBuf::from("/tmp"), Some(runtime_dir.clone()), None);
        let supervisor = ProcessSupervisor::new(host.clone(), options);

        supervisor
            .start(&"nc-server".to_string(), None)
            .await
            .expect("real nc-backed service should start and become ready");
        let state = host
            .service_states()
            .into_iter()
            .find(|s| s.service_id == "nc-server")
            .unwrap();
        assert_eq!(state.actual_state, ActualServiceState::Ready);
        let pid = match state.identity.unwrap() {
            ProcessIdentity::Posix(p) => p.pid,
            _ => panic!("expected a posix identity"),
        };
        assert!(
            nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid as i32), None).is_ok(),
            "the real OS process should be alive"
        );

        supervisor
            .stop(&"nc-server".to_string(), None)
            .await
            .expect("stop should succeed");
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(
            nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid as i32), None).is_err(),
            "the real OS process must be gone after stop()"
        );

        let _ = std::fs::remove_dir_all(&runtime_dir);
    }

    /// Real end-to-end test of the `is_container_command` path in `DefaultProcessAdapter::spawn`
    /// (and `stop_container`): a real `docker compose` project, driven entirely through a real
    /// `ProcessSupervisor`. This is the Docker half of the "not yet integration-tested against a
    /// real `docker compose`/`tailscale serve` setup" gap flagged when Phase 2 first landed — the
    /// container-identity *logic* (fingerprint matching, adoption, reap-on-mismatch) was already
    /// covered by fakes; what those fakes could never prove is that the real `docker inspect`/
    /// `docker compose up|down` shell-outs in this file actually parse and drive a real daemon.
    /// Needs a real `docker` (with a running daemon) on the test machine — every other test in this
    /// crate that shells out to a real tool (`nc`, `ps`, `sh`) makes the same assumption, so this
    /// follows the same convention rather than adding a first skip-if-missing guard.
    #[tokio::test]
    async fn real_process_supervisor_starts_and_stops_a_real_docker_compose_service() {
        use crate::catalog::{
            CommandSpec as Spec, ReadinessSpec, ServiceCommand as Cmd, ServiceKind,
            ServiceProfiles, ServiceRunProfile,
        };

        let dir = tempfile::tempdir().unwrap();
        let compose_path = dir.path().join("docker-compose.yml");
        std::fs::write(
            &compose_path,
            "services:\n  app:\n    image: alpine:latest\n    command: [\"sleep\", \"3600\"]\n",
        )
        .unwrap();
        // Unique per test run so concurrent/repeated runs never collide on a project name, and
        // lowercase-hex-only so it's always a valid Compose project name.
        let project = format!("hearth-core-test-{}", uuid::Uuid::new_v4().simple());
        let container_name = format!("{project}-app-1");
        let compose_path_str = compose_path.to_string_lossy().to_string();

        let service = ServiceDefinition {
            id: "docker-app".to_string(),
            label: None,
            kind: Some(ServiceKind::Application),
            ownership: None,
            disabled: false,
            profiles: ServiceProfiles {
                run: ServiceRunProfile::Verified {
                    command: Cmd {
                        command: Spec::Shell { shell: format!("docker compose -p {project} -f '{compose_path_str}' up -d"), exec: None },
                        cwd: ".".to_string(),
                        environment: None,
                        container_name: Some(container_name.clone()),
                        docker_stop_command: Some(Spec::Shell { shell: format!("docker compose -p {project} -f '{compose_path_str}' down --timeout 1"), exec: None }),
                    },
                    readiness: ReadinessSpec::Container,
                    readiness_timeout_ms: Some(30_000),
                    preparation: None,
                    preparation_command: None,
                },
                build: None,
            },
            ports: None,
            urls: None,
            artifact: None,
        };
        let host = TestHost::new("real-docker-adapter-test", service);

        let runtime_dir = std::env::temp_dir().join(format!(
            "hearth-core-real-docker-adapter-test-{}",
            uuid::Uuid::new_v4()
        ));
        let options =
            default_supervisor_options(dir.path().to_path_buf(), Some(runtime_dir.clone()), None);
        let supervisor = ProcessSupervisor::new(host.clone(), options);

        let start_result = supervisor.start(&"docker-app".to_string(), None).await;
        // Always clean up the compose project, whether start succeeded or not, so a failed
        // assertion never leaves a real container running on the test machine.
        let cleanup = || {
            let _ = std::process::Command::new("docker")
                .args([
                    "compose",
                    "-p",
                    &project,
                    "-f",
                    &compose_path_str,
                    "down",
                    "--timeout",
                    "1",
                ])
                .output();
        };
        if let Err(error) = &start_result {
            cleanup();
            let _ = std::fs::remove_dir_all(&runtime_dir);
            panic!("real docker-backed service should start and become ready: {error:?}");
        }

        let state = host
            .service_states()
            .into_iter()
            .find(|s| s.service_id == "docker-app")
            .unwrap();
        assert_eq!(state.actual_state, ActualServiceState::Ready);
        match &state.identity {
            Some(ProcessIdentity::Docker(id)) => assert_eq!(id.container_name, container_name),
            other => panic!("expected a docker identity, got {other:?}"),
        }
        assert!(
            container_running(&container_name).await.unwrap_or(false),
            "the real container should be running per `docker inspect`"
        );

        let stop_result = supervisor.stop(&"docker-app".to_string(), None).await;
        cleanup();
        let _ = std::fs::remove_dir_all(&runtime_dir);
        stop_result.expect("stop should succeed");
        assert!(
            !container_running(&container_name).await.unwrap_or(false),
            "the real container must be stopped/removed after stop()"
        );
    }

    /// Real end-to-end test of the declarative `preparation_command` (the JSON-serializable
    /// stand-in for a bespoke `PreparationAdapter` added to unblock a YAML-only
    /// consumer whose real services depend on a prepare step — see AGENTS.md's Rust-rewrite status
    /// for the `viclass` cutover this was built to unblock). Proves the whole real chain: catalog
    /// declares a command, the engine calls the real `DefaultProbeAdapter::command` (the same
    /// adapter method `ReadinessSpec::Command` readiness already exercises elsewhere), which really
    /// spawns a shell command — here, one that writes a marker file — *before* the service's own
    /// run command starts.
    #[tokio::test]
    async fn real_process_supervisor_runs_a_real_preparation_command_before_starting_the_service() {
        use crate::catalog::{
            CommandSpec as Spec, PreparationCommand, ReadinessSpec, ServiceCommand as Cmd,
            ServiceKind, ServiceProfiles, ServiceRunProfile,
        };

        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("prepared.marker");
        let port = {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();
            drop(listener);
            port
        };

        let service = ServiceDefinition {
            id: "prepped-server".to_string(),
            label: None,
            kind: Some(ServiceKind::Application),
            ownership: None,
            disabled: false,
            profiles: ServiceProfiles {
                run: ServiceRunProfile::Verified {
                    command: Cmd {
                        command: Spec::Shell {
                            shell: format!("exec nc -lk {port}"),
                            exec: Some(true),
                        },
                        cwd: "/tmp".to_string(),
                        environment: None,
                        container_name: None,
                        docker_stop_command: None,
                    },
                    readiness: ReadinessSpec::Tcp { port },
                    readiness_timeout_ms: Some(5_000),
                    preparation: None,
                    preparation_command: Some(PreparationCommand {
                        command: Spec::Shell {
                            shell: format!("touch '{}'", marker.display()),
                            exec: None,
                        },
                        cwd: None,
                        serialization_key: None,
                    }),
                },
                build: None,
            },
            ports: None,
            urls: None,
            artifact: None,
        };
        let host = TestHost::new("real-preparation-command-test", service);

        let runtime_dir = std::env::temp_dir().join(format!(
            "hearth-core-real-preparation-command-test-{}",
            uuid::Uuid::new_v4()
        ));
        let options =
            default_supervisor_options(PathBuf::from("/tmp"), Some(runtime_dir.clone()), None);
        let supervisor = ProcessSupervisor::new(host.clone(), options);

        assert!(
            !marker.exists(),
            "sanity check: the marker must not exist before start()"
        );
        supervisor
            .start(&"prepped-server".to_string(), None)
            .await
            .expect("real service with a real preparation command should start and become ready");
        assert!(
            marker.exists(),
            "the real preparation command should have run before the service started"
        );
        assert_eq!(
            host.service_states()
                .into_iter()
                .find(|s| s.service_id == "prepped-server")
                .unwrap()
                .actual_state,
            ActualServiceState::Ready
        );

        supervisor
            .stop(&"prepped-server".to_string(), None)
            .await
            .expect("stop should succeed");
        let _ = std::fs::remove_dir_all(&runtime_dir);
    }

    /// Real end-to-end test of the container log follower: `docker run` a container that prints a
    /// marker line, attach `tail_container_logs`, and the marker must arrive through `on_output` —
    /// the same path `attach_output` wires for a `Docker` identity. Needs a real `docker` daemon,
    /// like the compose test above.
    #[tokio::test]
    async fn tail_container_logs_forwards_a_real_containers_output() {
        let name = format!("hearth-core-logtail-{}", uuid::Uuid::new_v4().simple());
        let marker = format!("logtail-marker-{}", uuid::Uuid::new_v4().simple());
        let run = std::process::Command::new("docker")
            .args([
                "run",
                "-d",
                "--name",
                &name,
                "alpine:latest",
                "sh",
                "-c",
                &format!("echo {marker}; sleep 300"),
            ])
            .output();
        let cleanup = || {
            let _ = std::process::Command::new("docker")
                .args(["rm", "-f", &name])
                .output();
        };
        match run {
            Ok(output) if output.status.success() => {}
            other => {
                cleanup();
                panic!("docker run must succeed for this test: {other:?}");
            }
        }
        let started_at = match container_record(&name, "test").await {
            Ok(Some(record)) => record.container_started_at,
            _ => {
                cleanup();
                panic!("container {name} must be inspectable after docker run");
            }
        };

        let captured: Arc<std::sync::Mutex<String>> =
            Arc::new(std::sync::Mutex::new(String::new()));
        let captured_for_cb = captured.clone();
        let on_output: OnOutput =
            Arc::new(move |data: &str| captured_for_cb.lock().unwrap().push_str(data));
        let stop = tail_container_logs(
            name.clone(),
            Some(started_at),
            None,
            std::env::vars().collect(),
            on_output,
        );

        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        while std::time::Instant::now() < deadline {
            if captured.lock().unwrap().contains(&marker) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        stop.stop();
        cleanup();
        assert!(
            captured.lock().unwrap().contains(&marker),
            "container stdout must flow through the tail: {:?}",
            captured.lock().unwrap()
        );
    }

    /// Real (read-only) test of `tailnet_serving`/`DefaultProbeAdapter::tailnet` against whatever
    /// `tailscale serve` state actually exists on the test machine — the Tailnet half of the gap
    /// noted above `real_process_supervisor_starts_and_stops_a_real_docker_compose_service`.
    /// Deliberately does not call `tailscale serve` to add or remove anything (that would mutate a
    /// real, possibly shared Tailscale configuration outside this test's control) — instead it
    /// re-derives the same "Web" non-empty check independently and asserts the production function
    /// agrees with it, so the assertion is grounded in live system state rather than a hardcoded
    /// `true`/`false` that would silently stop meaning anything if the machine's Tailscale
    /// configuration ever changes.
    #[tokio::test]
    async fn tailnet_serving_agrees_with_the_real_tailscale_serve_status() {
        let output = match std::process::Command::new("tailscale")
            .args(["serve", "status", "--json"])
            .output()
        {
            Ok(output) => output,
            Err(error) => {
                panic!("`tailscale` must be installed and on PATH to run this test: {error}")
            }
        };
        let expected = output.status.success()
            && serde_json::from_slice::<serde_json::Value>(&output.stdout)
                .ok()
                .and_then(|v| {
                    v.get("Web")
                        .and_then(|w| w.as_object())
                        .map(|o| !o.is_empty())
                })
                .unwrap_or(false);
        assert_eq!(tailnet_serving().await, expected);
    }

    /// The kill prompt shows `PortHolder.command`; it used to carry the fingerprint — a 64-char
    /// sha256 — so the user was asked to kill `pid N (3fa9…)` with no idea what that was.
    #[tokio::test]
    async fn port_holders_name_the_raw_command_line_not_its_fingerprint() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let holders = port_holders(port).await.expect("lsof must be available");
        let me = holders
            .iter()
            .find(|h| h.pid == std::process::id() as i64)
            .expect("this test process holds the port");
        assert!(!me.command.is_empty());
        assert!(
            !(me.command.len() == 64 && me.command.chars().all(|c| c.is_ascii_hexdigit())),
            "a fingerprint hash, not a command line: {}",
            me.command
        );
        drop(listener);
    }

    #[tokio::test]
    async fn live_start_identities_include_this_process() {
        let alive = system_live_start_identities()
            .await
            .expect("ps must be readable");
        assert!(alive.contains_key(&(std::process::id() as i64)));
    }

    #[test]
    fn a_chunk_never_ends_inside_a_multi_byte_character() {
        let text = "ab\u{00e9}".as_bytes(); // `é` is two bytes
        assert_eq!(utf8_complete_prefix(text), text.len());
        assert_eq!(
            utf8_complete_prefix(&text[..3]),
            2,
            "a trailing partial character is held back"
        );
    }

    #[test]
    fn draining_a_small_raw_log_forwards_it_without_truncating() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("svc.raw");
        std::fs::write(&path, "line one\nline two\n").unwrap();
        let captured: Arc<std::sync::Mutex<String>> =
            Arc::new(std::sync::Mutex::new(String::new()));
        let sink = captured.clone();
        let on_output: OnOutput = Arc::new(move |data: &str| sink.lock().unwrap().push_str(data));
        let offset = AtomicU64::new(0);
        drain_raw_log_once(&path, &offset, &on_output);
        assert_eq!(*captured.lock().unwrap(), "line one\nline two\n");
        assert_eq!(
            std::fs::metadata(&path).unwrap().len(),
            18,
            "below the threshold the file is left alone"
        );
        assert_eq!(offset.load(Ordering::SeqCst), 18);
        drain_raw_log_once(&path, &offset, &on_output);
        assert_eq!(
            *captured.lock().unwrap(),
            "line one\nline two\n",
            "nothing is forwarded twice"
        );
    }

    fn record(pid: i64, fingerprint: &str) -> PosixProcessRecord {
        PosixProcessRecord {
            pid,
            pgid: pid,
            start_identity: "Mon Jan  1 00:00:00 2026".to_string(),
            command_fingerprint: fingerprint.to_string(),
        }
    }

    /// Regression, stated as the decision rather than as a race: right after spawning, the child
    /// can still be mid-`execve`, and macOS `ps` then reports a placeholder (`(sh)`) rather than
    /// the real command line. Accepting that first readable row as the identity's fingerprint
    /// poisoned it permanently — once exec completed, `ps` reported the real command,
    /// `owns_identity` compared unequal, and the daemon orphaned the service it had just started.
    ///
    /// Driven directly because the race itself is not reliably reproducible from Rust: spawning
    /// `ps` through `tokio::process` is slow enough that exec has almost always completed by the
    /// first poll. The TypeScript port, whose `ps` call is synchronous and lands sooner, hits the
    /// window ~15% of the time and has an end-to-end test for it.
    #[test]
    fn a_lone_unexpected_observation_is_not_accepted_as_the_fingerprint() {
        let placeholder = record(42, "fingerprint-of-(sh)");
        assert!(
            !accepts_spawn_observation(&placeholder, "expected-fingerprint", None),
            "a first observation that does not match what we spawned must not be trusted"
        );
    }

    #[test]
    fn an_observation_matching_what_we_spawned_is_accepted_immediately() {
        let observed = record(42, "expected-fingerprint");
        assert!(accepts_spawn_observation(
            &observed,
            "expected-fingerprint",
            None
        ));
    }

    /// An argv command legitimately observes differently from the logical fingerprint (`ps` reports
    /// the resolved binary path where `argv[0]` was a bare name), so a genuinely stable mismatch
    /// has to be accepted — otherwise those services could never start.
    #[test]
    fn a_stable_mismatch_is_accepted_after_repeating_identically() {
        let first = record(42, "resolved-path-fingerprint");
        let second = record(42, "resolved-path-fingerprint");
        assert!(!accepts_spawn_observation(
            &first,
            "expected-fingerprint",
            None
        ));
        assert!(accepts_spawn_observation(
            &second,
            "expected-fingerprint",
            Some(&first)
        ));
    }

    /// Two different readings are not stability — this is what separates a settled argv path from
    /// a placeholder that is about to change.
    #[test]
    fn a_changing_observation_is_not_accepted() {
        let first = record(42, "fingerprint-of-(sh)");
        let second = record(42, "some-other-fingerprint");
        assert!(!accepts_spawn_observation(
            &second,
            "expected-fingerprint",
            Some(&first)
        ));
    }

    /// A reused pid must not let a stale reading vouch for a new process.
    #[test]
    fn a_different_process_does_not_count_as_a_repeat() {
        let first = record(42, "same-fingerprint");
        let mut second = record(42, "same-fingerprint");
        second.start_identity = "Tue Jan  2 00:00:00 2026".to_string();
        assert!(!accepts_spawn_observation(
            &second,
            "expected-fingerprint",
            Some(&first)
        ));
    }
}
