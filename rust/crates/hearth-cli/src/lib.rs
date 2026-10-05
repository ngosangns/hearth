//! The `hearth` CLI: a thin HTTP client for one daemon's loopback API, plain-text or JSON output,
//! hand-rolled flag parsing. The typed per-endpoint client lives in `client.rs`.
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

use regex::Regex;
use serde_json::{json, Value};

pub mod client;
pub mod shared;
pub mod update;

pub use client::ManagerClient;

use hearth_core::catalog::{ServiceCatalog, ServiceId};
use hearth_core::doctor::{run_doctor, DefaultDoctorAdapter, DoctorChecks, DoctorCommandCheck};
use hearth_core::file_io::{create_file_io, remove_directory};
use hearth_core::manager::{
    is_stale_lock_marker, read_lock_ownership_key, read_owned_lock_artifacts,
    verify_lock_ownership_proof,
};
use hearth_core::paths::resolve_runtime_directory;
use hearth_core::state::{
    ActualServiceState, ManagerMetadata, Operation, OperationStatus, ServiceLifecycleState,
    ServiceOperationKind, StaleLockMarker, PROTOCOL_VERSION,
};

pub const EXIT_USAGE: i32 = 2;
pub const EXIT_UNAVAILABLE: i32 = 3;
pub const EXIT_PROTOCOL: i32 = 4;
pub const EXIT_FAILED: i32 = 5;
pub const EXIT_UNAUTHORIZED: i32 = 7;

/// Bulk operations may occupy the manager's single request path briefly; this bounds only
/// transport, while `wait_operation` polls until terminal state.
pub const MANAGER_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone)]
pub struct Client {
    pub root: PathBuf,
    pub runtime_directory: PathBuf,
    pub metadata: ManagerMetadata,
    pub token: String,
}

#[derive(Debug, Clone)]
pub enum Discovery {
    Absent,
    Malformed,
    Incompatible { client: Client },
    Stale { client: Client },
    Live { client: Client },
}

#[derive(Debug, Clone)]
pub struct LocalctlError {
    pub exit_code: i32,
    pub message: String,
}
impl std::fmt::Display for LocalctlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}
impl std::error::Error for LocalctlError {}
pub type LocalctlResult<T> = Result<T, LocalctlError>;

pub(crate) fn usage_err<T>(message: impl Into<String>) -> LocalctlResult<T> {
    Err(LocalctlError {
        exit_code: EXIT_USAGE,
        message: message.into(),
    })
}
pub(crate) fn fail_err<T>(exit_code: i32, message: impl Into<String>) -> LocalctlResult<T> {
    Err(LocalctlError {
        exit_code,
        message: message.into(),
    })
}
fn unavailable_err<T>(message: impl std::fmt::Display) -> LocalctlResult<T> {
    fail_err(EXIT_UNAVAILABLE, message.to_string())
}

/// Spawns the daemon process, detached, given the repository root. The consumer supplies this
/// because only it knows where its own daemon entrypoint lives.
pub type SpawnDaemon = Box<dyn Fn(&Path) + Send + Sync>;

pub struct LocalctlOptions {
    pub catalog: ServiceCatalog,
    pub spawn_daemon: SpawnDaemon,
}

/// The host tools the daemon and the test suite shell out to (AGENTS.md "Build, test, release").
fn default_doctor_checks() -> DoctorChecks {
    fn on_path(name: &str) -> DoctorCommandCheck {
        DoctorCommandCheck {
            name: name.to_string(),
            command: "sh".to_string(),
            args: vec!["-c".to_string(), format!("command -v {name}")],
            ok: None,
            detail: None,
        }
    }
    DoctorChecks {
        commands: ["docker", "tailscale", "nc", "ps", "sh"]
            .iter()
            .map(|name| on_path(name))
            .collect(),
        paths: vec![],
        ports: vec![],
    }
}

// ---------------------------------------------------------------------------------------------
// Flag parsing
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FlagName {
    Json,
    Wait,
    Follow,
    Tail,
    Name,
    Dest,
    Key,
    KillUnowned,
    Force,
}

#[derive(Debug, Default, Clone)]
pub struct Flags {
    pub positionals: Vec<String>,
    pub json: bool,
    pub wait: bool,
    pub follow: bool,
    pub tail: Option<u64>,
    pub name: Option<String>,
    pub dest: Option<String>,
    pub key: Option<String>,
    pub kill_unowned: bool,
    pub force: bool,
}

pub fn parse_command_flags(arguments: &[String], allowed: &[FlagName]) -> LocalctlResult<Flags> {
    let mut result = Flags::default();
    let mut seen: HashSet<&'static str> = HashSet::new();
    let mut index = 0usize;
    while index < arguments.len() {
        let argument = &arguments[index];
        if !argument.starts_with("--") {
            result.positionals.push(argument.clone());
            index += 1;
            continue;
        }
        let name = &argument[2..];
        let (flag, canonical) = match name {
            "json" => (FlagName::Json, "json"),
            "wait" => (FlagName::Wait, "wait"),
            "follow" => (FlagName::Follow, "follow"),
            "tail" => (FlagName::Tail, "tail"),
            "name" => (FlagName::Name, "name"),
            "dest" => (FlagName::Dest, "dest"),
            "key" => (FlagName::Key, "key"),
            "kill-unowned" => (FlagName::KillUnowned, "kill-unowned"),
            "force" => (FlagName::Force, "force"),
            _ => return usage_err(format!("unknown flag: {argument}")),
        };
        if !allowed.contains(&flag) {
            return usage_err(format!("unsupported flag: --{canonical}"));
        }
        if !seen.insert(canonical) {
            return usage_err(format!("duplicate flag: --{canonical}"));
        }
        match flag {
            FlagName::Json => result.json = true,
            FlagName::Wait => result.wait = true,
            FlagName::Follow => result.follow = true,
            FlagName::Tail => {
                index += 1;
                let value = arguments.get(index).and_then(|v| v.parse::<i64>().ok());
                match value {
                    Some(v) if v >= 1 => result.tail = Some(v as u64),
                    _ => return usage_err("--tail must be a positive integer"),
                }
            }
            FlagName::Name => {
                index += 1;
                match arguments.get(index).filter(|v| !v.is_empty()) {
                    Some(value) => result.name = Some(value.clone()),
                    None => return usage_err("--name requires a value"),
                }
            }
            FlagName::Dest => {
                index += 1;
                match arguments.get(index).filter(|v| !v.is_empty()) {
                    Some(value) => result.dest = Some(value.clone()),
                    None => return usage_err("--dest requires a value"),
                }
            }
            FlagName::KillUnowned => result.kill_unowned = true,
            FlagName::Force => result.force = true,
            FlagName::Key => {
                index += 1;
                match arguments.get(index).filter(|v| !v.is_empty()) {
                    Some(value) => result.key = Some(value.clone()),
                    None => return usage_err("--key requires a value"),
                }
            }
        }
        index += 1;
    }
    Ok(result)
}

// ---------------------------------------------------------------------------------------------
// Discovery / HTTP
// ---------------------------------------------------------------------------------------------

fn valid_metadata(metadata: &ManagerMetadata) -> bool {
    metadata.version == 1
        && !metadata.instance_id.is_empty()
        && metadata.port > 0
        && metadata.pid > 0
        && !metadata.started_at.is_empty()
}

/// The one HTTP client every daemon request shares, so polls reuse pooled connections instead of
/// building a TLS-capable client per request. Timeouts are set per request, never here: the SSE
/// stream must stay open indefinitely.
pub(crate) fn http_client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(reqwest::Client::new)
}

/// Percent-encodes one URL path segment (anything outside RFC 3986's unreserved set), so a service
/// or operation id can never change the request path or add a query.
pub fn encode_path_segment(segment: &str) -> String {
    let mut encoded = String::with_capacity(segment.len());
    for byte in segment.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            encoded.push(byte as char);
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

/// Sends one request to the daemon at `client`, with the standard auth + protocol headers. A
/// transport failure is `"manager unavailable"` (or a timeout message); an HTTP error is
/// `"<code>:<message>"` — callers match on the `unauthorized` code.
pub async fn request(
    client: &Client,
    path: &str,
    method: reqwest::Method,
    body: Option<&Value>,
    protocol_version: Option<u32>,
) -> Result<Value, String> {
    request_with_timeout(
        client,
        path,
        method,
        body,
        protocol_version,
        Some(MANAGER_REQUEST_TIMEOUT),
    )
    .await
}

/// `request` with a caller-chosen transport timeout (`None` = unbounded). `shared attach`/`install`
/// can legitimately take minutes — a first-time install downloads and extracts a tarball — so the
/// shared CLI uses this rather than the 10s manager default.
pub async fn request_with_timeout(
    client: &Client,
    path: &str,
    method: reqwest::Method,
    body: Option<&Value>,
    protocol_version: Option<u32>,
    timeout: Option<Duration>,
) -> Result<Value, String> {
    let url = format!("http://127.0.0.1:{}{}", client.metadata.port, path);
    let protocol = protocol_version.unwrap_or(PROTOCOL_VERSION);
    let mut builder = http_client()
        .request(method, &url)
        .bearer_auth(&client.token)
        .header("x-hearth-protocol", protocol.to_string());
    if let Some(timeout) = timeout {
        builder = builder.timeout(timeout);
    }
    if let Some(body) = body {
        builder = builder.json(body);
    }
    let response = match builder.send().await {
        Ok(response) => response,
        Err(error) if error.is_timeout() => return Err("manager request timed out".to_string()),
        Err(_) => return Err("manager unavailable".to_string()),
    };
    let status = response.status();
    let body: Value = response.json().await.unwrap_or_else(|_| json!({}));
    if !status.is_success() {
        let code = body
            .get("error")
            .and_then(|e| e.get("code"))
            .and_then(Value::as_str)
            .unwrap_or("request_failed");
        let message = body
            .get("error")
            .and_then(|e| e.get("message"))
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| status.as_u16().to_string());
        return Err(format!("{code}:{message}"));
    }
    Ok(body)
}

pub async fn discover(root: &Path, catalog: &ServiceCatalog) -> Discovery {
    discover_probed(root, catalog).await.0
}

/// `discover`, plus whether the liveness probe was answered `unauthorized` — `Live` covers both, and
/// `require_client_for` needs the difference without probing the daemon a second time.
async fn discover_probed(root: &Path, catalog: &ServiceCatalog) -> (Discovery, bool) {
    let discovery = |d: Discovery| (d, false);
    let io = create_file_io(catalog.private_file_guard != Some(false));
    let runtime_directory = resolve_runtime_directory(root, catalog.runtime_directory.as_deref());
    let lock_directory = runtime_directory.join("manager.lock");
    if !io.is_private_directory(&runtime_directory) || !io.is_private_directory(&lock_directory) {
        return discovery(Discovery::Absent);
    }
    let raw_metadata = io
        .read_file(&lock_directory.join("metadata.json"))
        .ok()
        .flatten();
    let raw_token = io.read_file(&lock_directory.join("token")).ok().flatten();
    let (Some(raw_metadata), Some(raw_token)) = (raw_metadata, raw_token) else {
        return discovery(Discovery::Absent);
    };
    let Ok(metadata) = serde_json::from_str::<ManagerMetadata>(&raw_metadata) else {
        return discovery(Discovery::Malformed);
    };
    let token = raw_token.trim().to_string();
    if !valid_metadata(&metadata) || token.is_empty() {
        return discovery(Discovery::Malformed);
    }
    let Some(artifacts) = read_owned_lock_artifacts(io.as_ref(), &lock_directory) else {
        return discovery(Discovery::Malformed);
    };
    let Some(ownership_key) = read_lock_ownership_key(io.as_ref(), &runtime_directory) else {
        return discovery(Discovery::Malformed);
    };
    if !verify_lock_ownership_proof(
        Some(&ownership_key),
        &artifacts.metadata,
        &artifacts.token,
        &artifacts.proof,
    ) {
        return discovery(Discovery::Malformed);
    }
    let client = Client {
        root: root.to_path_buf(),
        runtime_directory,
        metadata: metadata.clone(),
        token,
    };
    if metadata.protocol_version != PROTOCOL_VERSION {
        return discovery(Discovery::Incompatible { client });
    }
    match request(&client, "/v1/manager", reqwest::Method::GET, None, None).await {
        Ok(_) => (Discovery::Live { client }, false),
        Err(e) if e.contains("unauthorized") => (Discovery::Live { client }, true),
        Err(_) => discovery(Discovery::Stale { client }),
    }
}

