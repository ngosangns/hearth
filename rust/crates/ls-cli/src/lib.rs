//! Port of `src/cli/localctl.ts` — a thin HTTP client for one daemon's loopback API, plain-text or
//! JSON output, hand-rolled flag parsing. Phase 4 of the Rust-rewrite plan.
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

use regex::Regex;
use serde_json::{json, Value};

use ls_core::catalog::{ServiceCatalog, ServiceId};
use ls_core::doctor::{run_doctor, DefaultDoctorAdapter, DoctorChecks};
use ls_core::file_io::{create_file_io, remove_directory};
use ls_core::manager::{is_stale_lock_marker, read_lock_ownership_key, read_owned_lock_artifacts, verify_lock_ownership_proof};
use ls_core::paths::resolve_runtime_directory;
use ls_core::state::{ActualServiceState, ManagerMetadata, Operation, OperationStatus, ServiceLifecycleState, StaleLockMarker, PROTOCOL_VERSION};

pub const EXIT_USAGE: i32 = 2;
pub const EXIT_UNAVAILABLE: i32 = 3;
pub const EXIT_PROTOCOL: i32 = 4;
pub const EXIT_FAILED: i32 = 5;
#[allow(dead_code)]
pub const EXIT_TIMEOUT: i32 = 6;
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

fn usage_err<T>(message: impl Into<String>) -> LocalctlResult<T> {
    Err(LocalctlError { exit_code: EXIT_USAGE, message: message.into() })
}
fn fail_err<T>(exit_code: i32, message: impl Into<String>) -> LocalctlResult<T> {
    Err(LocalctlError { exit_code, message: message.into() })
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
    pub doctor_checks: Option<DoctorChecks>,
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
    metadata.version == 1 && !metadata.instance_id.is_empty() && metadata.port > 0 && metadata.pid > 0 && !metadata.started_at.is_empty()
}

/// Sends one request to the daemon at `client`, with the standard auth + protocol headers. Maps
/// transport failures to the same plain-text error shapes the TS source uses (`"manager unavailable"`
/// / a timeout message), since callers pattern-match on the string containing `"unauthorized"`.
pub async fn request(client: &Client, path: &str, method: reqwest::Method, body: Option<&Value>, protocol_version: Option<u32>) -> Result<Value, String> {
    let url = format!("http://127.0.0.1:{}{}", client.metadata.port, path);
    let protocol = protocol_version.unwrap_or(PROTOCOL_VERSION);
    let http_client = reqwest::Client::new();
    let mut builder = http_client.request(method, &url).bearer_auth(&client.token).header("x-local-services-protocol", protocol.to_string()).timeout(MANAGER_REQUEST_TIMEOUT);
    if let Some(body) = body {
        builder = builder.json(body);
    }
    let response = match builder.send().await {
        Ok(response) => response,
        Err(error) if error.is_timeout() => return Err(format!("manager request timed out after {}ms", MANAGER_REQUEST_TIMEOUT.as_millis())),
        Err(_) => return Err("manager unavailable".to_string()),
    };
    let status = response.status();
    let body: Value = response.json().await.unwrap_or_else(|_| json!({}));
    if !status.is_success() {
        let code = body.get("error").and_then(|e| e.get("code")).and_then(Value::as_str).unwrap_or("request_failed");
        let message = body.get("error").and_then(|e| e.get("message")).and_then(Value::as_str).map(str::to_string).unwrap_or_else(|| status.as_u16().to_string());
        return Err(format!("{code}:{message}"));
    }
    Ok(body)
}

pub async fn discover(root: &Path, catalog: &ServiceCatalog) -> Discovery {
    let io = create_file_io(catalog.private_file_guard != Some(false));
    let runtime_directory = resolve_runtime_directory(root, catalog.runtime_directory.as_deref());
    let lock_directory = runtime_directory.join("manager.lock");
    if !io.is_private_directory(&runtime_directory) || !io.is_private_directory(&lock_directory) {
        return Discovery::Absent;
    }
    let raw_metadata = io.read_file(&lock_directory.join("metadata.json")).ok().flatten();
    let raw_token = io.read_file(&lock_directory.join("token")).ok().flatten();
    let (Some(raw_metadata), Some(raw_token)) = (raw_metadata, raw_token) else { return Discovery::Absent };
    let Ok(metadata) = serde_json::from_str::<ManagerMetadata>(&raw_metadata) else { return Discovery::Malformed };
    let token = raw_token.trim().to_string();
    if !valid_metadata(&metadata) || token.is_empty() {
        return Discovery::Malformed;
    }
    let Some(artifacts) = read_owned_lock_artifacts(io.as_ref(), &lock_directory) else { return Discovery::Malformed };
    let Some(ownership_key) = read_lock_ownership_key(io.as_ref(), &runtime_directory) else { return Discovery::Malformed };
    if !verify_lock_ownership_proof(Some(&ownership_key), &artifacts.metadata, &artifacts.token, &artifacts.proof) {
        return Discovery::Malformed;
    }
    let client = Client { root: root.to_path_buf(), runtime_directory, metadata: metadata.clone(), token };
    if metadata.protocol_version != PROTOCOL_VERSION {
        return Discovery::Incompatible { client };
    }
    match request(&client, "/v1/manager", reqwest::Method::GET, None, None).await {
        Ok(_) => Discovery::Live { client },
        Err(e) if e.contains("unauthorized") => Discovery::Live { client },
        Err(_) => Discovery::Stale { client },
    }
}

/// Discovers a live daemon, spawning one (via `options.spawn_daemon`) and polling if none is found.
pub async fn ensure(root: &Path, options: &LocalctlOptions) -> LocalctlResult<Client> {
    let discovered = discover(root, &options.catalog).await;
    if let Discovery::Live { client } = discovered {
        return Ok(client);
    } else if let Discovery::Incompatible { .. } = discovered {
        return fail_err(EXIT_PROTOCOL, "local services manager protocol is incompatible");
    }
    (options.spawn_daemon)(root);
    for _ in 0..100 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        let discovered = discover(root, &options.catalog).await;
        if let Discovery::Live { client } = discovered {
            return Ok(client);
        } else if let Discovery::Incompatible { .. } = discovered {
            return fail_err(EXIT_PROTOCOL, "local services manager protocol is incompatible");
        }
    }
    fail_err(EXIT_UNAVAILABLE, "local services manager is unavailable")
}

