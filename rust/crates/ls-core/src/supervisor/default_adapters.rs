//! Default (production) adapters — port of the bottom of `src/core/supervisor.ts` (everything
//! below the `ProcessSupervisor` class): the real `ps`/`docker`/`tailscale` shell-outs and
//! `tokio::process::Command`-based spawning that make a `ProcessSupervisor` actually run real
//! processes and containers, as opposed to the fakes the engine's own unit tests use.
//!
//! Known simplification vs. the TS source: `forward_stream` decodes each read chunk with
//! `String::from_utf8_lossy` independently rather than a stateful streaming UTF-8 decoder (TS's
//! `TextDecoder(..., {stream:true})`) — a multi-byte UTF-8 character split exactly across a chunk
//! boundary can render as a replacement character in forwarded log output. Cosmetic only (log
//! text, not a wire protocol), not revisited here.
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
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

use super::fingerprint::{command_argv, normalize_observed_command_fingerprint};
use super::types::{
    DockerContainerRecord, ManagedProcess, ObservedProcess, OnOutput, PosixProcessRecord, ProbeAdapter,
    ProcessAdapter, ProcessRecord, ProcessSignal, RunBuild, SpawnInput, SupervisorClock, SupervisorError,
    SupervisorOptions, SystemClock,
};

const RAW_LOG_POLL_MS: u64 = 200;
const EXEC_IDENTITY_SETTLE_MS: u64 = 1_000;

fn to_nix_signal(signal: ProcessSignal) -> nix::sys::signal::Signal {
    match signal {
        ProcessSignal::Sigterm => nix::sys::signal::Signal::SIGTERM,
        ProcessSignal::Sigkill => nix::sys::signal::Signal::SIGKILL,
    }
}

fn send_signal(pid: i64, signal: ProcessSignal) {
    let _ = nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid as i32), to_nix_signal(signal));
}

/// A negative pid is POSIX shorthand for "the whole process group" — mirrors `process.kill(-pgid, sig)`.
fn send_signal_to_group(pgid: i64, signal: ProcessSignal) {
    let _ = nix::sys::signal::kill(nix::unistd::Pid::from_raw(-(pgid as i32)), to_nix_signal(signal));
}

async fn capture_command(argv: &[&str]) -> (i32, String) {
    if argv.is_empty() {
        return (-1, String::new());
    }
    match Command::new(argv[0]).args(&argv[1..]).output().await {
        Ok(output) => (output.status.code().unwrap_or(-1), String::from_utf8_lossy(&output.stdout).to_string()),
        Err(_) => (-1, String::new()),
    }
}

fn ps_observed_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^(\d+)\s+(\d+)\s+(.{24})\s+(.+)$").unwrap())
}

async fn observed_system_process(pid: i64) -> Option<ObservedProcess> {
    let pid_str = pid.to_string();
    let (code, stdout) = capture_command(&["ps", "-o", "pid=", "-o", "pgid=", "-o", "lstart=", "-o", "command=", "-p", &pid_str]).await;
    if code != 0 {
        return None;
    }
    let caps = ps_observed_re().captures(stdout.trim())?;
    let observed_pid: i64 = caps[1].parse().ok()?;
    let pgid: i64 = caps[2].parse().ok()?;
    let start_identity = caps[3].trim().to_string();
    let command_fingerprint = normalize_observed_command_fingerprint(&caps[4]);
    Some(ObservedProcess { record: ProcessRecord::Posix(PosixProcessRecord { pid: observed_pid, pgid, start_identity, command_fingerprint }), alive: true })
}

async fn process_inspection_available() -> bool {
    let pid = std::process::id().to_string();
    capture_command(&["ps", "-o", "pid=", "-p", &pid]).await.0 == 0
}

async fn container_running(container_name: &str) -> bool {
    capture_command(&["docker", "inspect", "-f", "{{.State.Running}}", container_name]).await.1.trim() == "true"
}