/// Discovers a live daemon, spawning one (via `options.spawn_daemon`) and polling if none is found.
pub async fn ensure(root: &Path, options: &LocalctlOptions) -> LocalctlResult<Client> {
    let discovered = discover(root, &options.catalog).await;
    match discovered {
        Discovery::Live { client } => return Ok(client),
        Discovery::Incompatible { .. } => {
            return fail_err(EXIT_PROTOCOL, "hearth manager protocol is incompatible")
        }
        // `/healthz` failed but the recorded pid is still alive. Spawning another daemon would
        // fight it for the lock; report unavailable and let the caller retry or restart.
        Discovery::Stale { client } if hearth_core::platform::is_pid_alive(client.metadata.pid) => {
            return fail_err(EXIT_UNAVAILABLE, "hearth manager is unavailable");
        }
        _ => {}
    }
    (options.spawn_daemon)(root);
    for _ in 0..100 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        let discovered = discover(root, &options.catalog).await;
        if let Discovery::Live { client } = discovered {
            return Ok(client);
        } else if let Discovery::Incompatible { .. } = discovered {
            return fail_err(EXIT_PROTOCOL, "hearth manager protocol is incompatible");
        }
    }
    fail_err(EXIT_UNAVAILABLE, "hearth manager is unavailable")
}

pub async fn require_client(root: &Path, options: &LocalctlOptions) -> LocalctlResult<Client> {
    require_client_for(root, &options.catalog).await
}

/// A live, authorized daemon for `root` — never spawns one. `discover` already probed
/// `/v1/manager`, so its answer decides liveness and auth without a second round trip.
pub async fn require_client_for(root: &Path, catalog: &ServiceCatalog) -> LocalctlResult<Client> {
    match discover_probed(root, catalog).await {
        (Discovery::Incompatible { .. }, _) => {
            fail_err(EXIT_PROTOCOL, "hearth manager protocol is incompatible")
        }
        (Discovery::Live { .. }, true) => {
            fail_err(EXIT_UNAUTHORIZED, "hearth manager authentication failed")
        }
        (Discovery::Live { client }, false) => Ok(client),
        _ => fail_err(EXIT_UNAVAILABLE, "hearth manager is unavailable"),
    }
}

/// The `manager ensure --json` payload: everything a generic HTTP+SSE client needs to talk to the
/// daemon directly — the bearer token and runtime directory
/// are no more exposed than the lock directory already is. `pub(crate)` because `hearth shared
/// ensure --json` prints the same contract for the smp daemon.
pub(crate) fn ensure_payload(client: &Client) -> Value {
    json!({
        "instanceId": client.metadata.instance_id,
        "port": client.metadata.port,
        "token": client.token,
        "protocolVersion": client.metadata.protocol_version,
        "runtimeDirectory": client.runtime_directory,
        "root": client.root,
    })
}

/// Restarts the daemon for `root`: shuts the running one down *without* stopping its services
/// (`leave-services` — daemon-owned processes are detached and outlive the daemon, and the next one
/// re-adopts them from their persisted identities), kills any other daemon process for this same
/// root, then ensures a fresh daemon. Returns the same payload `manager ensure --json` prints.
///
/// This is also the documented recovery path after a `PROTOCOL_VERSION` bump: the shutdown request
/// carries the *daemon's* own protocol version so an old daemon accepts it, and the daemon `ensure`
/// starts is this binary's.
pub async fn restart_manager(root: &Path, options: &LocalctlOptions) -> LocalctlResult<Value> {
    let mut recorded_pid = None;
    let mut shutdown_error = None;
    match discover(root, &options.catalog).await {
        Discovery::Live { client }
        | Discovery::Stale { client }
        | Discovery::Incompatible { client } => {
            recorded_pid = Some(client.metadata.pid);
            if let Err(error) = shut_down_for_restart(&client).await {
                shutdown_error = Some(error);
            }
        }
        _ => {}
    }
    // The lock records one pid. Another `hearth daemon --root <this>` (or `hearth smp` when
    // `root` is the shared root) can still be alive — a previous binary that never exited, or
    // a daemon that ignored the shutdown request. Kill those pids before starting a new one.
    if let Err(error) = hearth_core::supervisor::reap_duplicate_daemons(root).await {
        return fail_err(EXIT_FAILED, error);
    }
    if let Some(pid) = recorded_pid {
        let still_running = match hearth_core::supervisor::pid_is_live(pid).await {
            Ok(live) => live,
            Err(error) => return fail_err(EXIT_FAILED, error),
        };
        if pid != i64::from(std::process::id()) && still_running {
            return match shutdown_error {
                Some(error) => Err(error),
                None => fail_err(
                    EXIT_FAILED,
                    format!("hearth manager (pid {pid}) is still running"),
                ),
            };
        }
    }
    Ok(ensure_payload(&ensure(root, options).await?))
}

/// Shuts one running daemon down without stopping its services, and waits for the process to exit.
async fn shut_down_for_restart(client: &Client) -> LocalctlResult<()> {
    let pid = client.metadata.pid;
    let body = json!({ "requestId": uuid::Uuid::new_v4().to_string(), "mode": "leave-services" });
    match request(
        client,
        "/v1/manager/shutdown",
        reqwest::Method::POST,
        Some(&body),
        Some(client.metadata.protocol_version),
    )
    .await
    {
        Ok(_) => {}
        // A daemon from before `leave-services` existed rejects the mode outright. Its own SIGTERM
        // path is the same shutdown (`DaemonLifecycle` runs with `ShutdownMode::LeaveServices`), so signal
        // it instead: a daemon that predates this command is precisely the one `restart` exists to
        // replace, and refusing it would leave the user with no way to swap it.
        Err(error) if error.starts_with("invalid_shutdown_mode") => {
            if !hearth_core::platform::terminate_pid(pid) {
                return unavailable_err(error);
            }
        }
        // A recorded daemon whose pid is already gone is nothing to restart — the lock artifacts
        // simply outlived it, and `ensure` below starts a fresh one. Anything else (a live daemon
        // that refused, or never answered) is a real failure: falling through to `ensure` would hand
        // back the *same* daemon and report a restart that never happened.
        Err(_) if !hearth_core::platform::is_pid_alive(pid) => return Ok(()),
        Err(error) => return unavailable_err(error),
    }
    wait_for_daemon_exit(
        pid,
        MANAGER_STOP_TIMEOUT,
        hearth_core::platform::is_pid_alive,
    )
    .await
}

// ---------------------------------------------------------------------------------------------
// Targets / operation ids
// ---------------------------------------------------------------------------------------------

pub fn targets(catalog: &ServiceCatalog, target: Option<&str>) -> LocalctlResult<Vec<ServiceId>> {
    let Some(target) = target else {
        return Ok(catalog.services.iter().map(|s| s.id.clone()).collect());
    };
    if catalog.services.iter().any(|s| s.id == target) {
        return Ok(vec![target.to_string()]);
    }
    match catalog.groups.get(target) {
        Some(group) => Ok(group.clone()),
        None => usage_err(format!("unknown service or group: {target}")),
    }
}

pub fn runnable_targets(
    catalog: &ServiceCatalog,
    target: Option<&str>,
) -> LocalctlResult<Vec<ServiceId>> {
    let selected = targets(catalog, target)?;
    // `disabled: true` is a hard stop for a direct target ("start x is disabled"), and a silent
    // skip inside a group — `config_file` already strips disabled members from `catalog.groups`,
    // this only covers the no-group fallback that expands to every service.
    if let Some(t) = target {
        if catalog.services.iter().any(|s| s.id == *t && s.disabled) {
            return usage_err(format!("service {t} is disabled"));
        }
    }
    let selected: Vec<ServiceId> = selected
        .into_iter()
        .filter(|id| {
            !catalog
                .services
                .iter()
                .find(|s| &s.id == id)
                .map(|s| s.disabled)
                .unwrap_or(false)
        })
        .collect();
    if selected.is_empty() {
        return usage_err("nothing to run: every selected service is disabled".to_string());
    }
    for service_id in &selected {
        let verified = catalog
            .services
            .iter()
            .find(|s| &s.id == service_id)
            .map(|s| s.profiles.run.is_verified())
            .unwrap_or(false);
        if !verified {
            return fail_err(EXIT_FAILED, format!("unsupported service: {service_id}"));
        }
    }
    Ok(selected)
}

fn operation_id_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^[A-Za-z0-9._~-]{1,128}$").unwrap())
}

/// Validates an operation id: only RFC 3986 unreserved characters, 1–128 of them.
pub fn operation_id(value: &str) -> LocalctlResult<String> {
    if operation_id_re().is_match(value) {
        Ok(value.to_string())
    } else {
        usage_err("operationId is invalid")
    }
}

/// Polls an operation until it succeeds or fails, with no deadline.
pub async fn wait_operation(client: &Client, id: &str) -> LocalctlResult<Operation> {
    client.wait(id, None).await
}

// ---------------------------------------------------------------------------------------------
// status / cleanup / logs
// ---------------------------------------------------------------------------------------------

async fn service_rows(client: &Client) -> LocalctlResult<Vec<ServiceLifecycleState>> {
    client.services().await
}

/// The state `hearth status` prints. In-flight states collapse into "running", but a state that
/// means something went wrong or is out of this daemon's hands is NEVER collapsed into "stopped" —
/// a crashed service must not read as one nobody started.
fn text_state(state: Option<&ServiceLifecycleState>) -> &'static str {
    match state.map(|s| s.actual_state) {
        Some(ActualServiceState::Ready) => "ready",
        Some(ActualServiceState::Succeeded) => "succeeded",
        Some(ActualServiceState::QueuedStart) => "queued-start",
        Some(ActualServiceState::Running)
        | Some(ActualServiceState::RunningUnready)
        | Some(ActualServiceState::Starting)
        | Some(ActualServiceState::Preparing) => "running",
        Some(ActualServiceState::Stopping) => "stopping",
        Some(ActualServiceState::Failed) => "failed",
        Some(ActualServiceState::Orphaned) => "orphaned",
        Some(ActualServiceState::ExternallyOwned) => "externally-owned",
        Some(ActualServiceState::Stopped) | None => "stopped",
    }
}