pub async fn require_client(root: &Path, options: &LocalctlOptions) -> LocalctlResult<Client> {
    let discovered = discover(root, &options.catalog).await;
    if let Discovery::Incompatible { .. } = discovered {
        return fail_err(EXIT_PROTOCOL, "local services manager protocol is incompatible");
    }
    let client = match discovered {
        Discovery::Live { client } => client,
        _ => return fail_err(EXIT_UNAVAILABLE, "local services manager is unavailable"),
    };
    match request(&client, "/v1/manager", reqwest::Method::GET, None, None).await {
        Ok(_) => Ok(client),
        Err(e) if e.contains("unauthorized") => fail_err(EXIT_UNAUTHORIZED, "local services manager authentication failed"),
        Err(_) => fail_err(EXIT_UNAVAILABLE, "local services manager is unavailable"),
    }
}

// ---------------------------------------------------------------------------------------------
// Targets / operation ids
// ---------------------------------------------------------------------------------------------

pub fn targets(catalog: &ServiceCatalog, target: Option<&str>) -> LocalctlResult<Vec<ServiceId>> {
    let Some(target) = target else { return Ok(catalog.services.iter().map(|s| s.id.clone()).collect()) };
    if catalog.services.iter().any(|s| s.id == target) {
        return Ok(vec![target.to_string()]);
    }
    match catalog.groups.get(target) {
        Some(group) => Ok(group.clone()),
        None => usage_err(format!("unknown service or group: {target}")),
    }
}