async fn container_record(container_name: &str, command_fingerprint: &str) -> Option<DockerContainerRecord> {
    let (_, stdout) = capture_command(&["docker", "inspect", "-f", "{{.Id}}\t{{.State.Running}}\t{{.State.StartedAt}}", container_name]).await;
    let parts: Vec<&str> = stdout.trim().split('\t').collect();
    if parts.len() == 3 && parts[1] == "true" && !parts[0].is_empty() && !parts[2].is_empty() {
        Some(DockerContainerRecord {
            container_name: container_name.to_string(),
            container_id: parts[0].to_string(),
            container_started_at: parts[2].to_string(),
            command_fingerprint: command_fingerprint.to_string(),
        })
    } else {
        None
    }
}

fn same_container_instance(expected: &DockerContainerRecord, observed: Option<&DockerContainerRecord>) -> bool {
    observed
        .map(|o| o.container_name == expected.container_name && o.container_id == expected.container_id && o.container_started_at == expected.container_started_at)
        .unwrap_or(false)
}

async fn tailnet_serving() -> bool {
    let (code, stdout) = capture_command(&["tailscale", "serve", "status", "--json"]).await;
    if code != 0 {
        return false;
    }
    serde_json::from_str::<serde_json::Value>(&stdout)
        .ok()
        .and_then(|v| v.get("Web").and_then(|w| w.as_object()).map(|o| !o.is_empty()))
        .unwrap_or(false)
}

async fn tcp_probe(port: u16) -> bool {
    let addr = format!("127.0.0.1:{port}");
    tokio::time::timeout(Duration::from_millis(250), tokio::net::TcpStream::connect(&addr)).await.map(|r| r.is_ok()).unwrap_or(false)
}

async fn forward_stream<R: tokio::io::AsyncRead + Unpin>(stream: Option<R>, on_output: Option<OnOutput>) {
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

/// Spawns `argv` (cwd/env applied, own process group), forwards both stdout and stderr to
/// `on_output`, and returns its exit code (`-1` if it couldn't even be spawned) — port of
/// `runCommand`.
async fn run_command(argv: &[String], cwd: &Path, env: &HashMap<String, String>, on_output: Option<OnOutput>) -> i32 {
    if argv.is_empty() {
        return -1;
    }
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..]).current_dir(cwd).env_clear().envs(env).stdout(Stdio::piped()).stderr(Stdio::piped()).process_group(0);
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(_) => return -1,
    };
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let stdout_task = tokio::spawn(forward_stream(stdout, on_output.clone()));
    let stderr_task = tokio::spawn(forward_stream(stderr, on_output));
    let status = child.wait().await;
    let _ = stdout_task.await;
    let _ = stderr_task.await;
    status.ok().and_then(|s| s.code()).unwrap_or(-1)
}