pub async fn cleanup(root: &Path, options: &LocalctlOptions) -> LocalctlResult<()> {
    let io = create_file_io(options.catalog.private_file_guard != Some(false));
    let runtime_directory =
        resolve_runtime_directory(root, options.catalog.runtime_directory.as_deref());
    let discovered = discover(root, &options.catalog).await;
    if matches!(discovered, Discovery::Live { .. }) || !io.is_private_directory(&runtime_directory)
    {
        return Ok(());
    }
    let Some(ownership_key) = read_lock_ownership_key(io.as_ref(), &runtime_directory) else {
        return Ok(());
    };
    let mut candidates = vec!["manager.lock".to_string()];
    if let Ok(entries) = std::fs::read_dir(&runtime_directory) {
        for entry in entries.filter_map(|e| e.ok()) {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with("manager.lock.stale-") {
                candidates.push(name);
            }
        }
    }
    for entry in candidates {
        let path = runtime_directory.join(&entry);
        let Some(artifacts) = read_owned_lock_artifacts(io.as_ref(), &path) else {
            continue;
        };
        if !verify_lock_ownership_proof(
            Some(&ownership_key),
            &artifacts.metadata,
            &artifacts.token,
            &artifacts.proof,
        ) {
            continue;
        }
        if entry == "manager.lock" {
            // A live pid — including a stale or protocol-incompatible daemon — still owns this
            // lock. Deleting it makes `LockOwnershipWatch` exit the process that is actually running.
            if hearth_core::platform::is_pid_alive(artifacts.metadata.pid) {
                continue;
            }
            let _ = remove_directory(&path);
            continue;
        }
        let Ok(Some(marker_raw)) =
            io.read_file(&path.join(hearth_core::state::STALE_LOCK_MARKER_NAME))
        else {
            continue;
        };
        let Ok(marker) = serde_json::from_str::<StaleLockMarker>(&marker_raw) else {
            continue;
        };
        if is_stale_lock_marker(
            &marker,
            &ownership_key,
            &artifacts.metadata,
            &artifacts.token,
            &artifacts.proof,
        ) {
            let _ = remove_directory(&path);
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub async fn logs(
    mut client: Client,
    service_id: &str,
    tail: u64,
    follow: bool,
    json_output: bool,
    options: &LocalctlOptions,
    mut write: impl FnMut(&str),
    mut write_err: impl FnMut(&str),
) -> LocalctlResult<()> {
    let mut cursor: Option<u64> = None;
    let mut generation: Option<u64> = None;
    let mut reconnects = 0u32;
    // `--tail` trims the first snapshot only. Later follow chunks are the new lines since the
    // cursor; trimming each one drops everything but the last N lines of every poll.
    let mut first = true;
    loop {
        match client
            .log(service_id, cursor, generation, Some(16_384))
            .await
        {
            Ok(slice) => {
                let lines: Vec<&str> = slice.data.lines().filter(|l| !l.is_empty()).collect();
                let tail_lines: &[&str] = if first && lines.len() as u64 > tail {
                    &lines[lines.len() - tail as usize..]
                } else {
                    &lines
                };
                first = false;
                let joined = if lines.is_empty() {
                    String::new()
                } else {
                    format!("{}\n", tail_lines.join("\n"))
                };
                if json_output {
                    // The server's whole `LogSlice` (its echoed `cursor`, `truncated`), with `data`
                    // narrowed to the requested tail.
                    let printed = json!({"serviceId": service_id, "generation": slice.generation, "cursor": slice.cursor, "nextCursor": slice.next_cursor, "data": joined, "reset": slice.reset, "truncated": slice.truncated});
                    write(&printed.to_string());
                } else {
                    if slice.reset {
                        write_err(&format!(
                            "log reset {service_id} generation {}",
                            slice.generation
                        ));
                    }
                    if !joined.is_empty() {
                        write(&joined);
                    }
                }
                cursor = Some(slice.next_cursor);
                generation = Some(slice.generation);
                reconnects = 0;
                if !follow {
                    return Ok(());
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
            Err(_) => {
                reconnects += 1;
                if !follow || reconnects > 3 {
                    return fail_err(EXIT_UNAVAILABLE, "log follow lost manager connection");
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
                client = require_client(&client.root.clone(), options).await?;
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// main
// ---------------------------------------------------------------------------------------------

fn print_value(value: &Value, json_output: bool) -> String {
    if json_output {
        value.to_string()
    } else if let Value::String(s) = value {
        s.clone()
    } else {
        serde_json::to_string_pretty(value).unwrap()
    }
}

pub struct Io<'a> {
    pub out: &'a mut dyn FnMut(&str),
    pub err: &'a mut dyn FnMut(&str),
    /// Interactive confirmation prompt — receives the question text, returns the user's yes/no.
    /// `None` on non-interactive callers (scripts, tests), which makes prompt-gated behavior
    /// (killing an unowned port-holder) simply never fire.
    pub confirm: Option<&'a mut dyn FnMut(&str) -> bool>,
}

/// Splits a leading `--root <path>` off `argv`; the root defaults to the current directory. The
/// binary calls this once and hands the result to `main`, so every subcommand agrees on the root.
pub fn parse_root(argv: &[String]) -> LocalctlResult<(PathBuf, Vec<String>)> {
    if argv.first().map(String::as_str) == Some("--root") {
        let Some(path) = argv.get(1) else {
            return usage_err("--root requires a path");
        };
        return Ok((PathBuf::from(path), argv[2..].to_vec()));
    }
    let cwd = std::env::current_dir().map_err(|e| LocalctlError {
        exit_code: EXIT_FAILED,
        message: format!("cannot resolve the current directory: {e}"),
    })?;
    Ok((cwd, argv.to_vec()))
}

/// Runs one CLI command for the project at `root` (already split off by `parse_root`).
pub async fn main(options: &LocalctlOptions, root: &Path, argv: &[String], io: &mut Io<'_>) -> i32 {
    match main_inner(options, root, argv, io).await {
        Ok(code) => code,
        Err(error) => {
            (io.err)(&error.message);
            error.exit_code
        }
    }
}

async fn main_inner(
    options: &LocalctlOptions,
    root: &Path,
    argv: &[String],
    io: &mut Io<'_>,
) -> LocalctlResult<i32> {
    let root = root.to_path_buf();
    let Some(command) = argv.first() else {
        return usage_err("usage: hearth <command>");
    };
    let rest = &argv[1..];

    match command.as_str() {
        "doctor" => {
            let flags = parse_command_flags(rest, &[FlagName::Json])?;
            if !flags.positionals.is_empty() {
                return usage_err("doctor takes no positional arguments");
            }
            let adapter = DefaultDoctorAdapter;
            let report = run_doctor(&options.catalog, &default_doctor_checks(), &adapter);
            if flags.json {
                (io.out)(&print_value(&json!(report_to_json(&report)), true));
            } else {
                for check in &report.checks {
                    (io.out)(&format!(
                        "{} {} {}",
                        if check.ok { "ok" } else { "missing" },
                        check.name,
                        check.detail
                    ));
                }
                for warning in &report.unresolved_profiles {
                    (io.out)(&format!("unresolved {warning}"));
                }
            }
            if !report.ok {
                return fail_err(EXIT_FAILED, "doctor checks failed");
            }
            Ok(0)
        }
        "cleanup" => {
            let flags = parse_command_flags(rest, &[])?;
            if !flags.positionals.is_empty() {
                return usage_err("cleanup takes no positional arguments");
            }
            cleanup(&root, options).await?;
            Ok(0)
        }
        "manager" => manager_command(&root, options, rest, io).await,
        "status" => {
            let flags = parse_command_flags(rest, &[FlagName::Json])?;
            if flags.positionals.len() > 1 {
                return usage_err("usage: hearth status [target] [--json]");
            }
            let client = require_client(&root, options).await?;
            let rows = service_rows(&client).await?;
            let selected = targets(
                &options.catalog,
                flags.positionals.first().map(String::as_str),
            )?;
            let result: Vec<Value> = selected
                .iter()
                .map(|service_id| {
                    let row = rows.iter().find(|r| &r.service_id == service_id);
                    let pid =
                        row.and_then(|r| r.identity.as_ref())
                            .and_then(|identity| match identity {
                                hearth_core::state::ProcessIdentity::Posix(p) => Some(p.pid),
                                hearth_core::state::ProcessIdentity::Docker(_) => None,
                            });
                    // `pid` is omitted, never null, when there isn't one — consumers test for
                    // the key's presence.
                    let mut entry = json!({ "serviceId": service_id, "state": text_state(row) });
                    if let Some(pid) = pid {
                        entry["pid"] = json!(pid);
                    }
                    entry
                })
                .collect();
            if flags.json {
                (io.out)(&print_value(&json!({ "services": result }), true));
            } else {
                for row in &result {
                    let pid_suffix = row["pid"]
                        .as_i64()
                        .map(|p| format!(" pid {p}"))
                        .unwrap_or_default();
                    (io.out)(&format!(
                        "{} {}{}",
                        row["state"].as_str().unwrap(),
                        row["serviceId"].as_str().unwrap(),
                        pid_suffix
                    ));
                }
            }
            Ok(0)
        }
        "urls" => urls_command(&root, options, rest, io).await,
        "start" | "stop" | "restart" => {
            start_stop_restart_command(&root, options, command, rest, io).await
        }
        "operation" => {
            let flags = parse_command_flags(rest, &[FlagName::Json])?;
            if flags.positionals.len() != 2
                || !matches!(flags.positionals[0].as_str(), "get" | "watch")
            {
                return usage_err("usage: hearth operation get|watch <operationId> [--json]");
            }
            let client = require_client(&root, options).await?;
            let operation = if flags.positionals[0] == "watch" {
                wait_operation(&client, &flags.positionals[1]).await?
            } else {
                client.operation(&flags.positionals[1]).await?
            };
            (io.out)(&print_value(
                &serde_json::to_value(&operation).unwrap(),
                flags.json,
            ));
            Ok(0)
        }
        "logs" => {
            let flags =
                parse_command_flags(rest, &[FlagName::Tail, FlagName::Follow, FlagName::Json])?;
            let service_ok = flags.positionals.len() == 1
                && options
                    .catalog
                    .services
                    .iter()
                    .any(|s| Some(&s.id) == flags.positionals.first());
            if !service_ok {
                return usage_err("usage: hearth logs <service> [--tail N] [--follow] [--json]");
            }
            let client = require_client(&root, options).await?;
            logs(
                client,
                &flags.positionals[0],
                flags.tail.unwrap_or(200),
                flags.follow,
                flags.json,
                options,
                |s| (io.out)(s),
                |s| (io.err)(s),
            )
            .await?;
            Ok(0)
        }
        "mcp" => mcp_command(&root, rest, io).await,
        "skill" => skill_command(&root, rest, io).await,
        other => usage_err(format!("unknown command: {other}")),
    }
}

fn report_to_json(report: &hearth_core::doctor::DoctorReport) -> Value {
    json!({
        "ok": report.ok,
        "checks": report.checks.iter().map(|c| json!({"name": c.name, "ok": c.ok, "detail": c.detail})).collect::<Vec<_>>(),
        "unresolvedProfiles": report.unresolved_profiles,
    })
}

async fn manager_command(
    root: &Path,
    options: &LocalctlOptions,
    rest: &[String],
    io: &mut Io<'_>,
) -> LocalctlResult<i32> {
    let flags = parse_command_flags(rest, &[FlagName::Json])?;
    let subcommand = flags.positionals.first().cloned();
    if flags.positionals.len() != 1
        || !matches!(
            subcommand.as_deref(),
            Some("ensure") | Some("status") | Some("stop") | Some("restart") | Some("reload")
        )
    {
        return usage_err("usage: hearth manager ensure|status|stop|restart|reload [--json]");
    }
    match subcommand.as_deref().unwrap() {
        "reload" => {
            let client = require_client(root, options).await?;
            let body = json!({ "requestId": uuid::Uuid::new_v4().to_string(), "catalog": options.catalog });
            let result = request(
                &client,
                "/v1/manager/reload",
                reqwest::Method::POST,
                Some(&body),
                Some(client.metadata.protocol_version),
            )
            .await
            .or_else(unavailable_err)?;
            (io.out)(&print_value(&result, flags.json));
            Ok(0)
        }
        "ensure" => {
            let client = ensure(root, options).await?;
            (io.out)(&print_value(&ensure_payload(&client), flags.json));
            Ok(0)
        }
        "restart" => {
            let payload = restart_manager(root, options).await?;
            (io.out)(&print_value(&payload, flags.json));
            Ok(0)
        }
        "status" => {
            let discovered = discover(root, &options.catalog).await;
            if matches!(discovered, Discovery::Incompatible { .. }) {
                return fail_err(EXIT_PROTOCOL, "hearth manager protocol is incompatible");
            }
            let client = require_client(root, options).await?;
            let result = client.manager_info().await?;
            (io.out)(&print_value(&result, flags.json));
            Ok(0)
        }
        _ => {
            // "stop"
            let operation = stop_manager(root, options).await?;
            (io.out)(&print_value(&operation, flags.json));
            Ok(0)
        }
    }
}

/// The shared `manager stop` body, also used by the MCP `stop_daemon` tool: `stop-services`
/// shutdown, then wait for the daemon process to actually exit. `Incompatible` is deliberately
/// included in discovery: shutting the old daemon down is THE documented recovery path after a
/// `PROTOCOL_VERSION` bump, so it is exactly the case where a stop has to work. The request below
/// is sent with the daemon's own `protocol_version`, not ours, so the old daemon accepts it.
/// Returning the shutdown operation (or null when the daemon answered without one).
pub async fn stop_manager(root: &Path, options: &LocalctlOptions) -> LocalctlResult<Value> {
    let discovered = discover(root, &options.catalog).await;
    let client = match discovered {
        Discovery::Live { client }
        | Discovery::Stale { client }
        | Discovery::Incompatible { client } => client,
        _ => require_client(root, options).await?,
    };
    let body = json!({ "requestId": uuid::Uuid::new_v4().to_string(), "mode": "stop-services" });
    let result = request(
        &client,
        "/v1/manager/shutdown",
        reqwest::Method::POST,
        Some(&body),
        Some(client.metadata.protocol_version),
    )
    .await
    .or_else(unavailable_err)?;
    let operation = result.get("operation").cloned().unwrap_or(Value::Null);
    wait_for_daemon_exit(
        client.metadata.pid,
        MANAGER_STOP_TIMEOUT,
        hearth_core::platform::is_pid_alive,
    )
    .await?;
    Ok(operation)
}

/// How long `manager stop` waits for the daemon to finish stopping every service and exit.
const MANAGER_STOP_TIMEOUT: Duration = Duration::from_secs(300);

/// The shutdown request returns as soon as the daemon starts closing, but it keeps its lock (and
/// keeps answering discovery) until every service is stopped. Returning then let an immediate
/// `ensure` reconnect to the closing daemon and get `manager_closing` for every request — so `stop`
/// means "the daemon process is gone". An in-process daemon (tests, embedders) shares our pid and can
/// never be seen to exit, so it is not waited on.
async fn wait_for_daemon_exit(
    pid: i64,
    timeout: Duration,
    alive: impl Fn(i64) -> bool,
) -> LocalctlResult<()> {
    if pid == i64::from(std::process::id()) {
        return Ok(());
    }
    let deadline = tokio::time::Instant::now() + timeout;
    while alive(pid) {
        if tokio::time::Instant::now() >= deadline {
            return fail_err(
                EXIT_FAILED,
                format!(
                    "hearth manager (pid {pid}) did not exit within {}s",
                    timeout.as_secs()
                ),
            );
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    Ok(())
}

/// `urls [target] [--json]`: every registered URL of the selected services, placeholders resolved
/// by the daemon. A URL that only works while its service runs is marked `(not running)` when the
/// service is not up, so a dead link is recognisable before anyone clicks it. A finished one-shot
/// (`succeeded`) is not flagged — it has no process left by design — though `running` stays false
/// in `--json`. URLs whose
/// placeholder has no value on this machine go to stderr with the reason rather than vanishing.
async fn urls_command(
    root: &Path,
    options: &LocalctlOptions,
    rest: &[String],
    io: &mut Io<'_>,
) -> LocalctlResult<i32> {
    let flags = parse_command_flags(rest, &[FlagName::Json])?;
    if flags.positionals.len() > 1 {
        return usage_err("usage: hearth urls [target] [--json]");
    }
    let selected = targets(
        &options.catalog,
        flags.positionals.first().map(String::as_str),
    )?;
    let client = require_client(root, options).await?;
    let body = request(&client, "/v1/urls", reqwest::Method::GET, None, None)
        .await
        .or_else(unavailable_err)?;
    let rows = service_rows(&client).await?;
    let is_running = |service_id: &str| {
        matches!(
            text_state(rows.iter().find(|r| r.service_id == service_id)),
            "ready" | "running"
        )
    };
    let is_finished = |service_id: &str| {
        text_state(rows.iter().find(|r| r.service_id == service_id)) == "succeeded"
    };
    let in_selection = |entry: &&Value| {
        entry["serviceId"]
            .as_str()
            .is_some_and(|id| selected.iter().any(|s| s == id))
    };

    let urls: Vec<Value> = body["urls"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .filter(in_selection)
        .map(|entry| {
            let mut entry = entry.clone();
            entry["running"] = json!(is_running(entry["serviceId"].as_str().unwrap_or_default()));
            entry
        })
        .collect();
    let unresolved: Vec<Value> = body["unresolved"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .filter(in_selection)
        .cloned()
        .collect();

    if flags.json {
        (io.out)(&print_value(
            &json!({ "urls": urls, "unresolved": unresolved }),
            true,
        ));
        return Ok(0);
    }
    for entry in &urls {
        let stale = entry["requiresRunning"].as_bool() != Some(false)
            && entry["running"] == json!(false)
            && !is_finished(entry["serviceId"].as_str().unwrap_or_default());
        (io.out)(&format!(
            "{}  {}  {}{}",
            entry["serviceId"].as_str().unwrap_or_default(),
            entry["label"].as_str().unwrap_or("-"),
            entry["url"].as_str().unwrap_or_default(),
            if stale { "  (not running)" } else { "" }
        ));
    }
    for entry in &unresolved {
        (io.err)(&format!(
            "unresolved {} {}: no value for {{{}}} on this machine",
            entry["serviceId"].as_str().unwrap_or_default(),
            entry["url"].as_str().unwrap_or_default(),
            entry["placeholder"].as_str().unwrap_or_default()
        ));
    }
    Ok(0)
}

/// The row is `externally-owned` and the caller wired an interactive `confirm` — ask whether to
/// kill the port-holder. Returns `Some(true)` on yes (submit with `killUnowned`), `Some(false)`
/// on a declined prompt, `None` when prompting doesn't apply (not externally owned, --json, no
/// confirm callback, or the flag already set it).
fn prompt_kill_unowned(
    service_id: &str,
    row: Option<&ServiceLifecycleState>,
    flags: &Flags,
    io: &mut Io<'_>,
) -> Option<bool> {
    if flags.json || flags.kill_unowned {
        return None;
    }
    let row = row.filter(|r| r.actual_state == ActualServiceState::ExternallyOwned)?;
    let reason = row
        .error
        .clone()
        .unwrap_or_else(|| "its port is held by a process this manager does not own".to_string());
    io.confirm.as_deref_mut().map(|confirm| {
        confirm(&format!(
            "{service_id}: {reason}. Kill it and start? [y/N] "
        ))
    })
}

async fn start_stop_restart_command(
    root: &Path,
    options: &LocalctlOptions,
    command: &str,
    rest: &[String],
    io: &mut Io<'_>,
) -> LocalctlResult<i32> {
    let flags = parse_command_flags(
        rest,
        &[FlagName::Wait, FlagName::Json, FlagName::KillUnowned],
    )?;
    if flags.kill_unowned && command != "start" {
        return usage_err("--kill-unowned only applies to `hearth start`");
    }
    if flags.positionals.len() != 1 {
        let flag_hint = if command == "start" {
            " [--kill-unowned]"
        } else {
            ""
        };
        return usage_err(format!(
            "usage: hearth {command} <service|group> [--wait] [--json]{flag_hint}"
        ));
    }
    let target = &flags.positionals[0];
    let selected = runnable_targets(&options.catalog, Some(target))?;
    let is_single_service = options.catalog.services.iter().any(|s| &s.id == target);
    let client = ensure(root, options).await?;

    if command == "start" && !is_single_service {
        // An explicit `--kill-unowned` is the confirmation for every member of the group; there is
        // no per-service prompt on a bulk start.
        let accepted = client
            .bulk_start(
                &selected,
                flags.kill_unowned,
                &uuid::Uuid::new_v4().to_string(),
            )
            .await?;
        let operation = if flags.wait {
            wait_operation(&client, &accepted.id).await?
        } else {
            accepted
        };
        if flags.json {
            (io.out)(&print_value(&json!({ "operation": operation }), true));
        } else {
            (io.out)(&format!(
                "{} bulk-start {}",
                operation.status.as_wire_str(),
                operation.id
            ));
        }
        if operation.status == OperationStatus::Failed {
            return fail_err(EXIT_FAILED, "service operation failed");
        }
        return Ok(0);
    }

    let mut operations: Vec<Operation> = Vec::new();
    let mut state_rows: Option<Vec<ServiceLifecycleState>> = None;
    let action = match command {
        "start" => ServiceOperationKind::Start,
        "stop" => ServiceOperationKind::Stop,
        _ => ServiceOperationKind::Restart,
    };
    for service_id in &selected {
        let mut kill_unowned = flags.kill_unowned;
        let mut declined = false;
        // Pre-submit prompt: a prior attempt already persisted `externally-owned`, so the conflict
        // is known before this start even runs — works without --wait. A declined prompt still
        // submits a plain start: `externally-owned` is only re-evaluated by a start, and the daemon
        // fails it on its own if the port is still held.
        if action == ServiceOperationKind::Start {
            if state_rows.is_none() {
                state_rows = service_rows(&client).await.ok();
            }
            match prompt_kill_unowned(
                service_id,
                state_rows
                    .as_ref()
                    .and_then(|rows| rows.iter().find(|r| &r.service_id == service_id)),
                &flags,
                io,
            ) {
                Some(true) => kill_unowned = true,
                Some(false) => declined = true,
                None => {}
            }
        }
        let mut completed;
        loop {
            let accepted = client
                .submit(
                    action,
                    service_id,
                    kill_unowned,
                    &uuid::Uuid::new_v4().to_string(),
                )
                .await?;
            // A group has no bulk stop/restart. Waiting for each member keeps
            // stop-on-first-failure order. `--wait` is still what a single service uses
            // to block until that one operation finishes.
            completed = if flags.wait || !is_single_service {
                wait_operation(&client, &accepted.id).await?
            } else {
                accepted
            };
            // Post-failure prompt: a FRESH conflict only surfaces once the operation completes,
            // which needs --wait. Re-read the row — the daemon just persisted `externally-owned`
            // with the holder's pid/command in `error`. Never re-asks a question already declined.
            if action != ServiceOperationKind::Start
                || !flags.wait
                || kill_unowned
                || declined
                || completed.status != OperationStatus::Failed
            {
                break;
            }
            let fresh_row = service_rows(&client)
                .await
                .ok()
                .and_then(|rows| rows.into_iter().find(|r| &r.service_id == service_id));
            match prompt_kill_unowned(service_id, fresh_row.as_ref(), &flags, io) {
                Some(true) => {
                    kill_unowned = true;
                    continue;
                }
                Some(false) => (io.out)(&format!("declined {service_id}")),
                None => {}
            }
            break;
        }
        let failed = completed.status == OperationStatus::Failed;
        operations.push(completed);
        if failed {
            break;
        }
    }
    if flags.json {
        (io.out)(&print_value(&json!({ "operations": operations }), true));
    } else {
        for operation in &operations {
            (io.out)(&format!(
                "{} {} {}",
                operation.status.as_wire_str(),
                operation.service_id.clone().unwrap_or_default(),
                operation.id
            ));
        }
    }
    if operations
        .iter()
        .any(|o| o.status == OperationStatus::Failed)
    {
        return fail_err(EXIT_FAILED, "service operation failed");
    }
    Ok(0)
}

// ---------------------------------------------------------------------------------------------
// mcp install / skill install
//
// `mcp install` registers the *running* `hearth` binary's absolute path directly in an MCP host's
// JSON config — no wrapper script — merging only `{command, args}`.
// ---------------------------------------------------------------------------------------------

async fn mcp_command(root: &Path, rest: &[String], io: &mut Io<'_>) -> LocalctlResult<i32> {
    let Some(subcommand) = rest.first() else {
        return usage_err("usage: hearth mcp install [--name <name>] [--json] <config-file>...");
    };
    match subcommand.as_str() {
        "install" => mcp_install_command(root, &rest[1..], io).await,
        other => usage_err(format!("unknown mcp subcommand: {other}")),
    }
}

/// Merges `mcpServers.<name>` into each given JSON file, setting only `command`/`args` and
/// preserving every other field already on that entry (e.g. infra's `.agent/mcp.json` schema
/// carries `skill`/`summary`/`env`/`mutateGate`/`notes` alongside `command`/`args` — this must
/// never clobber those) and every other entry in the file. Creates the file (as `{}`) if it
/// doesn't exist yet, so a first-time install needs no pre-existing scaffold.
async fn mcp_install_command(root: &Path, rest: &[String], io: &mut Io<'_>) -> LocalctlResult<i32> {
    let flags = parse_command_flags(rest, &[FlagName::Name, FlagName::Key, FlagName::Json])?;
    if flags.positionals.is_empty() {
        return usage_err("usage: hearth mcp install [--name <name>] [--key <topLevelKey>] [--json] <config-file>...");
    }
    let name = flags.name.clone().unwrap_or_else(|| "hearth".to_string());
    let key = flags
        .key
        .clone()
        .unwrap_or_else(|| "mcpServers".to_string());
    let exe = std::env::current_exe().map_err(|e| LocalctlError {
        exit_code: EXIT_FAILED,
        message: format!("could not resolve the running hearth binary's own path: {e}"),
    })?;
    let resolved_root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let command = exe.to_string_lossy().to_string();
    let args = vec![
        "--root".to_string(),
        resolved_root.to_string_lossy().to_string(),
        "mcp".to_string(),
    ];
    for config_file in &flags.positionals {
        install_mcp_entry(Path::new(config_file), &key, &name, &command, &args)?;
    }
    if flags.json {
        (io.out)(&print_value(
            &json!({ "key": key, "server": name, "command": command, "args": args, "files": flags.positionals }),
            true,
        ));
    } else {
        for config_file in &flags.positionals {
            (io.out)(&format!(
                "installed mcp server \"{name}\" into {config_file}"
            ));
        }
    }
    Ok(0)
}

/// Merges `<key>.<name>` (default key: `mcpServers`, the shape every standard MCP host config
/// shares — Claude Code's `.mcp.json`, Kiro's `.kiro/settings/mcp.json`) into `path`, touching
/// only `command`/`args` on that entry. `serde_json`'s `preserve_order` feature is load-bearing
/// here: without it, `Value`'s object type is a `BTreeMap` and silently alphabetizes every key in
/// the *entire* document on write (see AGENTS.md).
fn install_mcp_entry(
    path: &Path,
    key: &str,
    name: &str,
    command: &str,
    args: &[String],
) -> LocalctlResult<()> {
    let io_err = |context: String| {
        move |e: std::io::Error| LocalctlError {
            exit_code: EXIT_FAILED,
            message: format!("{context}: {e}"),
        }
    };
    let mut document: Value = if path.exists() {
        let text = std::fs::read_to_string(path)
            .map_err(io_err(format!("could not read {}", path.display())))?;
        serde_json::from_str(&text).map_err(|e| LocalctlError {
            exit_code: EXIT_FAILED,
            message: format!("could not parse {} as JSON: {e}", path.display()),
        })?
    } else {
        json!({})
    };
    let not_an_object = |what: &str| LocalctlError {
        exit_code: EXIT_FAILED,
        message: format!("{}'s {what} is not a JSON object", path.display()),
    };
    let root_object = document
        .as_object_mut()
        .ok_or_else(|| not_an_object("top level"))?;
    let servers = root_object
        .entry(key.to_string())
        .or_insert_with(|| json!({}));
    let servers_object = servers
        .as_object_mut()
        .ok_or_else(|| not_an_object(&format!("\"{key}\"")))?;
    let entry = servers_object
        .entry(name.to_string())
        .or_insert_with(|| json!({}));
    let entry_object = entry
        .as_object_mut()
        .ok_or_else(|| not_an_object(&format!("\"{key}.{name}\"")))?;
    entry_object.insert("command".to_string(), json!(command));
    entry_object.insert("args".to_string(), json!(args));
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)
            .map_err(io_err(format!("could not create {}", parent.display())))?;
    }
    let serialized = serde_json::to_string_pretty(&document).map_err(|e| LocalctlError {
        exit_code: EXIT_FAILED,
        message: e.to_string(),
    })?;
    std::fs::write(path, serialized + "\n")
        .map_err(io_err(format!("could not write {}", path.display())))?;
    Ok(())
}

/// Generic skill pack (SKILL.md + scripts) for agent operation of hearth-backed projects.
/// MCP is retired for coding agents — scripts wrap the `hearth` CLI instead.
const SKILL_MARKDOWN: &str = include_str!("../skill/SKILL.md");
const SKILL_SCRIPT_HEARTH: &str = include_str!("../skill/scripts/hearth.sh");
const SKILL_SCRIPT_STATUS: &str = include_str!("../skill/scripts/status.sh");
const SKILL_SCRIPT_LOGS: &str = include_str!("../skill/scripts/logs.sh");
const SKILL_SCRIPT_URLS: &str = include_str!("../skill/scripts/urls.sh");
const SKILL_SCRIPT_DOCTOR: &str = include_str!("../skill/scripts/doctor.sh");
const SKILL_SCRIPT_MANAGE: &str = include_str!("../skill/scripts/manage.sh");
const SKILL_SCRIPT_TRACE: &str = include_str!("../skill/scripts/trace.sh");
const SKILL_SCRIPT_EVENTS: &str = include_str!("../skill/scripts/events.sh");
const SKILL_SCRIPT_RESTART_DAEMON: &str = include_str!("../skill/scripts/restart-daemon.sh");
const SKILL_SCRIPT_STOP_DAEMON: &str = include_str!("../skill/scripts/stop-daemon.sh");
const SKILL_SCRIPT_SHARED_LIST: &str = include_str!("../skill/scripts/shared-list.sh");
const SKILL_SCRIPT_SHARED_STATUS: &str = include_str!("../skill/scripts/shared-status.sh");
const SKILL_SCRIPT_SHARED_CONNECTION: &str = include_str!("../skill/scripts/shared-connection.sh");

const SKILL_SCRIPTS: &[(&str, &str)] = &[
    ("hearth.sh", SKILL_SCRIPT_HEARTH),
    ("status.sh", SKILL_SCRIPT_STATUS),
    ("logs.sh", SKILL_SCRIPT_LOGS),
    ("urls.sh", SKILL_SCRIPT_URLS),
    ("doctor.sh", SKILL_SCRIPT_DOCTOR),
    ("manage.sh", SKILL_SCRIPT_MANAGE),
    ("trace.sh", SKILL_SCRIPT_TRACE),
    ("events.sh", SKILL_SCRIPT_EVENTS),
    ("restart-daemon.sh", SKILL_SCRIPT_RESTART_DAEMON),
    ("stop-daemon.sh", SKILL_SCRIPT_STOP_DAEMON),
    ("shared-list.sh", SKILL_SCRIPT_SHARED_LIST),
    ("shared-status.sh", SKILL_SCRIPT_SHARED_STATUS),
    ("shared-connection.sh", SKILL_SCRIPT_SHARED_CONNECTION),
];

async fn skill_command(root: &Path, rest: &[String], io: &mut Io<'_>) -> LocalctlResult<i32> {
    let Some(subcommand) = rest.first() else {
        return usage_err("usage: hearth skill install --dest <skill-dir>");
    };
    match subcommand.as_str() {
        "install" => skill_install_command(root, &rest[1..], io).await,
        other => usage_err(format!("unknown skill subcommand: {other}")),
    }
}

async fn skill_install_command(
    root: &Path,
    rest: &[String],
    io: &mut Io<'_>,
) -> LocalctlResult<i32> {
    let flags = parse_command_flags(rest, &[FlagName::Dest])?;
    if !flags.positionals.is_empty() {
        return usage_err("skill install takes no positional arguments");
    }
    let Some(dest) = flags.dest.clone() else {
        return usage_err("usage: hearth skill install --dest <skill-dir>");
    };
    let dest_path = Path::new(&dest);
    // `--dest` is the skill directory (e.g. ~/.agents/skills/hearth). A trailing SKILL.md path
    // is accepted and normalized to its parent so older one-file installs still work.
    let mut resolved = if dest_path.is_absolute() {
        dest_path.to_path_buf()
    } else {
        root.join(dest_path)
    };
    if resolved.file_name().and_then(|n| n.to_str()) == Some("SKILL.md") {
        if let Some(parent) = resolved.parent() {
            resolved = parent.to_path_buf();
        }
    }
    let scripts_dir = resolved.join("scripts");
    std::fs::create_dir_all(&scripts_dir).map_err(|e| LocalctlError {
        exit_code: EXIT_FAILED,
        message: format!("could not create {}: {e}", scripts_dir.display()),
    })?;
    let skill_md = resolved.join("SKILL.md");
    std::fs::write(&skill_md, SKILL_MARKDOWN).map_err(|e| LocalctlError {
        exit_code: EXIT_FAILED,
        message: format!("could not write {}: {e}", skill_md.display()),
    })?;
    for (name, body) in SKILL_SCRIPTS {
        let path = scripts_dir.join(name);
        std::fs::write(&path, body).map_err(|e| LocalctlError {
            exit_code: EXIT_FAILED,
            message: format!("could not write {}: {e}", path.display()),
        })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&path)
                .map_err(|e| LocalctlError {
                    exit_code: EXIT_FAILED,
                    message: format!("could not stat {}: {e}", path.display()),
                })?
                .permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&path, perms).map_err(|e| LocalctlError {
                exit_code: EXIT_FAILED,
                message: format!("could not chmod {}: {e}", path.display()),
            })?;
        }
    }
    (io.out)(&format!("installed hearth skill at {}", resolved.display()));
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use hearth_core::catalog::{
        CommandSpec, ReadinessSpec, ServiceCommand, ServiceDefinition, ServiceKind,
        ServiceProfiles, ServiceRunProfile, StartFailurePolicy,
    };
    use hearth_core::manager::{bootstrap, HearthManager, HearthManagerOptions};
    use hearth_core::supervisor::Host as _;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    // -----------------------------------------------------------------------------------------
    // parse_command_flags
    // -----------------------------------------------------------------------------------------

    fn args(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parses_positionals_and_known_flags() {
        let flags = parse_command_flags(
            &args(&["start", "api", "--wait", "--json"]),
            &[FlagName::Wait, FlagName::Json],
        )
        .unwrap();
        assert_eq!(flags.positionals, vec!["start", "api"]);
        assert!(flags.wait);
        assert!(flags.json);
        assert!(!flags.follow);
    }

    #[test]
    fn rejects_an_unsupported_flag() {
        let err = parse_command_flags(&args(&["--wait"]), &[FlagName::Json]).unwrap_err();
        assert_eq!(err.exit_code, EXIT_USAGE);
        assert!(err.message.contains("unsupported flag"));
    }

    #[test]
    fn rejects_a_duplicate_flag() {
        let err = parse_command_flags(&args(&["--json", "--json"]), &[FlagName::Json]).unwrap_err();
        assert!(err.message.contains("duplicate flag"));
    }

    #[test]
    fn rejects_an_unknown_flag() {
        let err = parse_command_flags(&args(&["--bogus"]), &[FlagName::Json]).unwrap_err();
        assert!(err.message.contains("unknown flag"));
    }

    #[test]
    fn tail_requires_a_positive_integer() {
        let err = parse_command_flags(&args(&["--tail", "0"]), &[FlagName::Tail]).unwrap_err();
        assert!(err.message.contains("--tail must be a positive integer"));
        let err = parse_command_flags(&args(&["--tail", "abc"]), &[FlagName::Tail]).unwrap_err();
        assert!(err.message.contains("--tail must be a positive integer"));
        let flags = parse_command_flags(&args(&["--tail", "50"]), &[FlagName::Tail]).unwrap();
        assert_eq!(flags.tail, Some(50));
    }

    #[test]
    fn parse_root_splits_a_leading_root_flag_and_rejects_a_missing_path() {
        let (root, rest) = parse_root(&args(&["--root", "/tmp/project", "status"])).unwrap();
        assert_eq!(root, PathBuf::from("/tmp/project"));
        assert_eq!(rest, vec!["status".to_string()]);
        let (root, rest) = parse_root(&args(&["status"])).unwrap();
        assert_eq!(root, std::env::current_dir().unwrap());
        assert_eq!(rest, vec!["status".to_string()]);
        assert_eq!(
            parse_root(&args(&["--root"])).unwrap_err().exit_code,
            EXIT_USAGE
        );
    }

    #[test]
    fn encode_path_segment_escapes_everything_outside_the_unreserved_set() {
        assert_eq!(encode_path_segment("api-1_.~"), "api-1_.~");
        assert_eq!(encode_path_segment("a/b?c#d e"), "a%2Fb%3Fc%23d%20e");
        assert_eq!(encode_path_segment("é"), "%C3%A9");
    }

    #[test]
    fn operation_id_accepts_the_unreserved_charset_and_rejects_others() {
        assert_eq!(operation_id("abc-123_.~").unwrap(), "abc-123_.~");
        assert!(operation_id("has a space").is_err());
        assert!(operation_id("").is_err());
        assert!(operation_id(&"x".repeat(129)).is_err());
    }

    // -----------------------------------------------------------------------------------------
    // Real end-to-end: a real bootstrapped HearthManager, driven entirely through main().
    // -----------------------------------------------------------------------------------------

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
                run: ServiceRunProfile::Verified {
                    command: ServiceCommand {
                        command: CommandSpec::Shell {
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
        }
    }

    fn test_catalog(runtime_directory: &Path, services: Vec<ServiceDefinition>) -> ServiceCatalog {
        ServiceCatalog {
            services,
            groups: HashMap::new(),
            group_tree: Vec::new(),
            compose_file: None,
            runtime_directory: Some(runtime_directory.to_string_lossy().to_string()),
            start_failure_policy: StartFailurePolicy::StopOnFirstFailureKeepStarted,
            private_file_guard: Some(false),
        }
    }

    fn never_spawn() -> SpawnDaemon {
        Box::new(|_root| {
            panic!("spawn_daemon should not be called when a manager is already running")
        })
    }

    async fn bootstrap_manager(catalog: ServiceCatalog) -> Arc<HearthManager> {
        bootstrap(HearthManagerOptions {
            runtime_directory: None,
            root: Some(PathBuf::from("/tmp")),
            catalog,
            event_capacity: None,
            log_tail_bytes: None,
            log_max_bytes: None,
            log_rotation_count: None,
            supervisor: None,
            shared: None,
        })
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn status_command_reports_stopped_for_a_fresh_catalog() {
        let dir = tempfile::tempdir().unwrap();
        let port = free_port();
        let catalog = test_catalog(dir.path(), vec![tcp_service("api", port)]);
        let manager = bootstrap_manager(catalog.clone()).await;

        let out = Arc::new(Mutex::new(Vec::new()));
        let out2 = out.clone();
        let mut out_fn = move |s: &str| out2.lock().unwrap().push(s.to_string());
        let mut err_fn = move |_: &str| {};
        let options = LocalctlOptions {
            catalog,
            spawn_daemon: never_spawn(),
        };
        let mut io = Io {
            out: &mut out_fn,
            err: &mut err_fn,
            confirm: None,
        };
        let code = main(
            &options,
            Path::new("/tmp"),
            &args(&["status", "--json"]),
            &mut io,
        )
        .await;
        assert_eq!(code, 0, "{:?}", out.lock().unwrap());
        let printed: Value = serde_json::from_str(&out.lock().unwrap()[0]).unwrap();
        assert_eq!(printed["services"][0]["state"], "stopped");
        manager.close().await;
    }

    #[tokio::test]
    async fn start_wait_json_reports_ready_and_real_process_starts() {
        let dir = tempfile::tempdir().unwrap();
        let port = free_port();
        let catalog = test_catalog(dir.path(), vec![tcp_service("api", port)]);
        let manager = bootstrap_manager(catalog.clone()).await;

        let out = Arc::new(Mutex::new(Vec::new()));
        let out2 = out.clone();
        let mut out_fn = move |s: &str| out2.lock().unwrap().push(s.to_string());
        let mut err_fn = move |_: &str| {};
        let options = LocalctlOptions {
            catalog,
            spawn_daemon: never_spawn(),
        };
        let mut io = Io {
            out: &mut out_fn,
            err: &mut err_fn,
            confirm: None,
        };
        let code = main(
            &options,
            Path::new("/tmp"),
            &args(&["start", "api", "--wait", "--json"]),
            &mut io,
        )
        .await;
        assert_eq!(code, 0, "{:?}", out.lock().unwrap());
        let printed: Value = serde_json::from_str(out.lock().unwrap().last().unwrap()).unwrap();
        assert_eq!(printed["operations"][0]["status"], "succeeded");
        assert_eq!(
            manager.service_states()[0].actual_state,
            hearth_core::state::ActualServiceState::Ready
        );
        manager.close().await;
    }

    /// A real process squatting on the service's port — `nc -lk` in its own process group, killed
    /// on drop so a failing assertion never leaks it into the next test.
    struct PortSquatter {
        child: std::process::Child,
    }
    impl PortSquatter {
        #[allow(clippy::zombie_processes)] // Drop kills and reaps the child on every exit path
        async fn hold(port: u16) -> Self {
            use std::os::unix::process::CommandExt;
            let child = std::process::Command::new("nc")
                .args(["-lk", &port.to_string()])
                .process_group(0)
                .spawn()
                .unwrap();
            // Poll for the actual bind — a fixed sleep is a flake (see AGENTS.md).
            for _ in 0..100 {
                if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
                    return Self { child };
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            panic!("squatter never bound port {port}");
        }
    }
    impl Drop for PortSquatter {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    /// The flag is the non-interactive "yes": the daemon SIGTERMs the squatter and the service
    /// starts on the freed port.
    #[tokio::test]
    async fn start_kill_unowned_terminates_the_process_holding_the_port() {
        let dir = tempfile::tempdir().unwrap();
        let port = free_port();
        let mut squatter = PortSquatter::hold(port).await;
        let catalog = test_catalog(dir.path(), vec![tcp_service("api", port)]);
        let manager = bootstrap_manager(catalog.clone()).await;

        // Without the flag the same start must refuse and leave the squatter alone.
        let out = Arc::new(Mutex::new(Vec::new()));
        let out2 = out.clone();
        let mut out_fn = move |s: &str| out2.lock().unwrap().push(s.to_string());
        let mut err_fn = move |_: &str| {};
        let options = LocalctlOptions {
            catalog: catalog.clone(),
            spawn_daemon: never_spawn(),
        };
        let mut io = Io {
            out: &mut out_fn,
            err: &mut err_fn,
            confirm: None,
        };
        let code = main(
            &options,
            Path::new("/tmp"),
            &args(&["start", "api", "--wait", "--json"]),
            &mut io,
        )
        .await;
        assert_ne!(code, 0, "a held port must refuse a plain start");
        assert_eq!(
            manager.service_states()[0].actual_state,
            hearth_core::state::ActualServiceState::ExternallyOwned
        );
        assert!(
            squatter.child.try_wait().unwrap().is_none(),
            "the squatter must still be alive"
        );

        let mut io = Io {
            out: &mut out_fn,
            err: &mut err_fn,
            confirm: None,
        };
        let code = main(
            &options,
            Path::new("/tmp"),
            &args(&["start", "api", "--wait", "--json", "--kill-unowned"]),
            &mut io,
        )
        .await;
        assert_eq!(code, 0, "{:?}", out.lock().unwrap());
        let printed: Value = serde_json::from_str(out.lock().unwrap().last().unwrap()).unwrap();
        assert_eq!(printed["operations"][0]["status"], "succeeded");
        assert_eq!(
            manager.service_states()[0].actual_state,
            hearth_core::state::ActualServiceState::Ready
        );
        for _ in 0..50 {
            if squatter.child.try_wait().unwrap().is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(
            squatter.child.try_wait().unwrap().is_some(),
            "the squatter must have been terminated"
        );
        manager.close().await;
    }

    /// On an interactive terminal the held-port failure prompts instead of just failing; answering
    /// yes resubmits the same start with `killUnowned`.
    #[tokio::test]
    async fn start_prompts_and_reclaims_when_the_user_confirms() {
        let dir = tempfile::tempdir().unwrap();
        let port = free_port();
        let squatter = PortSquatter::hold(port).await;
        let catalog = test_catalog(dir.path(), vec![tcp_service("api", port)]);
        let manager = bootstrap_manager(catalog.clone()).await;

        let prompts = Arc::new(Mutex::new(Vec::new()));
        let prompts2 = prompts.clone();
        let mut confirm = move |question: &str| {
            prompts2.lock().unwrap().push(question.to_string());
            true
        };
        let mut out_fn = move |_: &str| {};
        let mut err_fn = move |_: &str| {};
        let options = LocalctlOptions {
            catalog,
            spawn_daemon: never_spawn(),
        };
        let mut io = Io {
            out: &mut out_fn,
            err: &mut err_fn,
            confirm: Some(&mut confirm),
        };
        let code = main(
            &options,
            Path::new("/tmp"),
            &args(&["start", "api", "--wait"]),
            &mut io,
        )
        .await;
        assert_eq!(code, 0);
        assert_eq!(
            prompts.lock().unwrap().len(),
            1,
            "the fresh conflict must prompt exactly once, after the failed submit"
        );
        assert!(
            prompts.lock().unwrap()[0].contains(&format!("api: Port {port} is held by pid")),
            "{:?}",
            prompts.lock().unwrap()
        );
        assert_eq!(
            manager.service_states()[0].actual_state,
            hearth_core::state::ActualServiceState::Ready
        );
        drop(squatter);
        manager.close().await;
    }

    /// Answering no — or having no terminal at all (`confirm: None`) — never kills anything: the
    /// service stays `externally-owned` and the squatter keeps its port.
    #[tokio::test]
    async fn start_declined_prompt_leaves_the_squatter_alone() {
        let dir = tempfile::tempdir().unwrap();
        let port = free_port();
        let mut squatter = PortSquatter::hold(port).await;
        let catalog = test_catalog(dir.path(), vec![tcp_service("api", port)]);
        let manager = bootstrap_manager(catalog.clone()).await;

        let prompts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let prompts2 = prompts.clone();
        let mut confirm = move |_: &str| {
            prompts2.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            false
        };
        let mut out_fn = move |_: &str| {};
        let mut err_fn = move |_: &str| {};
        let options = LocalctlOptions {
            catalog,
            spawn_daemon: never_spawn(),
        };
        let mut io = Io {
            out: &mut out_fn,
            err: &mut err_fn,
            confirm: Some(&mut confirm),
        };
        let code = main(
            &options,
            Path::new("/tmp"),
            &args(&["start", "api", "--wait"]),
            &mut io,
        )
        .await;
        assert_ne!(code, 0, "a declined reclaim is still a failed start");
        assert_eq!(
            manager.service_states()[0].actual_state,
            hearth_core::state::ActualServiceState::ExternallyOwned
        );
        assert!(
            squatter.child.try_wait().unwrap().is_none(),
            "declining must never signal the squatter"
        );

        // The row is now persisted `externally-owned`: the pre-submit prompt asks once, and a "no"
        // still submits a plain start (the daemon re-checks the port) rather than failing unsent.
        let before = manager.service_states()[0].updated_at.clone();
        prompts.store(0, std::sync::atomic::Ordering::SeqCst);
        let code = main(
            &options,
            Path::new("/tmp"),
            &args(&["start", "api", "--wait"]),
            &mut io,
        )
        .await;
        assert_ne!(code, 0);
        assert_eq!(
            prompts.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "a declined question is never asked twice"
        );
        assert_ne!(
            manager.service_states()[0].updated_at,
            before,
            "the declined prompt must still submit a plain start"
        );
        assert!(
            squatter.child.try_wait().unwrap().is_none(),
            "declining must never signal the squatter"
        );
        manager.close().await;
    }

    /// `--kill-unowned` on `stop`/`restart` is a usage error, not a silent ignore.
    #[tokio::test]
    async fn kill_unowned_on_a_non_start_command_is_a_usage_error() {
        let dir = tempfile::tempdir().unwrap();
        let catalog = test_catalog(dir.path(), vec![]);
        let err = Arc::new(Mutex::new(Vec::new()));
        let err2 = err.clone();
        let mut out_fn = move |_: &str| {};
        let mut err_fn = move |s: &str| err2.lock().unwrap().push(s.to_string());
        let options = LocalctlOptions {
            catalog,
            spawn_daemon: never_spawn(),
        };
        let mut io = Io {
            out: &mut out_fn,
            err: &mut err_fn,
            confirm: None,
        };
        let code = main(
            &options,
            Path::new("/tmp"),
            &args(&["stop", "api", "--kill-unowned"]),
            &mut io,
        )
        .await;
        assert_eq!(code, EXIT_USAGE);
        assert!(
            err.lock().unwrap()[0].contains("--kill-unowned only applies"),
            "{:?}",
            err.lock().unwrap()
        );
    }

    #[tokio::test]
    async fn manager_ensure_prints_a_connectable_client() {
        let dir = tempfile::tempdir().unwrap();
        let catalog = test_catalog(dir.path(), vec![]);
        let manager = bootstrap_manager(catalog.clone()).await;

        let out = Arc::new(Mutex::new(Vec::new()));
        let out2 = out.clone();
        let mut out_fn = move |s: &str| out2.lock().unwrap().push(s.to_string());
        let mut err_fn = move |_: &str| {};
        let options = LocalctlOptions {
            catalog,
            spawn_daemon: never_spawn(),
        };
        let mut io = Io {
            out: &mut out_fn,
            err: &mut err_fn,
            confirm: None,
        };
        let code = main(
            &options,
            Path::new("/tmp"),
            &args(&["manager", "ensure", "--json"]),
            &mut io,
        )
        .await;
        assert_eq!(code, 0, "{:?}", out.lock().unwrap());
        let printed: Value = serde_json::from_str(&out.lock().unwrap()[0]).unwrap();
        assert_eq!(printed["instanceId"], json!(manager.instance_id));
        assert_eq!(printed["token"], json!(manager.bearer_token()));
        manager.close().await;
    }

    #[tokio::test]
    async fn ensure_spawns_a_real_daemon_when_none_is_running() {
        let dir = tempfile::tempdir().unwrap();
        let catalog = test_catalog(dir.path(), vec![]);
        let root = PathBuf::from("/tmp");
        let catalog_for_spawn = catalog.clone();
        let spawn_daemon: SpawnDaemon = Box::new(move |root: &Path| {
            let root = root.to_path_buf();
            let catalog = catalog_for_spawn.clone();
            tokio::spawn(async move {
                hearth_core::daemon::run_daemon(
                    HearthManagerOptions {
                        runtime_directory: None,
                        root: Some(root),
                        catalog,
                        event_capacity: None,
                        log_tail_bytes: None,
                        log_max_bytes: None,
                        log_rotation_count: None,
                        supervisor: None,
                        shared: None,
                    },
                    hearth_core::manager::ShutdownMode::LeaveServices,
                )
                .await;
            });
        });
        let options = LocalctlOptions {
            catalog,
            spawn_daemon,
        };
        let client = ensure(&root, &options).await.unwrap();
        assert!(client.metadata.port > 0);
        // Clean up: ask the real daemon we just spawned to stop.
        let _ = request(
            &client,
            "/v1/manager/shutdown",
            reqwest::Method::POST,
            Some(&json!({"requestId": uuid::Uuid::new_v4().to_string(), "mode": "stop-services"})),
            Some(client.metadata.protocol_version),
        )
        .await;
    }

    #[tokio::test]
    async fn manager_stop_waits_for_the_daemon_process_to_exit_and_fails_if_it_never_does() {
        let checks = std::sync::atomic::AtomicUsize::new(0);
        let waited = wait_for_daemon_exit(999_999, Duration::from_secs(5), |_| {
            checks.fetch_add(1, std::sync::atomic::Ordering::SeqCst) < 2
        })
        .await;
        assert!(waited.is_ok());
        assert_eq!(checks.load(std::sync::atomic::Ordering::SeqCst), 3);

        let stuck = wait_for_daemon_exit(999_999, Duration::from_millis(300), |_| true).await;
        let err = stuck.expect_err("a daemon that never exits must fail the stop");
        assert!(
            format!("{err:?}").contains("did not exit within"),
            "{err:?}"
        );

        // Our own pid is an in-process daemon: never waited on, even though it is alive.
        assert!(wait_for_daemon_exit(
            i64::from(std::process::id()),
            Duration::from_millis(1),
            |_| true
        )
        .await
        .is_ok());
    }

    /// A daemon that predates `leave-services` rejects the mode outright — and that daemon is
    /// exactly the one `restart` exists to replace (a running daemon is always the *old* binary
    /// right after an install). It must be shut down through its own graceful SIGTERM path rather
    /// than left running with the restart reported as failed.
    #[tokio::test]
    async fn restart_falls_back_to_sigterm_for_a_daemon_that_rejects_leave_services() {
        use std::os::unix::process::{CommandExt, ExitStatusExt};

        // A stand-in for the old daemon's HTTP surface: it answers every request with the 400 an
        // older `post_shutdown` returns for a mode it does not know.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                tokio::spawn(async move {
                    use tokio::io::AsyncReadExt as _;
                    let mut buffer = [0u8; 4096];
                    let _ = socket.read(&mut buffer).await;
                    let body = r#"{"error":{"code":"invalid_shutdown_mode","message":"mode must be refuse-if-active or stop-services"}}"#;
                    let response = format!("HTTP/1.1 400 Bad Request\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len());
                    use tokio::io::AsyncWriteExt as _;
                    let _ = socket.write_all(response.as_bytes()).await;
                });
            }
        });

        // The "daemon process": its own process group, reaped by a thread the moment it dies — a
        // zombie still answers `kill(pid, 0)`, so nothing here may hold the pid open.
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .process_group(0)
            .spawn()
            .unwrap();
        let pid = i64::from(child.id());
        let reaper = std::thread::spawn(move || child.wait().unwrap());
        let client = Client {
            root: PathBuf::from("/tmp"),
            runtime_directory: PathBuf::from("/tmp"),
            metadata: ManagerMetadata {
                version: 1,
                protocol_version: PROTOCOL_VERSION,
                instance_id: "old-daemon".to_string(),
                pid,
                port,
                started_at: "2026-01-01T00:00:00.000Z".to_string(),
            },
            token: "token".to_string(),
        };

        shut_down_for_restart(&client)
            .await
            .expect("an old daemon must still be replaceable");

        let status = reaper.join().unwrap();
        // Nothing in this path but the fallback signals it — `sleep 30` never exits on its own.
        assert!(
            status.signal().is_some(),
            "the old daemon must be asked to shut down, not left running"
        );
        assert!(
            !hearth_core::platform::is_pid_alive(pid),
            "the old daemon's process must be gone"
        );
    }

    #[tokio::test]
    async fn doctor_reports_platform_check() {
        let dir = tempfile::tempdir().unwrap();
        let catalog = test_catalog(dir.path(), vec![]);
        let out = Arc::new(Mutex::new(Vec::new()));
        let out2 = out.clone();
        let mut out_fn = move |s: &str| out2.lock().unwrap().push(s.to_string());
        let mut err_fn = move |_: &str| {};
        let options = LocalctlOptions {
            catalog,
            spawn_daemon: never_spawn(),
        };
        let mut io = Io {
            out: &mut out_fn,
            err: &mut err_fn,
            confirm: None,
        };
        let code = main(&options, Path::new("/tmp"), &args(&["doctor"]), &mut io).await;
        assert_eq!(code, 0);
        assert!(out
            .lock()
            .unwrap()
            .iter()
            .any(|line| line.contains("platform")));
    }

    #[tokio::test]
    async fn unknown_command_is_a_usage_error() {
        let dir = tempfile::tempdir().unwrap();
        let catalog = test_catalog(dir.path(), vec![]);
        let out = Arc::new(Mutex::new(Vec::new()));
        let err = Arc::new(Mutex::new(Vec::new()));
        let out2 = out.clone();
        let err2 = err.clone();
        let mut out_fn = move |s: &str| out2.lock().unwrap().push(s.to_string());
        let mut err_fn = move |s: &str| err2.lock().unwrap().push(s.to_string());
        let options = LocalctlOptions {
            catalog,
            spawn_daemon: never_spawn(),
        };
        let mut io = Io {
            out: &mut out_fn,
            err: &mut err_fn,
            confirm: None,
        };
        let code = main(&options, Path::new("/tmp"), &args(&["bogus"]), &mut io).await;
        assert_eq!(code, EXIT_USAGE);
        assert!(err.lock().unwrap()[0].contains("unknown command"));
    }

    #[tokio::test]
    async fn status_without_a_running_manager_is_unavailable() {
        let dir = tempfile::tempdir().unwrap();
        let catalog = test_catalog(dir.path(), vec![]);
        let out = Arc::new(Mutex::new(Vec::new()));
        let err = Arc::new(Mutex::new(Vec::new()));
        let out2 = out.clone();
        let err2 = err.clone();
        let mut out_fn = move |s: &str| out2.lock().unwrap().push(s.to_string());
        let mut err_fn = move |s: &str| err2.lock().unwrap().push(s.to_string());
        let options = LocalctlOptions {
            catalog,
            spawn_daemon: never_spawn(),
        };
        let mut io = Io {
            out: &mut out_fn,
            err: &mut err_fn,
            confirm: None,
        };
        let code = main(&options, Path::new("/tmp"), &args(&["status"]), &mut io).await;
        assert_eq!(code, EXIT_UNAVAILABLE);
        assert!(err.lock().unwrap()[0].contains("unavailable"));
    }

    // -----------------------------------------------------------------------------------------
    // mcp install / skill install
    // -----------------------------------------------------------------------------------------

    async fn run_cli(dir: &Path, extra_args: &[&str]) -> (i32, Vec<String>, Vec<String>) {
        let catalog = test_catalog(dir, vec![]);
        let options = LocalctlOptions {
            catalog,
            spawn_daemon: never_spawn(),
        };
        let out = Arc::new(Mutex::new(Vec::new()));
        let err = Arc::new(Mutex::new(Vec::new()));
        let out2 = out.clone();
        let err2 = err.clone();
        let mut out_fn = move |s: &str| out2.lock().unwrap().push(s.to_string());
        let mut err_fn = move |s: &str| err2.lock().unwrap().push(s.to_string());
        let mut io = Io {
            out: &mut out_fn,
            err: &mut err_fn,
            confirm: None,
        };
        let code = main(&options, dir, &args(extra_args), &mut io).await;
        let out = out.lock().unwrap().clone();
        let err = err.lock().unwrap().clone();
        (code, out, err)
    }

    #[tokio::test]
    async fn mcp_install_creates_a_new_config_file_with_the_resolved_hearth_path() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("mcp.json");
        let (code, out, _err) = run_cli(
            dir.path(),
            &["mcp", "install", config_path.to_str().unwrap()],
        )
        .await;
        assert_eq!(code, 0);
        assert!(out[0].contains("installed mcp server \"hearth\""));

        let written: Value =
            serde_json::from_str(&std::fs::read_to_string(&config_path).unwrap()).unwrap();
        let entry = &written["mcpServers"]["hearth"];
        let expected_exe = std::env::current_exe()
            .unwrap()
            .to_string_lossy()
            .to_string();
        assert_eq!(entry["command"], json!(expected_exe));
        let expected_root = std::fs::canonicalize(dir.path())
            .unwrap()
            .to_string_lossy()
            .to_string();
        assert_eq!(entry["args"], json!(["--root", expected_root, "mcp"]));
    }

    #[tokio::test]
    async fn mcp_install_preserves_other_fields_and_other_servers() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("mcp.json");
        std::fs::write(
            &config_path,
            json!({
                "mcpServers": {
                    "other-server": { "command": "node", "args": ["other.mjs"] },
                    "hearth": {
                        "command": "node",
                        "args": ["old-wrapper.mjs"],
                        "skill": "local-dev",
                        "env": {},
                        "notes": ["some historical note"],
                    },
                },
            })
            .to_string(),
        )
        .unwrap();

        let (code, _out, _err) = run_cli(
            dir.path(),
            &["mcp", "install", config_path.to_str().unwrap()],
        )
        .await;
        assert_eq!(code, 0);

        let written: Value =
            serde_json::from_str(&std::fs::read_to_string(&config_path).unwrap()).unwrap();
        assert_eq!(
            written["mcpServers"]["other-server"]["command"],
            json!("node")
        );
        assert_eq!(
            written["mcpServers"]["other-server"]["args"],
            json!(["other.mjs"])
        );
        let entry = &written["mcpServers"]["hearth"];
        assert_eq!(entry["skill"], json!("local-dev"));
        assert_eq!(entry["notes"], json!(["some historical note"]));
        assert_ne!(entry["args"], json!(["old-wrapper.mjs"]));
    }

    #[tokio::test]
    async fn mcp_install_preserves_the_original_key_order_of_a_human_maintained_file() {
        // Deliberately non-alphabetical key order, matching how a hand-maintained file is grouped
        // rather than sorted — without serde_json's `preserve_order` feature, `Value`'s object
        // type is a BTreeMap and silently alphabetizes every key in the document on write, turning
        // a one-entry change into a huge, unreviewable diff (a real bug hit against infra's own
        // .agent/mcp.json mid-development — see AGENTS.md).
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("mcp.json");
        std::fs::write(
            &config_path,
            r#"{
  "mcpServers": {
    "zebra-server": { "command": "node", "args": ["z.mjs"] },
    "hearth": { "command": "node", "args": ["old-wrapper.mjs"] },
    "apple-server": { "command": "node", "args": ["a.mjs"] }
  }
}"#,
        )
        .unwrap();

        let (code, _out, _err) = run_cli(
            dir.path(),
            &["mcp", "install", config_path.to_str().unwrap()],
        )
        .await;
        assert_eq!(code, 0);

        let written = std::fs::read_to_string(&config_path).unwrap();
        let zebra_pos = written.find("zebra-server").unwrap();
        let local_pos = written.find("\"hearth\"").unwrap();
        let apple_pos = written.find("apple-server").unwrap();
        assert!(
            zebra_pos < local_pos && local_pos < apple_pos,
            "key order was not preserved:\n{written}"
        );
    }

    #[tokio::test]
    async fn mcp_install_supports_a_custom_top_level_key() {
        // infra's own .agent/mcp.json uses "servers" as its top-level key, not the standard
        // "mcpServers" every plain MCP host config shares — --key makes that schema installable
        // through the same generic merge instead of a repo-specific special case in this binary.
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("agent-mcp.json");
        std::fs::write(&config_path, json!({ "servers": { "hearth": { "skill": "local-dev", "command": "node", "args": ["old.mjs"] } } }).to_string()).unwrap();

        let (code, _out, _err) = run_cli(
            dir.path(),
            &[
                "mcp",
                "install",
                "--key",
                "servers",
                config_path.to_str().unwrap(),
            ],
        )
        .await;
        assert_eq!(code, 0);

        let written: Value =
            serde_json::from_str(&std::fs::read_to_string(&config_path).unwrap()).unwrap();
        assert!(
            written.get("mcpServers").is_none(),
            "must not create a stray \"mcpServers\" key when --key targets a different one"
        );
        let entry = &written["servers"]["hearth"];
        assert_eq!(entry["skill"], json!("local-dev"));
        assert_ne!(entry["args"], json!(["old.mjs"]));
    }

    #[tokio::test]
    async fn mcp_install_supports_a_custom_name_and_multiple_files() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.json");
        let b = dir.path().join("nested/b.json");
        let (code, out, _err) = run_cli(
            dir.path(),
            &[
                "mcp",
                "install",
                "--name",
                "viclass-hearth",
                a.to_str().unwrap(),
                b.to_str().unwrap(),
            ],
        )
        .await;
        assert_eq!(code, 0);
        assert_eq!(out.len(), 2);
        for path in [&a, &b] {
            let written: Value =
                serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
            assert!(written["mcpServers"]["viclass-hearth"]["command"].is_string());
        }
    }

    #[tokio::test]
    async fn mcp_install_requires_at_least_one_config_file() {
        let dir = tempfile::tempdir().unwrap();
        let (code, _out, err) = run_cli(dir.path(), &["mcp", "install"]).await;
        assert_eq!(code, EXIT_USAGE);
        assert!(err[0].contains("usage: hearth mcp install"));
    }

    #[tokio::test]
    async fn mcp_rejects_an_unknown_subcommand() {
        let dir = tempfile::tempdir().unwrap();
        let (code, _out, err) = run_cli(dir.path(), &["mcp", "bogus"]).await;
        assert_eq!(code, EXIT_USAGE);
        assert!(err[0].contains("unknown mcp subcommand"));
    }

    #[tokio::test]
    async fn skill_install_writes_the_skill_pack_at_a_relative_dest() {
        let dir = tempfile::tempdir().unwrap();
        let (code, out, _err) = run_cli(
            dir.path(),
            &["skill", "install", "--dest", ".agents/skills/hearth"],
        )
        .await;
        assert_eq!(code, 0);
        assert!(out[0].contains("installed hearth skill"));
        let skill_dir = dir.path().join(".agents/skills/hearth");
        let written = std::fs::read_to_string(skill_dir.join("SKILL.md")).unwrap();
        assert_eq!(written, SKILL_MARKDOWN);
        assert!(written.contains("scripts/manage.sh"));
        assert!(written.contains("`hearth`"));
        assert!(!written.contains("hearthd"));
        let wrapper = skill_dir.join("scripts/hearth.sh");
        assert!(wrapper.is_file());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&wrapper).unwrap().permissions().mode();
            assert_eq!(mode & 0o111, 0o111, "hearth.sh should be executable");
        }
        let wrapper_body = std::fs::read_to_string(&wrapper).unwrap();
        assert!(wrapper_body.contains("HEARTH_BIN"));
        assert!(wrapper_body.contains(".local/bin/hearth"));
        assert!(!wrapper_body.contains("hearthd"));
        assert!(skill_dir.join("scripts/status.sh").is_file());
        assert!(skill_dir.join("scripts/shared-connection.sh").is_file());
    }

    #[tokio::test]
    async fn skill_install_normalizes_a_skill_md_dest_to_its_parent_directory() {
        let dir = tempfile::tempdir().unwrap();
        let (code, out, _err) = run_cli(
            dir.path(),
            &[
                "skill",
                "install",
                "--dest",
                ".agents/skills/hearth/SKILL.md",
            ],
        )
        .await;
        assert_eq!(code, 0);
        assert!(out[0].contains("installed hearth skill"));
        assert!(dir.path().join(".agents/skills/hearth/SKILL.md").is_file());
        assert!(dir
            .path()
            .join(".agents/skills/hearth/scripts/hearth.sh")
            .is_file());
    }

    #[tokio::test]
    async fn skill_install_requires_a_dest_flag() {
        let dir = tempfile::tempdir().unwrap();
        let (code, _out, err) = run_cli(dir.path(), &["skill", "install"]).await;
        assert_eq!(code, EXIT_USAGE);
        assert!(err[0].contains("usage: hearth skill install"));
    }

    /// A failed service must not print as "stopped" — that made a crash indistinguishable from a
    /// service nobody started, while every other surface (TUI, MCP) reported it failed.
    #[test]
    fn text_state_never_hides_a_failure_as_stopped() {
        use hearth_core::state::{DesiredServiceState, ServiceReadiness};
        let state = |actual: ActualServiceState| ServiceLifecycleState {
            service_id: "svc".to_string(),
            desired_state: DesiredServiceState::Running,
            actual_state: actual,
            readiness: ServiceReadiness::Unknown,
            generation: 1,
            identity: None,
            readiness_kind: None,
            readiness_detail: None,
            created_at: "t".to_string(),
            updated_at: "t".to_string(),
            exited_at: None,
            exit_code: None,
            error: None,
            current_operation_id: None,
        };
        let expected = [
            (ActualServiceState::Stopped, "stopped"),
            (ActualServiceState::QueuedStart, "queued-start"),
            (ActualServiceState::Preparing, "running"),
            (ActualServiceState::Starting, "running"),
            (ActualServiceState::Running, "running"),
            (ActualServiceState::RunningUnready, "running"),
            (ActualServiceState::Ready, "ready"),
            (ActualServiceState::Succeeded, "succeeded"),
            (ActualServiceState::Stopping, "stopping"),
            (ActualServiceState::Failed, "failed"),
            (ActualServiceState::Orphaned, "orphaned"),
            (ActualServiceState::ExternallyOwned, "externally-owned"),
        ];
        assert_eq!(
            expected.len(),
            ActualServiceState::ALL.len(),
            "a new state needs an explicit CLI rendering"
        );
        for (actual, printed) in expected {
            assert_eq!(text_state(Some(&state(actual))), printed, "{actual:?}");
        }
        assert_eq!(text_state(None), "stopped");
    }

    /// A URL that needs its service running is flagged while the service is stopped, so a dead link
    /// is recognisable before anyone clicks it; a URL that works regardless is not.
    #[tokio::test]
    async fn urls_command_lists_urls_and_flags_the_ones_whose_service_is_down() {
        let dir = tempfile::tempdir().unwrap();
        let mut api = tcp_service("api", free_port());
        api.urls = Some(vec![
            hearth_core::catalog::ServiceUrl {
                url: "http://127.0.0.1:18080/".into(),
                label: Some("app".into()),
                requires_running: None,
            },
            hearth_core::catalog::ServiceUrl {
                url: "http://127.0.0.1:18081/".into(),
                label: None,
                requires_running: Some(false),
            },
        ]);
        let catalog = test_catalog(dir.path(), vec![api]);
        let manager = bootstrap_manager(catalog.clone()).await;

        let run = |extra: &'static [&'static str]| {
            let catalog = catalog.clone();
            async move {
                let out = Arc::new(Mutex::new(Vec::new()));
                let out2 = out.clone();
                let mut out_fn = move |s: &str| out2.lock().unwrap().push(s.to_string());
                let mut err_fn = move |_: &str| {};
                let options = LocalctlOptions {
                    catalog,
                    spawn_daemon: never_spawn(),
                };
                let mut io = Io {
                    out: &mut out_fn,
                    err: &mut err_fn,
                    confirm: None,
                };
                let mut argv = vec!["urls"];
                argv.extend_from_slice(extra);
                let code = main(&options, Path::new("/tmp"), &args(&argv), &mut io).await;
                let lines = out.lock().unwrap().clone();
                (code, lines)
            }
        };

        let (code, lines) = run(&[]).await;
        assert_eq!(code, 0, "{lines:?}");
        assert_eq!(
            lines,
            vec![
                "api  app  http://127.0.0.1:18080/  (not running)".to_string(),
                "api  -  http://127.0.0.1:18081/".to_string()
            ]
        );

        let (_, lines) = run(&["api", "--json"]).await;
        let printed: Value = serde_json::from_str(&lines[0]).unwrap();
        assert_eq!(printed["urls"][0]["running"], json!(false));
        assert_eq!(printed["urls"].as_array().unwrap().len(), 2);
        manager.close().await;
    }
}