pub fn runnable_targets(catalog: &ServiceCatalog, target: Option<&str>) -> LocalctlResult<Vec<ServiceId>> {
    let selected = targets(catalog, target)?;
    for service_id in &selected {
        let verified = catalog.services.iter().find(|s| &s.id == service_id).map(|s| s.profiles.run.is_verified()).unwrap_or(false);
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

/// The TS source URL-encodes this after validating it — a no-op in practice, since the validating
/// regex only accepts characters already in RFC 3986's "unreserved" set, which never need encoding.
/// Skipped here rather than pulling in a URL-encoding crate for a call that can never change its
/// input.
pub fn operation_id(value: &str) -> LocalctlResult<String> {
    if operation_id_re().is_match(value) {
        Ok(value.to_string())
    } else {
        usage_err("operationId is invalid")
    }
}

pub async fn wait_operation(client: &Client, id: &str) -> LocalctlResult<Operation> {
    let encoded = operation_id(id)?;
    loop {
        let body = request(client, &format!("/v1/operations/{encoded}"), reqwest::Method::GET, None, None).await.or_else(unavailable_err)?;
        let operation: Operation = serde_json::from_value(body["operation"].clone()).map_err(|e| LocalctlError { exit_code: EXIT_FAILED, message: e.to_string() })?;
        if matches!(operation.status, OperationStatus::Succeeded | OperationStatus::Failed) {
            return Ok(operation);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

// ---------------------------------------------------------------------------------------------
// status / cleanup / logs
// ---------------------------------------------------------------------------------------------

async fn service_rows(client: &Client) -> LocalctlResult<Vec<ServiceLifecycleState>> {
    let body = request(client, "/v1/services", reqwest::Method::GET, None, None).await.or_else(unavailable_err)?;
    serde_json::from_value(body["services"].clone()).map_err(|e| LocalctlError { exit_code: EXIT_FAILED, message: e.to_string() })
}

/// The state `lsd status` prints. In-flight states collapse into "running", but a state that
/// means something went wrong or is out of this daemon's hands is NEVER collapsed into "stopped".
/// It used to be: a crashed service (`failed`, still `desiredState: running`) printed as "stopped",
/// indistinguishable from one that was simply never started, while the TUI, the macOS app and the
/// MCP tools all reported it as failed. Four viclass services sat in that state for two days with
/// `lsd status` calling them stopped.
fn text_state(state: Option<&ServiceLifecycleState>) -> &'static str {
    match state.map(|s| s.actual_state) {
        Some(ActualServiceState::Ready) => "ready",
        Some(ActualServiceState::QueuedStart) => "queued-start",
        Some(ActualServiceState::Running) | Some(ActualServiceState::RunningUnready) | Some(ActualServiceState::Starting) | Some(ActualServiceState::Preparing) => "running",
        Some(ActualServiceState::Stopping) => "stopping",
        Some(ActualServiceState::Failed) => "failed",
        Some(ActualServiceState::Orphaned) => "orphaned",
        Some(ActualServiceState::ExternallyOwned) => "externally-owned",
        Some(ActualServiceState::Stopped) | None => "stopped",
    }
}

pub async fn cleanup(root: &Path, options: &LocalctlOptions) -> LocalctlResult<()> {
    let io = create_file_io(options.catalog.private_file_guard != Some(false));
    let runtime_directory = resolve_runtime_directory(root, options.catalog.runtime_directory.as_deref());
    let discovered = discover(root, &options.catalog).await;
    if matches!(discovered, Discovery::Live { .. }) || !io.is_private_directory(&runtime_directory) {
        return Ok(());
    }
    let Some(ownership_key) = read_lock_ownership_key(io.as_ref(), &runtime_directory) else { return Ok(()) };
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
        let Some(artifacts) = read_owned_lock_artifacts(io.as_ref(), &path) else { continue };
        if !verify_lock_ownership_proof(Some(&ownership_key), &artifacts.metadata, &artifacts.token, &artifacts.proof) {
            continue;
        }
        if entry == "manager.lock" {
            let _ = remove_directory(&path);
            continue;
        }
        let Ok(Some(marker_raw)) = io.read_file(&path.join(ls_core::state::STALE_LOCK_MARKER_NAME)) else { continue };
        let Ok(marker) = serde_json::from_str::<StaleLockMarker>(&marker_raw) else { continue };
        if is_stale_lock_marker(&marker, &ownership_key, &artifacts.metadata, &artifacts.token, &artifacts.proof) {
            let _ = remove_directory(&path);
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)] // mirrors the TS function's own parameter list 1:1
pub async fn logs(mut client: Client, service_id: &str, tail: u64, follow: bool, json_output: bool, options: &LocalctlOptions, mut write: impl FnMut(&str), mut write_err: impl FnMut(&str)) -> LocalctlResult<()> {
    let mut cursor: Option<u64> = None;
    let mut generation: Option<u64> = None;
    let mut reconnects = 0u32;
    loop {
        let mut query = "limit=16384".to_string();
        if let Some(c) = cursor {
            query.push_str(&format!("&cursor={c}"));
        }
        if let Some(g) = generation {
            query.push_str(&format!("&generation={g}"));
        }
        match request(&client, &format!("/v1/logs/{service_id}?{query}"), reqwest::Method::GET, None, None).await {
            Ok(body) => {
                let data = body["data"].as_str().unwrap_or("").to_string();
                let next_cursor = body["nextCursor"].as_u64().unwrap_or(0);
                let response_generation = body["generation"].as_u64().unwrap_or(0);
                let reset = body["reset"].as_bool().unwrap_or(false);
                let lines: Vec<&str> = data.lines().filter(|l| !l.is_empty()).collect();
                let tail_lines: &[&str] = if lines.len() as u64 > tail { &lines[lines.len() - tail as usize..] } else { &lines };
                let joined = if lines.is_empty() { String::new() } else { format!("{}\n", tail_lines.join("\n")) };
                if json_output {
                    // `cursor` is the server's echoed cursor, not the client's previous one (which
                    // is null on a first poll), and `truncated` is carried through. The TS CLI
                    // prints the server's whole `LogSlice`, so both fields were simply missing or
                    // wrong here.
                    let mut slice = json!({"serviceId": service_id, "generation": response_generation, "cursor": body.get("cursor").cloned().unwrap_or(Value::Null), "nextCursor": next_cursor, "data": joined, "reset": reset});
                    if let Some(truncated) = body.get("truncated") {
                        slice["truncated"] = truncated.clone();
                    }
                    write(&slice.to_string());
                } else {
                    if reset {
                        write_err(&format!("log reset {service_id} generation {response_generation}"));
                    }
                    if !joined.is_empty() {
                        write(&joined);
                    }
                }
                cursor = Some(next_cursor);
                generation = Some(response_generation);
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
}

pub async fn main(options: &LocalctlOptions, argv: &[String], io: &mut Io<'_>) -> i32 {
    match main_inner(options, argv, io).await {
        Ok(code) => code,
        Err(error) => {
            (io.err)(&error.message);
            error.exit_code
        }
    }
}

async fn main_inner(options: &LocalctlOptions, argv: &[String], io: &mut Io<'_>) -> LocalctlResult<i32> {
    let mut argv = argv;
    let mut root = std::env::current_dir().unwrap();
    let owned_argv;
    if argv.first().map(String::as_str) == Some("--root") {
        let path = argv.get(1).cloned();
        let Some(path) = path else { return usage_err("--root requires a path") };
        root = PathBuf::from(path);
        owned_argv = argv[2..].to_vec();
        argv = &owned_argv;
    }
    let Some(command) = argv.first() else { return usage_err("usage: local-services <command>") };
    let rest = &argv[1..];

    match command.as_str() {
        "doctor" => {
            let flags = parse_command_flags(rest, &[FlagName::Json])?;
            if !flags.positionals.is_empty() {
                return usage_err("doctor takes no positional arguments");
            }
            let adapter = DefaultDoctorAdapter;
            let default_checks = DoctorChecks::default();
            let checks = options.doctor_checks.as_ref().unwrap_or(&default_checks);
            let report = run_doctor(&options.catalog, checks, &adapter);
            if flags.json {
                (io.out)(&print_value(&json!(report_to_json(&report)), true));
            } else {
                for check in &report.checks {
                    (io.out)(&format!("{} {} {}", if check.ok { "ok" } else { "missing" }, check.name, check.detail));
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
                return usage_err("usage: local-services status [target] [--json]");
            }
            let client = require_client(&root, options).await?;
            let rows = service_rows(&client).await?;
            let selected = targets(&options.catalog, flags.positionals.first().map(String::as_str))?;
            let result: Vec<Value> = selected
                .iter()
                .map(|service_id| {
                    let row = rows.iter().find(|r| &r.service_id == service_id);
                    let pid = row.and_then(|r| r.identity.as_ref()).and_then(|identity| match identity {
                        ls_core::state::ProcessIdentity::Posix(p) => Some(p.pid),
                        ls_core::state::ProcessIdentity::Docker(_) => None,
                    });
                    // `pid` is OMITTED when there isn't one, rather than emitted as null — the
                    // TS CLI builds the object the same way, and a consumer doing
                    // `"pid" in service` sees a different answer between the two otherwise.
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
                    let pid_suffix = row["pid"].as_i64().map(|p| format!(" pid {p}")).unwrap_or_default();
                    (io.out)(&format!("{} {}{}", row["state"].as_str().unwrap(), row["serviceId"].as_str().unwrap(), pid_suffix));
                }
            }
            Ok(0)
        }
        "urls" => urls_command(&root, options, rest, io).await,
        "start" | "stop" | "restart" => start_stop_restart_command(&root, options, command, rest, io).await,
        "operation" => {
            let flags = parse_command_flags(rest, &[FlagName::Json])?;
            if flags.positionals.len() != 2 || !matches!(flags.positionals[0].as_str(), "get" | "watch") {
                return usage_err("usage: local-services operation get|watch <operationId> [--json]");
            }
            let client = require_client(&root, options).await?;
            let operation = if flags.positionals[0] == "watch" {
                wait_operation(&client, &flags.positionals[1]).await?
            } else {
                let encoded = operation_id(&flags.positionals[1])?;
                let body = request(&client, &format!("/v1/operations/{encoded}"), reqwest::Method::GET, None, None).await.or_else(unavailable_err)?;
                serde_json::from_value(body["operation"].clone()).map_err(|e| LocalctlError { exit_code: EXIT_FAILED, message: e.to_string() })?
            };
            (io.out)(&print_value(&serde_json::to_value(&operation).unwrap(), flags.json));
            Ok(0)
        }
        "logs" => {
            let flags = parse_command_flags(rest, &[FlagName::Tail, FlagName::Follow, FlagName::Json])?;
            let service_ok = flags.positionals.len() == 1 && options.catalog.services.iter().any(|s| Some(&s.id) == flags.positionals.first());
            if !service_ok {
                return usage_err("usage: local-services logs <service> [--tail N] [--follow] [--json]");
            }
            let client = require_client(&root, options).await?;
            logs(client, &flags.positionals[0], flags.tail.unwrap_or(200), flags.follow, flags.json, options, |s| (io.out)(s), |s| (io.err)(s)).await?;
            Ok(0)
        }
        "tui" => {
            let flags = parse_command_flags(rest, &[])?;
            if !flags.positionals.is_empty() {
                return usage_err("tui takes no positional arguments");
            }
            ensure(&root, options).await?;
            usage_err("tui is not available: this build did not wire a `tui` runtime handler (see the /tui subpath)")
        }
        "mcp" => mcp_command(&root, rest, io).await,
        "skill" => skill_command(&root, rest, io).await,
        other => usage_err(format!("unknown command: {other}")),
    }
}

fn report_to_json(report: &ls_core::doctor::DoctorReport) -> Value {
    json!({
        "ok": report.ok,
        "checks": report.checks.iter().map(|c| json!({"name": c.name, "ok": c.ok, "detail": c.detail})).collect::<Vec<_>>(),
        "unresolvedProfiles": report.unresolved_profiles,
    })
}

async fn manager_command(root: &Path, options: &LocalctlOptions, rest: &[String], io: &mut Io<'_>) -> LocalctlResult<i32> {
    let flags = parse_command_flags(rest, &[FlagName::Json])?;
    let subcommand = flags.positionals.first().cloned();
    if flags.positionals.len() != 1 || !matches!(subcommand.as_deref(), Some("ensure") | Some("status") | Some("stop") | Some("reload")) {
        return usage_err("usage: local-services manager ensure|status|stop|reload [--json]");
    }
    match subcommand.as_deref().unwrap() {
        "reload" => {
            let client = require_client(root, options).await?;
            let body = json!({ "requestId": uuid::Uuid::new_v4().to_string(), "catalog": options.catalog });
            let result = request(&client, "/v1/manager/reload", reqwest::Method::POST, Some(&body), Some(client.metadata.protocol_version)).await.or_else(unavailable_err)?;
            (io.out)(&print_value(&result, flags.json));
            Ok(0)
        }
        "ensure" => {
            let client = ensure(root, options).await?;
            // A generic client (a desktop app's connection layer) needs the bearer token and
            // runtime directory to talk to the daemon directly over HTTP+SSE — no more exposed
            // than the lock directory already is.
            let payload = json!({
                "instanceId": client.metadata.instance_id,
                "port": client.metadata.port,
                "token": client.token,
                "protocolVersion": client.metadata.protocol_version,
                "runtimeDirectory": client.runtime_directory,
                "root": client.root,
            });
            (io.out)(&print_value(&payload, flags.json));
            Ok(0)
        }
        "status" => {
            let discovered = discover(root, &options.catalog).await;
            if matches!(discovered, Discovery::Incompatible { .. }) {
                return fail_err(EXIT_PROTOCOL, "local services manager protocol is incompatible");
            }
            let client = require_client(root, options).await?;
            let result = request(&client, "/v1/manager", reqwest::Method::GET, None, None).await.or_else(unavailable_err)?;
            (io.out)(&print_value(&result, flags.json));
            Ok(0)
        }
        _ => {
            // "stop"
            let discovered = discover(root, &options.catalog).await;
            // `Incompatible` is deliberately included: shutting the old daemon down is THE
            // documented recovery path after a `PROTOCOL_VERSION` bump, so it is exactly the case
            // where `manager stop` has to work. The request below is sent with the daemon's own
            // `protocol_version`, not ours, so the old daemon accepts it. Excluding it here sent
            // this through `require_client`, which then failed with exit 4 — leaving a user whose
            // daemon predates a protocol bump no way to stop it.
            let client = match discovered {
                Discovery::Live { client } | Discovery::Stale { client } | Discovery::Incompatible { client } => client,
                _ => require_client(root, options).await?,
            };
            let body = json!({ "requestId": uuid::Uuid::new_v4().to_string(), "mode": "stop-services" });
            let result = request(&client, "/v1/manager/shutdown", reqwest::Method::POST, Some(&body), Some(client.metadata.protocol_version)).await.or_else(unavailable_err)?;
            let operation = result.get("operation").cloned().unwrap_or(Value::Null);
            wait_for_daemon_exit(client.metadata.pid, MANAGER_STOP_TIMEOUT, ls_core::platform::is_pid_alive).await?;
            (io.out)(&print_value(&operation, flags.json));
            Ok(0)
        }
    }
}

/// How long `manager stop` waits for the daemon to finish stopping every service and exit.
const MANAGER_STOP_TIMEOUT: Duration = Duration::from_secs(300);

/// The shutdown request returns as soon as the daemon starts closing, but it keeps its lock (and
/// keeps answering discovery) until every service is stopped. Returning then let an immediate
/// `ensure` reconnect to the closing daemon and get `manager_closing` for every request — so `stop`
/// means "the daemon process is gone". An in-process daemon (tests, embedders) shares our pid and can
/// never be seen to exit, so it is not waited on.
async fn wait_for_daemon_exit(pid: i64, timeout: Duration, alive: impl Fn(i64) -> bool) -> LocalctlResult<()> {
    if pid == i64::from(std::process::id()) {
        return Ok(());
    }
    let deadline = tokio::time::Instant::now() + timeout;
    while alive(pid) {
        if tokio::time::Instant::now() >= deadline {
            return fail_err(EXIT_FAILED, format!("local services manager (pid {pid}) did not exit within {}s", timeout.as_secs()));
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    Ok(())
}

/// `urls [target] [--json]`: every registered URL of the selected services, placeholders resolved
/// by the daemon. A URL that only works while its service runs is marked `(not running)` when the
/// service is not up, so a dead link is recognisable before anyone clicks it; URLs whose
/// placeholder has no value on this machine go to stderr with the reason rather than vanishing.
async fn urls_command(root: &Path, options: &LocalctlOptions, rest: &[String], io: &mut Io<'_>) -> LocalctlResult<i32> {
    let flags = parse_command_flags(rest, &[FlagName::Json])?;
    if flags.positionals.len() > 1 {
        return usage_err("usage: local-services urls [target] [--json]");
    }
    let selected = targets(&options.catalog, flags.positionals.first().map(String::as_str))?;
    let client = require_client(root, options).await?;
    let body = request(&client, "/v1/urls", reqwest::Method::GET, None, None).await.or_else(unavailable_err)?;
    let rows = service_rows(&client).await?;
    let is_running = |service_id: &str| matches!(text_state(rows.iter().find(|r| r.service_id == service_id)), "ready" | "running");
    let in_selection = |entry: &&Value| entry["serviceId"].as_str().is_some_and(|id| selected.iter().any(|s| s == id));

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
    let unresolved: Vec<Value> = body["unresolved"].as_array().map(Vec::as_slice).unwrap_or_default().iter().filter(in_selection).cloned().collect();

    if flags.json {
        (io.out)(&print_value(&json!({ "urls": urls, "unresolved": unresolved }), true));
        return Ok(0);
    }
    for entry in &urls {
        let stale = entry["requiresRunning"].as_bool() != Some(false) && entry["running"] == json!(false);
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

async fn start_stop_restart_command(root: &Path, options: &LocalctlOptions, command: &str, rest: &[String], io: &mut Io<'_>) -> LocalctlResult<i32> {
    let flags = parse_command_flags(rest, &[FlagName::Wait, FlagName::Json])?;
    if flags.positionals.len() != 1 {
        return usage_err(format!("usage: local-services {command} <service|group> [--wait] [--json]"));
    }
    let target = &flags.positionals[0];
    let selected = runnable_targets(&options.catalog, Some(target))?;
    let is_single_service = options.catalog.services.iter().any(|s| &s.id == target);
    if !flags.wait && !is_single_service {
        return usage_err(format!("group {target} requires --wait to preserve stop-on-first-failure ordering"));
    }
    let client = ensure(root, options).await?;

    if command == "start" && !is_single_service {
        let body = json!({ "requestId": uuid::Uuid::new_v4().to_string(), "targets": selected });
        let accepted_body = request(&client, "/v1/operations/bulk-start", reqwest::Method::POST, Some(&body), None).await.or_else(unavailable_err)?;
        let accepted: Operation = serde_json::from_value(accepted_body["operation"].clone()).map_err(|e| LocalctlError { exit_code: EXIT_FAILED, message: e.to_string() })?;
        let operation = if flags.wait { wait_operation(&client, &accepted.id).await? } else { accepted };
        if flags.json {
            (io.out)(&print_value(&json!({ "operation": operation }), true));
        } else {
            (io.out)(&format!("{} bulk-start {}", operation_status_str(operation.status), operation.id));
        }
        if operation.status == OperationStatus::Failed {
            return fail_err(EXIT_FAILED, "service operation failed");
        }
        return Ok(0);
    }

    let mut operations: Vec<Operation> = Vec::new();
    for service_id in &selected {
        let action = match command {
            "start" => "start",
            "stop" => "stop",
            _ => "restart",
        };
        let body = json!({ "requestId": uuid::Uuid::new_v4().to_string(), "serviceId": service_id, "action": action });
        let accepted_body = request(&client, "/v1/operations", reqwest::Method::POST, Some(&body), None).await.or_else(unavailable_err)?;
        let accepted: Operation = serde_json::from_value(accepted_body["operation"].clone()).map_err(|e| LocalctlError { exit_code: EXIT_FAILED, message: e.to_string() })?;
        let completed = if flags.wait { wait_operation(&client, &accepted.id).await? } else { accepted };
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
            (io.out)(&format!("{} {} {}", operation_status_str(operation.status), operation.service_id.clone().unwrap_or_default(), operation.id));
        }
    }
    if operations.iter().any(|o| o.status == OperationStatus::Failed) {
        return fail_err(EXIT_FAILED, "service operation failed");
    }
    Ok(0)
}

fn operation_status_str(status: OperationStatus) -> &'static str {
    match status {
        OperationStatus::Queued => "queued",
        OperationStatus::Running => "running",
        OperationStatus::Succeeded => "succeeded",
        OperationStatus::Failed => "failed",
    }
}

// ---------------------------------------------------------------------------------------------
// mcp install / skill install
//
// Every real consumer of this package (infra, viclass) used to hand-author its own ~20-line
// Node/Bun wrapper spawning `lsd mcp`, plus hand-edit its MCP host's JSON config to point at that
// wrapper — duplicated per repo for no reason once `lsd` itself is a single binary any MCP host
// can spawn directly. `mcp install` replaces both: it resolves the *running* `lsd` binary's own
// absolute path (no wrapper script needed) and merges a `{command, args}` entry directly into
// whatever JSON file the caller points it at.
// ---------------------------------------------------------------------------------------------

async fn mcp_command(root: &Path, rest: &[String], io: &mut Io<'_>) -> LocalctlResult<i32> {
    let Some(subcommand) = rest.first() else {
        return usage_err("usage: local-services mcp install [--name <name>] [--json] <config-file>...");
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
        return usage_err("usage: local-services mcp install [--name <name>] [--key <topLevelKey>] [--json] <config-file>...");
    }
    let name = flags.name.clone().unwrap_or_else(|| "local-services".to_string());
    let key = flags.key.clone().unwrap_or_else(|| "mcpServers".to_string());
    let exe = std::env::current_exe()
        .map_err(|e| LocalctlError { exit_code: EXIT_FAILED, message: format!("could not resolve the running lsd binary's own path: {e}") })?;
    let resolved_root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let command = exe.to_string_lossy().to_string();
    let args = vec!["--root".to_string(), resolved_root.to_string_lossy().to_string(), "mcp".to_string()];
    for config_file in &flags.positionals {
        install_mcp_entry(Path::new(config_file), &key, &name, &command, &args)?;
    }
    if flags.json {
        (io.out)(&print_value(&json!({ "key": key, "server": name, "command": command, "args": args, "files": flags.positionals }), true));
    } else {
        for config_file in &flags.positionals {
            (io.out)(&format!("installed mcp server \"{name}\" into {config_file}"));
        }
    }
    Ok(0)
}

/// Merges `<key>.<name>` (default key: `mcpServers`, the shape every standard MCP host config
/// shares — Claude Code's `.mcp.json`, Kiro's `.kiro/settings/mcp.json`) into `path`, touching
/// only `command`/`args` on that entry. `serde_json`'s `preserve_order` feature is load-bearing
/// here: without it, `Value`'s object type is a `BTreeMap` and silently alphabetizes every key in
/// the *entire* document on write, turning a one-entry change into a huge, unreviewable diff of a
/// human-maintained file (caught the hard way against a real file mid-development — see AGENTS.md).
fn install_mcp_entry(path: &Path, key: &str, name: &str, command: &str, args: &[String]) -> LocalctlResult<()> {
    let io_err = |context: String| move |e: std::io::Error| LocalctlError { exit_code: EXIT_FAILED, message: format!("{context}: {e}") };
    let mut document: Value = if path.exists() {
        let text = std::fs::read_to_string(path).map_err(io_err(format!("could not read {}", path.display())))?;
        serde_json::from_str(&text)
            .map_err(|e| LocalctlError { exit_code: EXIT_FAILED, message: format!("could not parse {} as JSON: {e}", path.display()) })?
    } else {
        json!({})
    };
    let not_an_object = |what: &str| LocalctlError { exit_code: EXIT_FAILED, message: format!("{}'s {what} is not a JSON object", path.display()) };
    let root_object = document.as_object_mut().ok_or_else(|| not_an_object("top level"))?;
    let servers = root_object.entry(key.to_string()).or_insert_with(|| json!({}));
    let servers_object = servers.as_object_mut().ok_or_else(|| not_an_object(&format!("\"{key}\"")))?;
    let entry = servers_object.entry(name.to_string()).or_insert_with(|| json!({}));
    let entry_object = entry.as_object_mut().ok_or_else(|| not_an_object(&format!("\"{key}.{name}\"")))?;
    entry_object.insert("command".to_string(), json!(command));
    entry_object.insert("args".to_string(), json!(args));
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent).map_err(io_err(format!("could not create {}", parent.display())))?;
    }
    let serialized = serde_json::to_string_pretty(&document).map_err(|e| LocalctlError { exit_code: EXIT_FAILED, message: e.to_string() })?;
    std::fs::write(path, serialized + "\n").map_err(io_err(format!("could not write {}", path.display())))?;
    Ok(())
}

/// A generic, project-agnostic doc describing the MCP tool contract and mutate-gate policy any
/// `@gnasdev/local-services`-backed project shares — deliberately doesn't mention a project's own
/// service list, ports, or Taskfile targets, since those vary per consumer and belong in that
/// project's own supplementary docs.
const SKILL_MARKDOWN: &str = include_str!("../skill/local-services-mcp.md");

async fn skill_command(root: &Path, rest: &[String], io: &mut Io<'_>) -> LocalctlResult<i32> {
    let Some(subcommand) = rest.first() else {
        return usage_err("usage: local-services skill install --dest <path>");
    };
    match subcommand.as_str() {
        "install" => skill_install_command(root, &rest[1..], io).await,
        other => usage_err(format!("unknown skill subcommand: {other}")),
    }
}

async fn skill_install_command(root: &Path, rest: &[String], io: &mut Io<'_>) -> LocalctlResult<i32> {
    let flags = parse_command_flags(rest, &[FlagName::Dest])?;
    if !flags.positionals.is_empty() {
        return usage_err("skill install takes no positional arguments");
    }
    let Some(dest) = flags.dest.clone() else {
        return usage_err("usage: local-services skill install --dest <path>");
    };
    let dest_path = Path::new(&dest);
    let resolved = if dest_path.is_absolute() { dest_path.to_path_buf() } else { root.join(dest_path) };
    if let Some(parent) = resolved.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| LocalctlError { exit_code: EXIT_FAILED, message: format!("could not create {}: {e}", parent.display()) })?;
    }
    std::fs::write(&resolved, SKILL_MARKDOWN)
        .map_err(|e| LocalctlError { exit_code: EXIT_FAILED, message: format!("could not write {}: {e}", resolved.display()) })?;
    (io.out)(&format!("installed skill doc at {}", resolved.display()));
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ls_core::catalog::{CommandSpec, ReadinessSpec, ServiceCommand, ServiceDefinition, ServiceKind, ServiceProfiles, ServiceRunProfile, StartFailurePolicy};
    use ls_core::manager::{bootstrap, LocalServicesManager, LocalServicesManagerOptions};
    use ls_core::supervisor::Host as _;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    // -----------------------------------------------------------------------------------------
    // parse_command_flags — a 1:1-ish port of localctl.test.ts's own flag-parsing coverage.
    // -----------------------------------------------------------------------------------------

    fn args(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parses_positionals_and_known_flags() {
        let flags = parse_command_flags(&args(&["start", "api", "--wait", "--json"]), &[FlagName::Wait, FlagName::Json]).unwrap();
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
    fn operation_id_accepts_the_unreserved_charset_and_rejects_others() {
        assert_eq!(operation_id("abc-123_.~").unwrap(), "abc-123_.~");
        assert!(operation_id("has a space").is_err());
        assert!(operation_id("").is_err());
        assert!(operation_id(&"x".repeat(129)).is_err());
    }

    // -----------------------------------------------------------------------------------------
    // Real end-to-end: a real bootstrapped LocalServicesManager, driven entirely through main().
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
            dependencies: None,
            profiles: ServiceProfiles {
                run: ServiceRunProfile::Verified {
                    command: ServiceCommand { command: CommandSpec::Shell { shell: format!("exec nc -lk {port}"), exec: Some(true) }, cwd: "/tmp".to_string(), environment: None, container_name: None, docker_stop_command: None },
                    readiness: ReadinessSpec::Tcp { port },
                    readiness_timeout_ms: Some(5_000),
                    preparation: None,
                    preparation_command: None,
                },
                build: None,
            },
            ports: None,
            urls: None,
        }
    }

    fn test_catalog(runtime_directory: &Path, services: Vec<ServiceDefinition>) -> ServiceCatalog {
        ServiceCatalog {
            services,
            groups: HashMap::new(),
            compose_file: None,
            runtime_directory: Some(runtime_directory.to_string_lossy().to_string()),
            start_failure_policy: StartFailurePolicy::StopOnFirstFailureKeepStarted,
            private_file_guard: Some(false),
        }
    }

    fn never_spawn() -> SpawnDaemon {
        Box::new(|_root| panic!("spawn_daemon should not be called when a manager is already running"))
    }

    async fn bootstrap_manager(catalog: ServiceCatalog) -> Arc<LocalServicesManager> {
        bootstrap(LocalServicesManagerOptions {
            runtime_directory: None,
            root: Some(PathBuf::from("/tmp")),
            catalog,
            event_capacity: None,
            log_tail_bytes: None,
            log_max_bytes: None,
            log_rotation_count: None,
            supervisor: None,
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
        let options = LocalctlOptions { catalog, spawn_daemon: never_spawn(), doctor_checks: None };
        let mut io = Io { out: &mut out_fn, err: &mut err_fn };
        let code = main(&options, &args(&["--root", "/tmp", "status", "--json"]), &mut io).await;
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
        let options = LocalctlOptions { catalog, spawn_daemon: never_spawn(), doctor_checks: None };
        let mut io = Io { out: &mut out_fn, err: &mut err_fn };
        let code = main(&options, &args(&["--root", "/tmp", "start", "api", "--wait", "--json"]), &mut io).await;
        assert_eq!(code, 0, "{:?}", out.lock().unwrap());
        let printed: Value = serde_json::from_str(out.lock().unwrap().last().unwrap()).unwrap();
        assert_eq!(printed["operations"][0]["status"], "succeeded");
        assert_eq!(manager.service_states()[0].actual_state, ls_core::state::ActualServiceState::Ready);
        manager.close().await;
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
        let options = LocalctlOptions { catalog, spawn_daemon: never_spawn(), doctor_checks: None };
        let mut io = Io { out: &mut out_fn, err: &mut err_fn };
        let code = main(&options, &args(&["--root", "/tmp", "manager", "ensure", "--json"]), &mut io).await;
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
                ls_core::daemon::run_daemon(LocalServicesManagerOptions { runtime_directory: None, root: Some(root), catalog, event_capacity: None, log_tail_bytes: None, log_max_bytes: None, log_rotation_count: None, supervisor: None }, false).await;
            });
        });
        let options = LocalctlOptions { catalog, spawn_daemon, doctor_checks: None };
        let client = ensure(&root, &options).await.unwrap();
        assert!(client.metadata.port > 0);
        // Clean up: ask the real daemon we just spawned to stop.
        let _ = request(&client, "/v1/manager/shutdown", reqwest::Method::POST, Some(&json!({"requestId": uuid::Uuid::new_v4().to_string(), "mode": "stop-services"})), Some(client.metadata.protocol_version)).await;
    }

    #[tokio::test]
    async fn manager_stop_waits_for_the_daemon_process_to_exit_and_fails_if_it_never_does() {
        let checks = std::sync::atomic::AtomicUsize::new(0);
        let waited = wait_for_daemon_exit(999_999, Duration::from_secs(5), |_| checks.fetch_add(1, std::sync::atomic::Ordering::SeqCst) < 2).await;
        assert!(waited.is_ok());
        assert_eq!(checks.load(std::sync::atomic::Ordering::SeqCst), 3);

        let stuck = wait_for_daemon_exit(999_999, Duration::from_millis(300), |_| true).await;
        let err = stuck.expect_err("a daemon that never exits must fail the stop");
        assert!(format!("{err:?}").contains("did not exit within"), "{err:?}");

        // Our own pid is an in-process daemon: never waited on, even though it is alive.
        assert!(wait_for_daemon_exit(i64::from(std::process::id()), Duration::from_millis(1), |_| true).await.is_ok());
    }

    #[tokio::test]
    async fn doctor_reports_platform_check() {
        let dir = tempfile::tempdir().unwrap();
        let catalog = test_catalog(dir.path(), vec![]);
        let out = Arc::new(Mutex::new(Vec::new()));
        let out2 = out.clone();
        let mut out_fn = move |s: &str| out2.lock().unwrap().push(s.to_string());
        let mut err_fn = move |_: &str| {};
        let options = LocalctlOptions { catalog, spawn_daemon: never_spawn(), doctor_checks: None };
        let mut io = Io { out: &mut out_fn, err: &mut err_fn };
        let code = main(&options, &args(&["--root", "/tmp", "doctor"]), &mut io).await;
        assert_eq!(code, 0);
        assert!(out.lock().unwrap().iter().any(|line| line.contains("platform")));
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
        let options = LocalctlOptions { catalog, spawn_daemon: never_spawn(), doctor_checks: None };
        let mut io = Io { out: &mut out_fn, err: &mut err_fn };
        let code = main(&options, &args(&["--root", "/tmp", "bogus"]), &mut io).await;
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
        let options = LocalctlOptions { catalog, spawn_daemon: never_spawn(), doctor_checks: None };
        let mut io = Io { out: &mut out_fn, err: &mut err_fn };
        let code = main(&options, &args(&["--root", "/tmp", "status"]), &mut io).await;
        assert_eq!(code, EXIT_UNAVAILABLE);
        assert!(err.lock().unwrap()[0].contains("unavailable"));
    }

    // -----------------------------------------------------------------------------------------
    // mcp install / skill install
    // -----------------------------------------------------------------------------------------

    async fn run_cli(dir: &Path, extra_args: &[&str]) -> (i32, Vec<String>, Vec<String>) {
        let catalog = test_catalog(dir, vec![]);
        let options = LocalctlOptions { catalog, spawn_daemon: never_spawn(), doctor_checks: None };
        let out = Arc::new(Mutex::new(Vec::new()));
        let err = Arc::new(Mutex::new(Vec::new()));
        let out2 = out.clone();
        let err2 = err.clone();
        let mut out_fn = move |s: &str| out2.lock().unwrap().push(s.to_string());
        let mut err_fn = move |s: &str| err2.lock().unwrap().push(s.to_string());
        let mut io = Io { out: &mut out_fn, err: &mut err_fn };
        let mut full_args: Vec<&str> = vec!["--root", dir.to_str().unwrap()];
        full_args.extend_from_slice(extra_args);
        let code = main(&options, &args(&full_args), &mut io).await;
        let out = out.lock().unwrap().clone();
        let err = err.lock().unwrap().clone();
        (code, out, err)
    }

    #[tokio::test]
    async fn mcp_install_creates_a_new_config_file_with_the_resolved_lsd_path() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("mcp.json");
        let (code, out, _err) = run_cli(dir.path(), &["mcp", "install", config_path.to_str().unwrap()]).await;
        assert_eq!(code, 0);
        assert!(out[0].contains("installed mcp server \"local-services\""));

        let written: Value = serde_json::from_str(&std::fs::read_to_string(&config_path).unwrap()).unwrap();
        let entry = &written["mcpServers"]["local-services"];
        let expected_exe = std::env::current_exe().unwrap().to_string_lossy().to_string();
        assert_eq!(entry["command"], json!(expected_exe));
        let expected_root = std::fs::canonicalize(dir.path()).unwrap().to_string_lossy().to_string();
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
                    "local-services": {
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

        let (code, _out, _err) = run_cli(dir.path(), &["mcp", "install", config_path.to_str().unwrap()]).await;
        assert_eq!(code, 0);

        let written: Value = serde_json::from_str(&std::fs::read_to_string(&config_path).unwrap()).unwrap();
        assert_eq!(written["mcpServers"]["other-server"]["command"], json!("node"));
        assert_eq!(written["mcpServers"]["other-server"]["args"], json!(["other.mjs"]));
        let entry = &written["mcpServers"]["local-services"];
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
    "local-services": { "command": "node", "args": ["old-wrapper.mjs"] },
    "apple-server": { "command": "node", "args": ["a.mjs"] }
  }
}"#,
        )
        .unwrap();

        let (code, _out, _err) = run_cli(dir.path(), &["mcp", "install", config_path.to_str().unwrap()]).await;
        assert_eq!(code, 0);

        let written = std::fs::read_to_string(&config_path).unwrap();
        let zebra_pos = written.find("zebra-server").unwrap();
        let local_pos = written.find("\"local-services\"").unwrap();
        let apple_pos = written.find("apple-server").unwrap();
        assert!(zebra_pos < local_pos && local_pos < apple_pos, "key order was not preserved:\n{written}");
    }

    #[tokio::test]
    async fn mcp_install_supports_a_custom_top_level_key() {
        // infra's own .agent/mcp.json uses "servers" as its top-level key, not the standard
        // "mcpServers" every plain MCP host config shares — --key makes that schema installable
        // through the same generic merge instead of a repo-specific special case in this binary.
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("agent-mcp.json");
        std::fs::write(&config_path, json!({ "servers": { "local-services": { "skill": "local-dev", "command": "node", "args": ["old.mjs"] } } }).to_string()).unwrap();

        let (code, _out, _err) = run_cli(dir.path(), &["mcp", "install", "--key", "servers", config_path.to_str().unwrap()]).await;
        assert_eq!(code, 0);

        let written: Value = serde_json::from_str(&std::fs::read_to_string(&config_path).unwrap()).unwrap();
        assert!(written.get("mcpServers").is_none(), "must not create a stray \"mcpServers\" key when --key targets a different one");
        let entry = &written["servers"]["local-services"];
        assert_eq!(entry["skill"], json!("local-dev"));
        assert_ne!(entry["args"], json!(["old.mjs"]));
    }

    #[tokio::test]
    async fn mcp_install_supports_a_custom_name_and_multiple_files() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.json");
        let b = dir.path().join("nested/b.json");
        let (code, out, _err) =
            run_cli(dir.path(), &["mcp", "install", "--name", "viclass-local-services", a.to_str().unwrap(), b.to_str().unwrap()]).await;
        assert_eq!(code, 0);
        assert_eq!(out.len(), 2);
        for path in [&a, &b] {
            let written: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
            assert!(written["mcpServers"]["viclass-local-services"]["command"].is_string());
        }
    }

    #[tokio::test]
    async fn mcp_install_requires_at_least_one_config_file() {
        let dir = tempfile::tempdir().unwrap();
        let (code, _out, err) = run_cli(dir.path(), &["mcp", "install"]).await;
        assert_eq!(code, EXIT_USAGE);
        assert!(err[0].contains("usage: local-services mcp install"));
    }

    #[tokio::test]
    async fn mcp_rejects_an_unknown_subcommand() {
        let dir = tempfile::tempdir().unwrap();
        let (code, _out, err) = run_cli(dir.path(), &["mcp", "bogus"]).await;
        assert_eq!(code, EXIT_USAGE);
        assert!(err[0].contains("unknown mcp subcommand"));
    }

    #[tokio::test]
    async fn skill_install_writes_the_generic_doc_at_a_relative_dest() {
        let dir = tempfile::tempdir().unwrap();
        let (code, out, _err) = run_cli(dir.path(), &["skill", "install", "--dest", ".agent/skills/local-dev/SKILL.md"]).await;
        assert_eq!(code, 0);
        assert!(out[0].contains("installed skill doc"));
        let written = std::fs::read_to_string(dir.path().join(".agent/skills/local-dev/SKILL.md")).unwrap();
        assert_eq!(written, SKILL_MARKDOWN);
        assert!(written.contains("local_services_manage"));
    }

    #[tokio::test]
    async fn skill_install_requires_a_dest_flag() {
        let dir = tempfile::tempdir().unwrap();
        let (code, _out, err) = run_cli(dir.path(), &["skill", "install"]).await;
        assert_eq!(code, EXIT_USAGE);
        assert!(err[0].contains("usage: local-services skill install"));
    }

    /// A failed service must not print as "stopped" — that made a crash indistinguishable from a
    /// service nobody started, while every other surface (TUI, macOS app, MCP) reported it failed.
    #[test]
    fn text_state_never_hides_a_failure_as_stopped() {
        use ls_core::state::{DesiredServiceState, ServiceReadiness};
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
            (ActualServiceState::Stopping, "stopping"),
            (ActualServiceState::Failed, "failed"),
            (ActualServiceState::Orphaned, "orphaned"),
            (ActualServiceState::ExternallyOwned, "externally-owned"),
        ];
        assert_eq!(expected.len(), ActualServiceState::ALL.len(), "a new state needs an explicit CLI rendering");
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
            ls_core::catalog::ServiceUrl { url: "http://127.0.0.1:18080/".into(), label: Some("app".into()), requires_running: None },
            ls_core::catalog::ServiceUrl { url: "http://127.0.0.1:18081/".into(), label: None, requires_running: Some(false) },
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
                let options = LocalctlOptions { catalog, spawn_daemon: never_spawn(), doctor_checks: None };
                let mut io = Io { out: &mut out_fn, err: &mut err_fn };
                let mut argv = vec!["--root", "/tmp", "urls"];
                argv.extend_from_slice(extra);
                let code = main(&options, &args(&argv), &mut io).await;
                let lines = out.lock().unwrap().clone();
                (code, lines)
            }
        };

        let (code, lines) = run(&[]).await;
        assert_eq!(code, 0, "{lines:?}");
        assert_eq!(lines, vec!["api  app  http://127.0.0.1:18080/  (not running)".to_string(), "api  -  http://127.0.0.1:18081/".to_string()]);

        let (_, lines) = run(&["api", "--json"]).await;
        let printed: Value = serde_json::from_str(&lines[0]).unwrap();
        assert_eq!(printed["urls"][0]["running"], json!(false));
        assert_eq!(printed["urls"].as_array().unwrap().len(), 2);
        manager.close().await;
    }
}