async fn stop_unverified_child(child: &mut tokio::process::Child) {
    if let Some(pid) = child.id() {
        send_signal(pid as i64, ProcessSignal::Sigterm);
    }
    let _ = tokio::time::timeout(Duration::from_millis(500), child.wait()).await;
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
pub(crate) fn accepts_spawn_observation(current: &PosixProcessRecord, expected_fingerprint: &str, previous: Option<&PosixProcessRecord>) -> bool {
    if current.command_fingerprint == expected_fingerprint {
        return true;
    }
    match previous {
        Some(previous) => {
            previous.pid == current.pid && previous.pgid == current.pgid && previous.start_identity == current.start_identity && previous.command_fingerprint == current.command_fingerprint
        }
        None => false,
    }
}


/// Exec fingerprint settling loop — port of `observedStableExecProcess`. A shell wrapper's own
/// fingerprint (the `sh -c ...` line as `ps` shows it *before* exec) differs from the execed
/// program's; this polls until it observes the SAME non-wrapper fingerprint twice in a row, with a
/// settle-check delay between, before trusting it.
async fn observed_stable_exec_process(child: &mut tokio::process::Child, expected_fingerprint: &str) -> Result<PosixProcessRecord, SupervisorError> {
    let pid = child.id().ok_or_else(|| SupervisorError("child has no pid".to_string()))? as i64;
    let mut candidate: Option<PosixProcessRecord> = None;
    for _ in 0..20 {
        tokio::time::sleep(Duration::from_millis(25)).await;
        let observed = observed_system_process(pid).await;
        let posix = match observed {
            Some(ObservedProcess { record: ProcessRecord::Posix(r), .. }) => Some(r),
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
        let settled = observed_system_process(pid).await;
        if let Some(ObservedProcess { record: ProcessRecord::Posix(settled_rec), .. }) = settled {
            if Some(&settled_rec) == candidate.as_ref() {
                return Ok(settled_rec);
            }
        }
        candidate = None;
    }
    stop_unverified_child(child).await;
    Err(SupervisorError("Unable to establish stable POSIX exec process identity".to_string()))
}

fn drain_raw_log_once(path: &Path, offset: &AtomicU64, on_output: &OnOutput) {
    use std::io::{Read, Seek, SeekFrom};
    let _: std::io::Result<()> = (|| {
        let size = std::fs::metadata(path)?.len();
        let start = offset.load(Ordering::SeqCst);
        if size <= start {
            return Ok(());
        }
        let mut file = std::fs::File::open(path)?;
        file.seek(SeekFrom::Start(start))?;
        let mut buf = vec![0u8; (size - start) as usize];
        file.read_exact(&mut buf)?;
        let text = String::from_utf8_lossy(&buf);
        if !text.is_empty() {
            on_output(&text);
        }
        drop(file);
        // copytruncate: shrink the capture file back to empty so a long-lived service's raw
        // output never grows unbounded between polls. The writer's fd is opened append-mode, so
        // its next write always lands at the (now shorter) current end of file.
        let write_file = std::fs::OpenOptions::new().write(true).open(path)?;
        // Only truncate if the file is still exactly the size we read. The child writes to this
        // file continuously and independently, so anything it appended between the `read_exact`
        // above and this call would be destroyed by a blind `set_len(0)` — silently dropping log
        // lines on every poll of a chatty service. When it has grown, leave the bytes alone and
        // just advance our read offset; whichever later poll catches the file quiescent truncates.
        if write_file.metadata()?.len() == size {
            write_file.set_len(0)?;
            offset.store(0, Ordering::SeqCst);
        } else {
            offset.store(size, Ordering::SeqCst);
        }
        Ok(())
    })();
}

/// Port of `tailFile` — polls a raw capture file every `RAW_LOG_POLL_MS`, forwarding new bytes and
/// truncating what it's read. Returns a stop handle (drains one last time, then stops polling).
fn tail_file(path: PathBuf, on_output: OnOutput) -> Box<dyn FnOnce() + Send> {
    let stopped = Arc::new(AtomicBool::new(false));
    let offset = Arc::new(AtomicU64::new(0));
    let task_path = path.clone();
    let task_output = on_output.clone();
    let task_offset = offset.clone();
    let task_stopped = stopped.clone();
    let handle = tokio::spawn(async move {
        while !task_stopped.load(Ordering::SeqCst) {
            drain_raw_log_once(&task_path, &task_offset, &task_output);
            tokio::time::sleep(Duration::from_millis(RAW_LOG_POLL_MS)).await;
        }
    });
    Box::new(move || {
        stopped.store(true, Ordering::SeqCst);
        drain_raw_log_once(&path, &offset, &on_output);
        handle.abort();
    })
}

fn merged_environment(base: &HashMap<String, String>, command_env: &Option<HashMap<String, String>>) -> HashMap<String, String> {
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
}

#[async_trait]
impl ProcessAdapter for DefaultProcessAdapter {
    async fn spawn(&self, input: SpawnInput, on_output: OnOutput) -> Result<ManagedProcess, SupervisorError> {
        let env = merged_environment(&self.base_environment, &input.command.environment);

        if is_container_command(&input.command) {
            let (argv, _) = command_argv(&input.command.command);
            let cwd = self.root.join(&input.command.cwd);
            let code = run_command(&argv, &cwd, &env, Some(on_output)).await;
            if code != 0 {
                return Err(SupervisorError(format!("Docker service command exited with {code}")));
            }
            let container_name = input.command.container_name.clone().unwrap();
            let record = container_record(&container_name, &input.command_fingerprint)
                .await
                .ok_or_else(|| SupervisorError(format!("Docker container {container_name} is not running after start")))?;
            let (tx, rx) = oneshot::channel();
            let poll_record = record.clone();
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    let current = container_record(&poll_record.container_name, &poll_record.command_fingerprint).await;
                    if !same_container_instance(&poll_record, current.as_ref()) {
                        let _ = tx.send(0);
                        return;
                    }
                }
            });
            return Ok(ManagedProcess { record: ProcessRecord::Docker(record), exited: rx });
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
        std::fs::OpenOptions::new().create(true).write(true).truncate(true).open(&raw).map_err(|e| SupervisorError(e.to_string()))?;
        let stdout_file = std::fs::OpenOptions::new().append(true).open(&raw).map_err(|e| SupervisorError(e.to_string()))?;
        let stderr_file = stdout_file.try_clone().map_err(|e| SupervisorError(e.to_string()))?;

        let cwd = self.root.join(&input.command.cwd);
        let mut cmd = Command::new(&argv[0]);
        cmd.args(&argv[1..])
            .current_dir(&cwd)
            .env_clear()
            .envs(&env)
            .stdout(Stdio::from(stdout_file))
            .stderr(Stdio::from(stderr_file))
            .process_group(0);
        let mut child = cmd.spawn().map_err(|e| SupervisorError(format!("Service process failed to start: {e}")))?;
        let pid = child.id().ok_or_else(|| SupervisorError("spawned child reported no pid".to_string()))? as i64;

        let record = if exec {
            observed_stable_exec_process(&mut child, &input.command_fingerprint).await?
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
                if let Some(ObservedProcess { record: ProcessRecord::Posix(current), .. }) = observed_system_process(pid).await {
                    if accepts_spawn_observation(&current, &input.command_fingerprint, previous.as_ref()) {
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
                    return Err(SupervisorError("Unable to establish POSIX process ownership identity after 8 inspections".to_string()));
                }
            }
        };

        let (tx, rx) = oneshot::channel();
        tokio::spawn(async move {
            let status = child.wait().await;
            let code = status.ok().and_then(|s| s.code()).unwrap_or(-1);
            let _ = tx.send(code);
        });
        Ok(ManagedProcess { record: ProcessRecord::Posix(record), exited: rx })
    }

    async fn inspect(&self, identity: &ProcessIdentity) -> Option<ObservedProcess> {
        match identity {
            ProcessIdentity::Docker(id) => {
                let record = container_record(&id.container_name, &id.command_fingerprint).await?;
                let expected = DockerContainerRecord {
                    container_name: id.container_name.clone(),
                    container_id: id.container_id.clone(),
                    container_started_at: id.container_started_at.clone(),
                    command_fingerprint: id.command_fingerprint.clone(),
                };
                same_container_instance(&expected, Some(&record)).then_some(ObservedProcess { record: ProcessRecord::Docker(record), alive: true })
            }
            ProcessIdentity::Posix(id) => {
                if id.pid == 0 {
                    return Some(ObservedProcess {
                        record: ProcessRecord::Posix(PosixProcessRecord { pid: 0, pgid: 0, start_identity: String::new(), command_fingerprint: id.command_fingerprint.clone() }),
                        alive: false,
                    });
                }
                observed_system_process(id.pid).await
            }
        }
    }

    async fn signal_group(&self, pgid: i64, signal: ProcessSignal) {
        if pgid == 0 {
            return;
        }
        send_signal_to_group(pgid, signal);
    }

    async fn stop_container(&self, command: &ServiceCommand, on_output: OnOutput) -> Option<Result<(), SupervisorError>> {
        let default_stop = CommandSpec::Argv { argv: vec!["docker".to_string(), "compose".to_string(), "stop".to_string()] };
        let (argv, _) = command_argv(command.docker_stop_command.as_ref().unwrap_or(&default_stop));
        let code = run_command(&argv, &self.root, &self.base_environment, Some(on_output)).await;
        Some(if code != 0 { Err(SupervisorError(format!("Docker service stop exited with {code}"))) } else { Ok(()) })
    }

    fn attach_output(&self, service_id: &ServiceId, on_output: OnOutput) -> Option<Box<dyn FnOnce() + Send>> {
        Some(tail_file(raw_log_path(&self.runtime_directory, service_id), on_output))
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
        match tokio::time::timeout(Duration::from_millis(250), self.http_client.get(url).send()).await {
            Ok(Ok(response)) => response.status().is_success(),
            _ => false,
        }
    }
    async fn container(&self, container_name: &str) -> bool {
        container_running(container_name).await
    }
    async fn tailnet(&self) -> bool {
        tailnet_serving().await
    }
    async fn port_in_use(&self, port: u16) -> Option<bool> {
        Some(tcp_probe(port).await)
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
    async fn run(&self, command: &ServiceCommand, on_output: OnOutput, cancel: CancellationToken) -> Result<(), SupervisorError> {
        if cancel.is_cancelled() {
            return Err(SupervisorError("Build cancelled".to_string()));
        }
        let (argv, _) = command_argv(&command.command);
        let env = merged_environment(&self.base_environment, &command.environment);
        let cwd = self.root.join(&command.cwd);
        let mut cmd = Command::new(&argv[0]);
        cmd.args(&argv[1..]).current_dir(&cwd).env_clear().envs(&env).stdout(Stdio::piped()).stderr(Stdio::piped()).process_group(0);
        let mut child = cmd.spawn().map_err(|e| SupervisorError(format!("Build command failed to start: {e}")))?;
        let pid = child.id().map(|p| p as i64);
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let stdout_task = tokio::spawn(forward_stream(stdout, Some(on_output.clone())));
        let stderr_task = tokio::spawn(forward_stream(stderr, Some(on_output)));

        tokio::select! {
            status = child.wait() => {
                let _ = stdout_task.await;
                let _ = stderr_task.await;
                let code = status.ok().and_then(|s| s.code()).unwrap_or(-1);
                if code != 0 { Err(SupervisorError(format!("Build command exited with {code}"))) } else { Ok(()) }
            }
            _ = cancel.cancelled() => {
                if let Some(pid) = pid { send_signal_to_group(pid, ProcessSignal::Sigterm); }
                let _ = tokio::time::timeout(Duration::from_secs(5), child.wait()).await;
                if let Some(pid) = pid { send_signal_to_group(pid, ProcessSignal::Sigkill); }
                let _ = child.wait().await;
                Err(SupervisorError("Build cancelled".to_string()))
            }
        }
    }
}

/// `base_environment` should come from `crate::env::resolve_base_environment` for a daemon that
/// might be launched from a GUI (bare `PATH`, no login-shell customization) — defaults to the
/// current process's own environment, i.e. whatever spawned the daemon, matching the TS default.
pub fn default_supervisor_options(root: PathBuf, runtime_directory: Option<PathBuf>, base_environment: Option<HashMap<String, String>>) -> SupervisorOptions {
    let runtime_directory = runtime_directory.unwrap_or_else(|| resolve_runtime_directory(&root, None));
    let base_environment = base_environment.unwrap_or_else(|| std::env::vars().collect());
    let http_client = reqwest::Client::new();
    let clock: Arc<dyn SupervisorClock> = Arc::new(SystemClock);
    SupervisorOptions {
        process: Arc::new(DefaultProcessAdapter { root: root.clone(), runtime_directory, base_environment: base_environment.clone() }),
        run_build: Arc::new(DefaultRunBuild { root: root.clone(), base_environment: base_environment.clone() }),
        probes: Arc::new(DefaultProbeAdapter { root, base_environment, http_client }),
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
    use crate::supervisor::ProcessSupervisor;
    use std::collections::HashMap as Map;

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
        let mut child = Command::new("sh").args(["-c", "sleep 5"]).process_group(0).spawn().unwrap();
        let pid = child.id().unwrap() as i64;
        tokio::time::sleep(Duration::from_millis(50)).await;
        let observed = observed_system_process(pid).await;
        match observed {
            Some(ObservedProcess { record: ProcessRecord::Posix(rec), alive }) => {
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
        assert!(observed_system_process(i32::MAX as i64).await.is_none());
    }

    #[tokio::test]
    async fn run_command_captures_exit_code_and_forwards_output() {
        let output: Arc<std::sync::Mutex<String>> = Arc::new(std::sync::Mutex::new(String::new()));
        let output_for_cb = output.clone();
        let on_output: OnOutput = Arc::new(move |data: &str| output_for_cb.lock().unwrap().push_str(data));
        let code = run_command(&["sh".to_string(), "-c".to_string(), "echo hello-from-run-command".to_string()], Path::new("/tmp"), &Map::new(), Some(on_output)).await;
        assert_eq!(code, 0);
        assert!(output.lock().unwrap().contains("hello-from-run-command"));
    }

    #[tokio::test]
    async fn run_command_reports_a_nonzero_exit_code() {
        let code = run_command(&["sh".to_string(), "-c".to_string(), "exit 7".to_string()], Path::new("/tmp"), &Map::new(), None).await;
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
            CommandSpec as Spec, ReadinessSpec, ServiceCatalog, ServiceCommand as Cmd, ServiceDefinition, ServiceKind,
            ServiceProfiles, ServiceRunProfile, StartFailurePolicy,
        };
        use crate::state::{ActualServiceState, DesiredServiceState, ServiceLifecycleState, ServiceReadiness};
        use crate::supervisor::Host;
        use async_trait::async_trait;
        use std::sync::Mutex as StdMutex;

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

        struct TestHost {
            catalog: Arc<ServiceCatalog>,
            states: StdMutex<Map<String, ServiceLifecycleState>>,
        }
        #[async_trait]
        impl Host for TestHost {
            fn instance_id(&self) -> String {
                "real-adapter-test".to_string()
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
                self.states.lock().unwrap().insert(next.service_id.clone(), next);
            }
            async fn append_log(&self, _service_id: &str, _data: &str) {}
            fn publish(&self, _event_type: &str, _data: serde_json::Value) {}
        }

        let service = ServiceDefinition {
            id: "nc-server".to_string(),
            label: None,
            kind: Some(ServiceKind::Application),
            ownership: None,
            dependencies: None,
            profiles: ServiceProfiles {
                run: ServiceRunProfile::Verified {
                    command: Cmd {
                        command: Spec::Shell { shell: format!("exec nc -lk {port}"), exec: Some(true) },
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
        };
        let catalog = ServiceCatalog {
            services: vec![service],
            groups: Map::new(),
            compose_file: None,
            runtime_directory: None,
            start_failure_policy: StartFailurePolicy::StopOnFirstFailureKeepStarted,
            private_file_guard: None,
        };
        let host = Arc::new(TestHost { catalog: Arc::new(catalog), states: StdMutex::new(Map::new()) });

        let runtime_dir = std::env::temp_dir().join(format!("ls-core-real-adapter-test-{}", uuid::Uuid::new_v4()));
        let options = default_supervisor_options(PathBuf::from("/tmp"), Some(runtime_dir.clone()), None);
        let supervisor = ProcessSupervisor::new(host.clone(), options);

        supervisor.start(&"nc-server".to_string(), None).await.expect("real nc-backed service should start and become ready");
        let state = host.service_states().into_iter().find(|s| s.service_id == "nc-server").unwrap();
        assert_eq!(state.actual_state, ActualServiceState::Ready);
        let pid = match state.identity.unwrap() {
            ProcessIdentity::Posix(p) => p.pid,
            _ => panic!("expected a posix identity"),
        };
        assert!(nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid as i32), None).is_ok(), "the real OS process should be alive");

        supervisor.stop(&"nc-server".to_string(), None).await.expect("stop should succeed");
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
            CommandSpec as Spec, ReadinessSpec, ServiceCatalog, ServiceCommand as Cmd, ServiceDefinition, ServiceKind,
            ServiceProfiles, ServiceRunProfile, StartFailurePolicy,
        };
        use crate::state::{ActualServiceState, DesiredServiceState, ServiceLifecycleState, ServiceReadiness};
        use crate::supervisor::Host;
        use async_trait::async_trait;
        use std::sync::Mutex as StdMutex;

        struct TestHost {
            catalog: Arc<ServiceCatalog>,
            states: StdMutex<Map<String, ServiceLifecycleState>>,
        }
        #[async_trait]
        impl Host for TestHost {
            fn instance_id(&self) -> String {
                "real-docker-adapter-test".to_string()
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
                self.states.lock().unwrap().insert(next.service_id.clone(), next);
            }
            async fn append_log(&self, _service_id: &str, _data: &str) {}
            fn publish(&self, _event_type: &str, _data: serde_json::Value) {}
        }

        let dir = tempfile::tempdir().unwrap();
        let compose_path = dir.path().join("docker-compose.yml");
        std::fs::write(&compose_path, "services:\n  app:\n    image: alpine:latest\n    command: [\"sleep\", \"3600\"]\n").unwrap();
        // Unique per test run so concurrent/repeated runs never collide on a project name, and
        // lowercase-hex-only so it's always a valid Compose project name.
        let project = format!("ls-core-test-{}", uuid::Uuid::new_v4().simple());
        let container_name = format!("{project}-app-1");
        let compose_path_str = compose_path.to_string_lossy().to_string();

        let service = ServiceDefinition {
            id: "docker-app".to_string(),
            label: None,
            kind: Some(ServiceKind::Application),
            ownership: None,
            dependencies: None,
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
        };
        let catalog = ServiceCatalog {
            services: vec![service],
            groups: Map::new(),
            compose_file: None,
            runtime_directory: None,
            start_failure_policy: StartFailurePolicy::StopOnFirstFailureKeepStarted,
            private_file_guard: None,
        };
        let host = Arc::new(TestHost { catalog: Arc::new(catalog), states: StdMutex::new(Map::new()) });

        let runtime_dir = std::env::temp_dir().join(format!("ls-core-real-docker-adapter-test-{}", uuid::Uuid::new_v4()));
        let options = default_supervisor_options(dir.path().to_path_buf(), Some(runtime_dir.clone()), None);
        let supervisor = ProcessSupervisor::new(host.clone(), options);

        let start_result = supervisor.start(&"docker-app".to_string(), None).await;
        // Always clean up the compose project, whether start succeeded or not, so a failed
        // assertion never leaves a real container running on the test machine.
        let cleanup = || {
            let _ = std::process::Command::new("docker").args(["compose", "-p", &project, "-f", &compose_path_str, "down", "--timeout", "1"]).output();
        };
        if let Err(error) = &start_result {
            cleanup();
            let _ = std::fs::remove_dir_all(&runtime_dir);
            panic!("real docker-backed service should start and become ready: {error:?}");
        }

        let state = host.service_states().into_iter().find(|s| s.service_id == "docker-app").unwrap();
        assert_eq!(state.actual_state, ActualServiceState::Ready);
        match &state.identity {
            Some(ProcessIdentity::Docker(id)) => assert_eq!(id.container_name, container_name),
            other => panic!("expected a docker identity, got {other:?}"),
        }
        assert!(container_running(&container_name).await, "the real container should be running per `docker inspect`");

        let stop_result = supervisor.stop(&"docker-app".to_string(), None).await;
        cleanup();
        let _ = std::fs::remove_dir_all(&runtime_dir);
        stop_result.expect("stop should succeed");
        assert!(!container_running(&container_name).await, "the real container must be stopped/removed after stop()");
    }

    /// Real end-to-end test of the declarative `preparation_command` (the JSON-serializable
    /// stand-in for a bespoke `PreparationAdapter` added to unblock a `.config.ts`/YAML-only
    /// consumer whose real services depend on a prepare step — see AGENTS.md's Rust-rewrite status
    /// for the `viclass` cutover this was built to unblock). Proves the whole real chain: catalog
    /// declares a command, the engine calls the real `DefaultProbeAdapter::command` (the same
    /// adapter method `ReadinessSpec::Command` readiness already exercises elsewhere), which really
    /// spawns a shell command — here, one that writes a marker file — *before* the service's own
    /// run command starts.
    #[tokio::test]
    async fn real_process_supervisor_runs_a_real_preparation_command_before_starting_the_service() {
        use crate::catalog::{
            CommandSpec as Spec, PreparationCommand, ReadinessSpec, ServiceCatalog, ServiceCommand as Cmd, ServiceDefinition, ServiceKind, ServiceProfiles,
            ServiceRunProfile, StartFailurePolicy,
        };
        use crate::state::{ActualServiceState, DesiredServiceState, ServiceLifecycleState, ServiceReadiness};
        use crate::supervisor::Host;
        use async_trait::async_trait;
        use std::sync::Mutex as StdMutex;

        struct TestHost {
            catalog: Arc<ServiceCatalog>,
            states: StdMutex<Map<String, ServiceLifecycleState>>,
        }
        #[async_trait]
        impl Host for TestHost {
            fn instance_id(&self) -> String {
                "real-preparation-command-test".to_string()
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
                self.states.lock().unwrap().insert(next.service_id.clone(), next);
            }
            async fn append_log(&self, _service_id: &str, _data: &str) {}
            fn publish(&self, _event_type: &str, _data: serde_json::Value) {}
        }

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
            dependencies: None,
            profiles: ServiceProfiles {
                run: ServiceRunProfile::Verified {
                    command: Cmd { command: Spec::Shell { shell: format!("exec nc -lk {port}"), exec: Some(true) }, cwd: "/tmp".to_string(), environment: None, container_name: None, docker_stop_command: None },
                    readiness: ReadinessSpec::Tcp { port },
                    readiness_timeout_ms: Some(5_000),
                    preparation: None,
                    preparation_command: Some(PreparationCommand { command: Spec::Shell { shell: format!("touch '{}'", marker.display()), exec: None }, cwd: None, serialization_key: None }),
                },
                build: None,
            },
            ports: None,
        };
        let catalog = ServiceCatalog { services: vec![service], groups: Map::new(), compose_file: None, runtime_directory: None, start_failure_policy: StartFailurePolicy::StopOnFirstFailureKeepStarted, private_file_guard: None };
        let host = Arc::new(TestHost { catalog: Arc::new(catalog), states: StdMutex::new(Map::new()) });

        let runtime_dir = std::env::temp_dir().join(format!("ls-core-real-preparation-command-test-{}", uuid::Uuid::new_v4()));
        let options = default_supervisor_options(PathBuf::from("/tmp"), Some(runtime_dir.clone()), None);
        let supervisor = ProcessSupervisor::new(host.clone(), options);

        assert!(!marker.exists(), "sanity check: the marker must not exist before start()");
        supervisor.start(&"prepped-server".to_string(), None).await.expect("real service with a real preparation command should start and become ready");
        assert!(marker.exists(), "the real preparation command should have run before the service started");
        assert_eq!(host.service_states().into_iter().find(|s| s.service_id == "prepped-server").unwrap().actual_state, ActualServiceState::Ready);

        supervisor.stop(&"prepped-server".to_string(), None).await.expect("stop should succeed");
        let _ = std::fs::remove_dir_all(&runtime_dir);
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
        let output = match std::process::Command::new("tailscale").args(["serve", "status", "--json"]).output() {
            Ok(output) => output,
            Err(error) => panic!("`tailscale` must be installed and on PATH to run this test: {error}"),
        };
        let expected = output.status.success()
            && serde_json::from_slice::<serde_json::Value>(&output.stdout).ok().and_then(|v| v.get("Web").and_then(|w| w.as_object()).map(|o| !o.is_empty())).unwrap_or(false);
        assert_eq!(tailnet_serving().await, expected);
    }

    fn record(pid: i64, fingerprint: &str) -> PosixProcessRecord {
        PosixProcessRecord { pid, pgid: pid, start_identity: "Mon Jan  1 00:00:00 2026".to_string(), command_fingerprint: fingerprint.to_string() }
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
        assert!(accepts_spawn_observation(&observed, "expected-fingerprint", None));
    }

    /// An argv command legitimately observes differently from the logical fingerprint (`ps` reports
    /// the resolved binary path where `argv[0]` was a bare name), so a genuinely stable mismatch
    /// has to be accepted — otherwise those services could never start.
    #[test]
    fn a_stable_mismatch_is_accepted_after_repeating_identically() {
        let first = record(42, "resolved-path-fingerprint");
        let second = record(42, "resolved-path-fingerprint");
        assert!(!accepts_spawn_observation(&first, "expected-fingerprint", None));
        assert!(accepts_spawn_observation(&second, "expected-fingerprint", Some(&first)));
    }

    /// Two different readings are not stability — this is what separates a settled argv path from
    /// a placeholder that is about to change.
    #[test]
    fn a_changing_observation_is_not_accepted() {
        let first = record(42, "fingerprint-of-(sh)");
        let second = record(42, "some-other-fingerprint");
        assert!(!accepts_spawn_observation(&second, "expected-fingerprint", Some(&first)));
    }

    /// A reused pid must not let a stale reading vouch for a new process.
    #[test]
    fn a_different_process_does_not_count_as_a_repeat() {
        let first = record(42, "same-fingerprint");
        let mut second = record(42, "same-fingerprint");
        second.start_identity = "Tue Jan  2 00:00:00 2026".to_string();
        assert!(!accepts_spawn_observation(&second, "expected-fingerprint", Some(&first)));
    }
}
