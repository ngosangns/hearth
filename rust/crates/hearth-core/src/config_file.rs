//! Declarative catalog authoring — port of `src/core/config-file.ts`. A `hearth.yaml` (or
//! `.yml`/`.json`) file sitting in a project's root is mapped onto the same `ServiceCatalog` every
//! other entry point takes. TypeScript catalogs (`hearth.config.ts`) are not accepted.
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::catalog::{
    validate_catalog, CommandSpec, ReadinessSpec, ServiceCatalog, ServiceDefinition, ServiceKind,
    ServiceOwnership, ServicePort, ServiceProfiles, ServiceRunProfile, ServiceUrl, StartFailurePolicy,
};
use crate::env::load_env_file;

pub const CONFIG_FILE_NAMES: [&str; 3] = ["hearth.yaml", "hearth.yml", "hearth.json"];

#[derive(Debug, Clone)]
pub struct LoadedCatalog {
    pub catalog: ServiceCatalog,
    pub path: PathBuf,
}

#[derive(Debug, Clone, thiserror::Error)]
#[error("{}", errors.join("; "))]
pub struct ConfigFileLoadError {
    pub path: Option<PathBuf>,
    pub errors: Vec<String>,
}

/// First existing candidate in `CONFIG_FILE_NAMES` order, or `None` if none exists.
pub fn find_config_file(root: &Path) -> Option<PathBuf> {
    CONFIG_FILE_NAMES.iter().map(|name| root.join(name)).find(|path| path.exists())
}

pub fn load_catalog(root: &Path) -> Result<LoadedCatalog, ConfigFileLoadError> {
    let path = find_config_file(root).ok_or_else(|| {
        let mut message = format!(
            "no config file found in {} (looked for {})",
            root.display(),
            CONFIG_FILE_NAMES.join(", ")
        );
        if root.join("hearth.config.ts").exists() {
            message.push_str("; hearth.config.ts is no longer accepted — author a hearth.yaml instead");
        }
        ConfigFileLoadError { path: None, errors: vec![message] }
    })?;
    load_catalog_from_file(&path, root)
}

pub fn load_catalog_from_file(path: &Path, root: &Path) -> Result<LoadedCatalog, ConfigFileLoadError> {
    if path.extension().and_then(|e| e.to_str()) == Some("ts") {
        return Err(ConfigFileLoadError {
            path: Some(path.to_path_buf()),
            errors: vec![format!(
                "{} is a TypeScript catalog; only hearth.yaml, .yml, or .json are accepted",
                path.display()
            )],
        });
    }

    let text = std::fs::read_to_string(path).map_err(|e| ConfigFileLoadError {
        path: Some(path.to_path_buf()),
        errors: vec![format!("failed to read {}: {e}", path.display())],
    })?;

    let is_yaml = matches!(path.extension().and_then(|e| e.to_str()), Some("yaml") | Some("yml"));
    let raw: Value = if is_yaml {
        // JSON is valid YAML, so this one parser handles both .yaml/.yml and any .json file that
        // happens to reach this branch — mirrors `Bun.YAML.parse` being used for both in the TS
        // source. `.json` files are still routed through `serde_json` below for a stricter parse.
        let yaml_value: serde_yaml::Value = serde_yaml::from_str(&text).map_err(|e| ConfigFileLoadError {
            path: Some(path.to_path_buf()),
            errors: vec![format!("failed to parse {}: {e}", path.display())],
        })?;
        serde_json::to_value(yaml_value).map_err(|e| ConfigFileLoadError {
            path: Some(path.to_path_buf()),
            errors: vec![format!("failed to parse {}: {e}", path.display())],
        })?
    } else {
        serde_json::from_str(&text).map_err(|e| ConfigFileLoadError {
            path: Some(path.to_path_buf()),
            errors: vec![format!("failed to parse {}: {e}", path.display())],
        })?
    };

    let catalog = map_config_file(&raw, root, path).map_err(|errors| ConfigFileLoadError {
        path: Some(path.to_path_buf()),
        errors,
    })?;
    let validation = validate_catalog(&catalog);
    if !validation.errors.is_empty() {
        return Err(ConfigFileLoadError { path: Some(path.to_path_buf()), errors: validation.errors });
    }
    Ok(LoadedCatalog { catalog, path: path.to_path_buf() })
}

// -------------------------------------------------------------------------------------------
// Declarative shape -> ServiceCatalog
// -------------------------------------------------------------------------------------------

const READINESS_KINDS: [&str; 6] = ["process", "tcp", "http", "container", "tailnet", "command"];
const SERVICE_KINDS: [&str; 2] = ["application", "infrastructure"];
const OWNERSHIPS: [&str; 2] = ["daemon", "external"];

fn is_string_array(value: &Value) -> bool {
    value.as_array().map(|a| a.iter().all(Value::is_string)).unwrap_or(false)
}

fn is_string_record(value: &Value) -> bool {
    value.as_object().map(|o| o.values().all(Value::is_string)).unwrap_or(false)
}

fn string_record(value: &Value) -> HashMap<String, String> {
    value
        .as_object()
        .map(|o| {
            o.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect()
        })
        .unwrap_or_default()
}

fn string_array(value: &Value) -> Vec<String> {
    value
        .as_array()
        .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
        .unwrap_or_default()
}

/// JS's `String(x)` for the scalar types the argv-coercion sharp edge accepts (string/number/bool).
fn scalar_to_string(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(s.clone()),
        Value::Bool(b) => Some(b.to_string()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

struct ReadCommand {
    spec: CommandSpec,
}

/// A raw config-file command (`{argv}` or `{shell}`, same shape as `CommandSpec`) at `path`, or
/// `None` with an error pushed to `errors` if present-but-malformed. Absent is valid — callers
/// decide whether that means "no command" or "required".
fn read_command_spec(value: &Value, path: &str, errors: &mut Vec<String>) -> Option<ReadCommand> {
    let Some(obj) = value.as_object() else {
        errors.push(format!("{path} must be an object with `argv` or `shell`"));
        return None;
    };
    let has_argv = obj.contains_key("argv");
    let has_shell = obj.contains_key("shell");
    if has_argv == has_shell {
        errors.push(format!("{path} must set exactly one of `argv` or `shell`"));
        return None;
    }
    if has_argv {
        // A bare numeric/boolean argv element (`argv: [sleep, 30]`) parses as a YAML number/boolean,
        // not a string — extremely easy to author by accident (port numbers, `sleep 30`) and always
        // safe to coerce, since every argv element ends up as a string on the spawn call regardless.
        let argv_value = &obj["argv"];
        let argv: Option<Vec<String>> = argv_value.as_array().and_then(|items| {
            items.iter().map(scalar_to_string).collect::<Option<Vec<String>>>()
        });
        match argv {
            Some(argv) if !argv.is_empty() => Some(ReadCommand { spec: CommandSpec::Argv { argv } }),
            _ => {
                errors.push(format!(
                    "{path}.argv must be a non-empty array of strings (numbers/booleans are coerced to strings)"
                ));
                None
            }
        }
    } else {
        let shell = obj.get("shell").and_then(Value::as_str);
        let Some(shell) = shell.filter(|s| !s.trim().is_empty()) else {
            errors.push(format!("{path}.shell must be a non-empty string"));
            return None;
        };
        let exec = match obj.get("exec") {
            None => None,
            Some(Value::Bool(b)) => Some(*b),
            Some(_) => {
                errors.push(format!("{path}.exec must be a boolean"));
                return None;
            }
        };
        Some(ReadCommand { spec: CommandSpec::Shell { shell: shell.to_string(), exec } })
    }
}

fn read_readiness(value: &Value, path: &str, errors: &mut Vec<String>) -> Option<ReadinessSpec> {
    let obj = value.as_object();
    let kind = obj.and_then(|o| o.get("kind")).and_then(Value::as_str);
    let Some(kind) = kind else {
        errors.push(format!("{path}.kind is required (one of {})", READINESS_KINDS.join(", ")));
        return None;
    };
    if !READINESS_KINDS.contains(&kind) {
        errors.push(format!(
            "{path}.kind must be one of {}, got {:?}",
            READINESS_KINDS.join(", "),
            kind
        ));
        return None;
    }
    let obj = obj.unwrap();
    match kind {
        "process" => Some(ReadinessSpec::Process),
        "container" => Some(ReadinessSpec::Container),
        "tailnet" => Some(ReadinessSpec::Tailnet),
        "tcp" => {
            let port = obj.get("port").and_then(Value::as_i64);
            match port {
                Some(p) if p > 0 && p <= u16::MAX as i64 => Some(ReadinessSpec::Tcp { port: p as u16 }),
                _ => {
                    errors.push(format!("{path}.port must be a positive integer"));
                    None
                }
            }
        }
        "http" => {
            let url = obj.get("url").and_then(Value::as_str).filter(|s| !s.is_empty());
            match url {
                Some(url) => Some(ReadinessSpec::Http { url: url.to_string() }),
                None => {
                    errors.push(format!("{path}.url must be a non-empty string"));
                    None
                }
            }
        }
        _ => {
            // kind === "command"
            let default_command = Value::Null;
            let command = read_command_spec(obj.get("command").unwrap_or(&default_command), &format!("{path}.command"), errors)?;
            let cwd = match obj.get("cwd") {
                None => None,
                Some(Value::String(s)) => Some(s.clone()),
                Some(_) => {
                    errors.push(format!("{path}.cwd must be a string"));
                    return None;
                }
            };
            Some(ReadinessSpec::Command { command: command.spec, cwd })
        }
    }
}

fn read_preparation_command(value: &Value, path: &str, errors: &mut Vec<String>) -> Option<crate::catalog::PreparationCommand> {
    let Some(obj) = value.as_object() else {
        errors.push(format!("{path} must be an object with `command` (and optional `cwd`)"));
        return None;
    };
    let command = read_command_spec(obj.get("command").unwrap_or(&Value::Null), &format!("{path}.command"), errors);
    let cwd = match obj.get("cwd") {
        None => None,
        Some(Value::String(s)) => Some(s.clone()),
        Some(_) => {
            errors.push(format!("{path}.cwd must be a string"));
            None
        }
    };
    let serialization_key = match obj.get("serializationKey") {
        None => None,
        Some(Value::String(s)) => Some(s.clone()),
        Some(_) => {
            errors.push(format!("{path}.serializationKey must be a string"));
            None
        }
    };
    Some(crate::catalog::PreparationCommand { command: command?.spec, cwd, serialization_key })
}

/// `urls` accepts a bare string or `{ url, label?, requiresRunning? }` per entry. Only the shape is
/// checked here; the URL format and its placeholders are checked by `validate_catalog`, which runs
/// for every catalog source, not just config files. Mirrors `readUrls` in the TS source.
fn read_urls(value: Option<&Value>, path: &str, errors: &mut Vec<String>) -> Option<Vec<ServiceUrl>> {
    let Some(value) = value else { return Some(Vec::new()) };
    let Some(array) = value.as_array() else {
        errors.push(format!("{path} must be an array"));
        return None;
    };
    let mut urls = Vec::new();
    for (index, entry) in array.iter().enumerate() {
        let entry_path = format!("{path}[{index}]");
        if let Some(url) = entry.as_str() {
            urls.push(ServiceUrl { url: url.to_string(), label: None, requires_running: None });
            continue;
        }
        let Some(url) = entry.as_object().and_then(|o| o.get("url")).and_then(Value::as_str) else {
            errors.push(format!("{entry_path} must be a URL string or {{ url: string, label?: string, requiresRunning?: boolean }}"));
            continue;
        };
        let obj = entry.as_object().expect("checked above");
        let label = match obj.get("label") {
            None => None,
            Some(Value::String(label)) => Some(label.clone()),
            Some(_) => {
                errors.push(format!("{entry_path}.label must be a string"));
                continue;
            }
        };
        let requires_running = match obj.get("requiresRunning") {
            None => None,
            Some(Value::Bool(b)) => Some(*b),
            Some(_) => {
                errors.push(format!("{entry_path}.requiresRunning must be a boolean"));
                continue;
            }
        };
        urls.push(ServiceUrl { url: url.to_string(), label, requires_running });
    }
    Some(urls)
}

fn read_ports(value: Option<&Value>, path: &str, errors: &mut Vec<String>) -> Option<Vec<ServicePort>> {
    let Some(value) = value else { return Some(Vec::new()) };
    let Some(array) = value.as_array() else {
        errors.push(format!("{path} must be an array"));
        return None;
    };
    let mut ports = Vec::new();
    for (index, entry) in array.iter().enumerate() {
        let entry_path = format!("{path}[{index}]");
        let obj = entry.as_object();
        let port = obj.and_then(|o| o.get("port")).and_then(Value::as_i64);
        let label = obj.and_then(|o| o.get("label")).and_then(Value::as_str).filter(|s| !s.is_empty());
        let (Some(port), Some(label)) = (port, label) else {
            errors.push(format!("{entry_path} must be {{ port: number, label: string }}"));
            continue;
        };
        // An out-of-range port is an ERROR, not something to silently wrap into a u16. `70000 as
        // u16` is 4464 — the catalog would have loaded, advertised a port nobody asked for, and
        // the mistake would only surface as a confusing conflict much later.
        if !(1..=u16::MAX as i64).contains(&port) {
            errors.push(format!("{entry_path}.port must be between 1 and 65535, got {port}"));
            continue;
        }
        let requires_running = match obj.and_then(|o| o.get("requiresRunning")) {
            None => None,
            Some(Value::Bool(b)) => Some(*b),
            Some(_) => {
                errors.push(format!("{entry_path}.requiresRunning must be a boolean"));
                continue;
            }
        };
        ports.push(ServicePort { port: port as u16, label: label.to_string(), requires_running });
    }
    Some(ports)
}

/// `cwd` is authored relative to the project root and must stay inside it. This is a lexical check
/// on the resolved path, not a symlink-escape defense.
fn resolve_service_cwd(cwd: Option<&str>, path: &str, errors: &mut Vec<String>) -> Option<String> {
    let relative_cwd = cwd.unwrap_or(".");
    if Path::new(relative_cwd).is_absolute() {
        errors.push(format!("{path}.cwd must be a relative path, got {relative_cwd}"));
        return None;
    }
    let mut depth: i64 = 0;
    for component in Path::new(relative_cwd).components() {
        match component {
            std::path::Component::ParentDir => depth -= 1,
            std::path::Component::Normal(_) => depth += 1,
            _ => {}
        }
        if depth < 0 {
            errors.push(format!("{path}.cwd escapes the project root: {relative_cwd}"));
            return None;
        }
    }
    Some(relative_cwd.to_string())
}

/// A key outside these sets is a typo (e.g. `command:` for `run:`), which would otherwise be silently
/// dropped and surface much later as an unrelated error. `x-`-prefixed keys stay free for YAML anchors.
const TOP_LEVEL_KEYS: &[&str] = &["version", "env", "envFile", "runtimeDirectory", "privateFileGuard", "groups", "services", "shared"];
const SERVICE_KEYS: &[&str] = &[
    "label", "kind", "ownership", "env", "container", "cwd", "run", "stop", "build", "readiness", "readinessTimeoutMs", "preparationCommand", "ports", "urls",
];

fn read_positive_u64(value: Option<&Value>, path: &str, errors: &mut Vec<String>) -> Option<u64> {
    match value {
        None => None,
        Some(v) => match v.as_i64() {
            Some(n) if n > 0 => Some(n as u64),
            _ => {
                errors.push(format!("{path} must be a positive integer"));
                None
            }
        },
    }
}

fn check_known_keys(value: &serde_json::Map<String, Value>, known: &[&str], path: &str, errors: &mut Vec<String>) {
    for key in value.keys() {
        if !known.contains(&key.as_str()) && !key.starts_with("x-") {
            errors.push(format!("{path} has unknown key \"{key}\" (known: {})", known.join(", ")));
        }
    }
}

/// `shared:` keys become both service ids and smp instance names — restrict to the charset both
/// sides can carry safely (also a valid filename under `installs/`).
fn is_valid_shared_name(name: &str) -> bool {
    !name.is_empty()
        && name.chars().next().is_some_and(|c| c.is_ascii_alphanumeric())
        && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

fn map_config_file(raw: &Value, root: &Path, _path: &Path) -> Result<ServiceCatalog, Vec<String>> {
    let mut errors: Vec<String> = Vec::new();
    let Some(top) = raw.as_object() else {
        return Err(vec!["config file must contain a YAML/JSON object".to_string()]);
    };

    check_known_keys(top, TOP_LEVEL_KEYS, "config file", &mut errors);
    if top.get("version").and_then(Value::as_i64) != Some(1) {
        errors.push(format!("version must be 1, got {}", top.get("version").cloned().unwrap_or(Value::Null)));
    }
    if let Some(env) = top.get("env") {
        if !is_string_record(env) {
            errors.push("env must be a map of string to string".to_string());
        }
    }
    if let Some(env_file) = top.get("envFile") {
        if !env_file.is_string() {
            errors.push("envFile must be a string".to_string());
        }
    }
    if let Some(rd) = top.get("runtimeDirectory") {
        if !rd.is_string() {
            errors.push("runtimeDirectory must be a string".to_string());
        }
    }
    if let Some(pfg) = top.get("privateFileGuard") {
        if !pfg.is_boolean() {
            errors.push("privateFileGuard must be a boolean".to_string());
        }
    }
    if let Some(groups) = top.get("groups") {
        let ok = groups.as_object().map(|g| g.values().all(is_string_array)).unwrap_or(false);
        if !ok {
            errors.push("groups must be a map of string to string[]".to_string());
        }
    }
    if let Some(shared) = top.get("shared") {
        match shared.as_object() {
            None => errors.push("shared must be a map of service name to version (or { version })".to_string()),
            Some(entries) => {
                for (name, value) in entries {
                    let path = format!("shared.{name}");
                    if !is_valid_shared_name(name) {
                        errors.push(format!("{path} has an invalid service name (expected [a-zA-Z0-9][a-zA-Z0-9._-]*)"));
                    }
                    match value {
                        Value::String(_) => {}
                        Value::Object(o) => match o.get("version") {
                            Some(Value::String(s)) if !s.is_empty() => {}
                            _ => errors.push(format!("{path}.version must be a non-empty string")),
                        },
                        _ => errors.push(format!("{path} must be a version string or an object with version")),
                    }
                }
            }
        }
    }
    let services_raw = top.get("services").and_then(Value::as_object);
    let shared_raw = top.get("shared").and_then(Value::as_object);
    if services_raw.map(|s| s.is_empty()).unwrap_or(true) && shared_raw.map(|s| s.is_empty()).unwrap_or(true) {
        errors.push("services must be a non-empty map of service id to service definition".to_string());
    }
    if !errors.is_empty() {
        return Err(errors);
    }

    let global_env = top.get("env").map(string_record).unwrap_or_default();
    let file_env = match top.get("envFile").and_then(Value::as_str) {
        Some(env_file) => {
            let path = if Path::new(env_file).is_absolute() { PathBuf::from(env_file) } else { root.join(env_file) };
            load_env_file(&path)
        }
        None => HashMap::new(),
    };
    let mut base_env = file_env;
    base_env.extend(global_env);

    let mut services: Vec<ServiceDefinition> = Vec::new();
    // May be absent/empty when a project only declares `shared:` services — allowed above.
    let services_raw = services_raw.cloned().unwrap_or_default();
    // Document order, not sorted: the TS loader iterates `Object.entries` (insertion order), and
    // this order is user-visible — it drives `/v1/catalog`, `hearthd status` row order, and the TUI
    // list. Sorting here made the same YAML file present differently depending on which
    // implementation read it. `serde_json`'s `preserve_order` feature (enabled workspace-wide) is
    // what makes `keys()` document-ordered.
    for id in services_raw.keys() {
        let value = &services_raw[id];
        let svc_path = format!("services.{id}");
        let Some(obj) = value.as_object() else {
            errors.push(format!("{svc_path} must be an object"));
            continue;
        };
        check_known_keys(obj, SERVICE_KEYS, &svc_path, &mut errors);

        let kind: Option<ServiceKind> = match obj.get("kind") {
            None => None,
            Some(Value::String(s)) if s == "application" => Some(ServiceKind::Application),
            Some(Value::String(s)) if s == "infrastructure" => Some(ServiceKind::Infrastructure),
            Some(_) => {
                errors.push(format!("{svc_path}.kind must be one of {}", SERVICE_KINDS.join(", ")));
                None
            }
        };
        let ownership: Option<ServiceOwnership> = match obj.get("ownership") {
            None => None,
            Some(Value::String(s)) if s == "daemon" => Some(ServiceOwnership::Daemon),
            Some(Value::String(s)) if s == "external" => Some(ServiceOwnership::External),
            Some(_) => {
                errors.push(format!("{svc_path}.ownership must be one of {}", OWNERSHIPS.join(", ")));
                None
            }
        };
        if let Some(env) = obj.get("env") {
            if !is_string_record(env) {
                errors.push(format!("{svc_path}.env must be a map of string to string"));
            }
        }
        let container = match obj.get("container") {
            None => None,
            Some(Value::String(s)) => Some(s.clone()),
            Some(_) => {
                errors.push(format!("{svc_path}.container must be a string"));
                None
            }
        };

        let cwd = resolve_service_cwd(obj.get("cwd").and_then(Value::as_str), &svc_path, &mut errors);
        // Unconditional, even when the key is absent: `read_readiness` is what reports
        // "readiness.kind is required", and an `and_then` on `get("readiness")` would swallow that
        // — the service would then be skipped below with no error at all, silently vanishing from
        // a catalog that otherwise loaded fine.
        let readiness = read_readiness(obj.get("readiness").unwrap_or(&Value::Null), &format!("{svc_path}.readiness"), &mut errors);
        let ports = read_ports(obj.get("ports"), &format!("{svc_path}.ports"), &mut errors);
        let urls = read_urls(obj.get("urls"), &format!("{svc_path}.urls"), &mut errors);
        let readiness_timeout_ms = read_positive_u64(obj.get("readinessTimeoutMs"), &format!("{svc_path}.readinessTimeoutMs"), &mut errors);
        let preparation_command = obj.get("preparationCommand").and_then(|v| read_preparation_command(v, &format!("{svc_path}.preparationCommand"), &mut errors));

        let mut profile_run: Option<ServiceRunProfile> = None;
        match obj.get("run") {
            None => {
                if let Some(readiness) = readiness.clone() {
                    profile_run = Some(ServiceRunProfile::Unresolved { readiness, readiness_timeout_ms, preparation: None, preparation_command: preparation_command.clone() });
                }
            }
            Some(run_value) => {
                let run = read_command_spec(run_value, &format!("{svc_path}.run"), &mut errors);
                let stop = obj.get("stop").and_then(|s| read_command_spec(s, &format!("{svc_path}.stop"), &mut errors));
                if let (Some(run), Some(readiness), Some(cwd)) = (run, readiness.clone(), cwd.clone()) {
                    let service_env = obj.get("env").map(string_record).unwrap_or_default();
                    let mut environment = base_env.clone();
                    environment.extend(service_env);
                    profile_run = Some(ServiceRunProfile::Verified {
                        command: crate::catalog::ServiceCommand {
                            command: run.spec,
                            cwd,
                            environment: if environment.is_empty() { None } else { Some(environment) },
                            container_name: container.clone(),
                            docker_stop_command: stop.map(|s| s.spec),
                        },
                        readiness,
                        readiness_timeout_ms,
                        preparation: None,
                        preparation_command,
                    });
                }
            }
        }

        let mut profile_build = None;
        if let Some(build_value) = obj.get("build") {
            if let Some(build_obj) = build_value.as_object() {
                let build = read_command_spec(build_value, &format!("{svc_path}.build"), &mut errors);
                let timeout_ms = match build_obj.get("timeoutMs") {
                    None => None,
                    Some(v) => match v.as_i64() {
                        Some(n) if n > 0 => Some(n as u64),
                        _ => {
                            errors.push(format!("{svc_path}.build.timeoutMs must be a positive integer"));
                            None
                        }
                    },
                };
                let serialization_key = match build_obj.get("serializationKey") {
                    None => None,
                    Some(Value::String(s)) => Some(s.clone()),
                    Some(_) => {
                        errors.push(format!("{svc_path}.build.serializationKey must be a string"));
                        None
                    }
                };
                if let (Some(build), Some(cwd)) = (build, cwd.clone()) {
                    profile_build = Some(crate::catalog::ServiceBuildProfile {
                        command: crate::catalog::ServiceCommand { command: build.spec, cwd, environment: None, container_name: None, docker_stop_command: None },
                        timeout_ms,
                        serialization_key,
                    });
                }
            } else {
                errors.push(format!("{svc_path}.build must be an object"));
            }
        }

        let Some(profile_run) = profile_run else {
            // Already recorded a more specific error above (bad run/readiness/cwd).
            continue;
        };
        services.push(ServiceDefinition {
            id: id.clone(),
            label: obj.get("label").and_then(Value::as_str).map(str::to_string),
            kind,
            ownership,
            profiles: ServiceProfiles { run: profile_run, build: profile_build },
            ports: ports.filter(|p| !p.is_empty()),
            urls: urls.filter(|u| !u.is_empty()),
        });
    }

    // `shared:` entries expand into generated `ownership: external` services (document order,
    // after the project's own services). A key colliding with a `services` id fails validation
    // through the existing `duplicate service` check — both lists share `services` below.
    let mut shared_ids: Vec<String> = Vec::new();
    if let Some(entries) = top.get("shared").and_then(Value::as_object) {
        let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("hearthd"));
        for (name, value) in entries {
            let version = match value {
                Value::String(s) => s.clone(),
                Value::Object(o) => o.get("version").and_then(Value::as_str).unwrap_or_default().to_string(),
                _ => continue, // shape errors already recorded above
            };
            if !is_valid_shared_name(name) || version.is_empty() {
                continue;
            }
            let instance = crate::shared::instance_id(name, &version);
            services.push(crate::shared::synthesize::project_service_entry(name.clone(), &instance, &exe));
            shared_ids.push(name.clone());
        }
    }
    if !errors.is_empty() {
        return Err(errors);
    }

    let mut groups: HashMap<String, Vec<String>> = top
        .get("groups")
        .and_then(Value::as_object)
        .map(|g| g.iter().map(|(k, v)| (k.clone(), string_array(v))).collect())
        .unwrap_or_default();
    // Shared services join the conventional `all` group when the project defines one — so
    // `hearthd start all` brings them up too. No `all` group → no implicit membership.
    if let Some(all) = groups.get_mut("all") {
        all.extend(shared_ids);
    }

    Ok(ServiceCatalog {
        start_failure_policy: StartFailurePolicy::StopOnFirstFailureKeepStarted,
        services,
        groups,
        compose_file: None,
        runtime_directory: top.get("runtimeDirectory").and_then(Value::as_str).map(str::to_string),
        private_file_guard: top.get("privateFileGuard").and_then(Value::as_bool),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write(dir: &tempfile::TempDir, name: &str, content: &str) -> PathBuf {
        let path = dir.path().join(name);
        fs::write(&path, content).unwrap();
        path
    }

    #[test]
    fn loads_a_minimal_yaml_catalog() {
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir,
            "hearth.yaml",
            r#"
version: 1
services:
  api:
    run: { argv: [sleep, 30] }
    readiness: { kind: process }
"#,
        );
        let loaded = load_catalog(dir.path()).expect("should load");
        assert_eq!(loaded.catalog.services.len(), 1);
        let service = &loaded.catalog.services[0];
        assert_eq!(service.id, "api");
        match &service.profiles.run {
            ServiceRunProfile::Verified { command, .. } => match &command.command {
                CommandSpec::Argv { argv } => assert_eq!(argv, &vec!["sleep".to_string(), "30".to_string()]),
                _ => panic!("expected argv"),
            },
            _ => panic!("expected verified"),
        }
    }

    #[test]
    fn coerces_numeric_and_boolean_argv_elements_to_strings() {
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir,
            "hearth.yaml",
            r#"
version: 1
services:
  api:
    run: { argv: [sleep, 30, true] }
    readiness: { kind: process }
"#,
        );
        let loaded = load_catalog(dir.path()).expect("should load");
        match &loaded.catalog.services[0].profiles.run {
            ServiceRunProfile::Verified { command, .. } => match &command.command {
                CommandSpec::Argv { argv } => assert_eq!(argv, &vec!["sleep".to_string(), "30".to_string(), "true".to_string()]),
                _ => panic!("expected argv"),
            },
            _ => panic!("expected verified"),
        }
    }

    #[test]
    fn json_file_is_valid_config() {
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir,
            "hearth.json",
            r#"{"version":1,"services":{"api":{"run":{"argv":["sleep","30"]},"readiness":{"kind":"process"}}}}"#,
        );
        let loaded = load_catalog(dir.path()).expect("should load");
        assert_eq!(loaded.catalog.services.len(), 1);
    }

    #[test]
    fn format_precedence_yaml_over_yml_over_json() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir, "hearth.yaml", "version: 1\nservices:\n  yaml_svc:\n    run: { argv: [x] }\n    readiness: { kind: process }\n");
        write(&dir, "hearth.yml", "version: 1\nservices:\n  yml_svc:\n    run: { argv: [x] }\n    readiness: { kind: process }\n");
        write(&dir, "hearth.json", r#"{"version":1,"services":{"json_svc":{"run":{"argv":["x"]},"readiness":{"kind":"process"}}}}"#);
        let loaded = load_catalog(dir.path()).expect("should load");
        assert_eq!(loaded.catalog.services[0].id, "yaml_svc");
    }

    #[test]
    fn rejects_argv_and_shell_both_set() {
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir,
            "hearth.yaml",
            "version: 1\nservices:\n  api:\n    run: { argv: [x], shell: 'echo hi' }\n    readiness: { kind: process }\n",
        );
        let err = load_catalog(dir.path()).unwrap_err();
        assert!(err.errors.iter().any(|e| e.contains("exactly one of")), "{:?}", err.errors);
    }

    #[test]
    fn rejects_unknown_readiness_kind() {
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir,
            "hearth.yaml",
            "version: 1\nservices:\n  api:\n    run: { argv: [x] }\n    readiness: { kind: bogus }\n",
        );
        let err = load_catalog(dir.path()).unwrap_err();
        assert!(err.errors.iter().any(|e| e.contains("must be one of")), "{:?}", err.errors);
    }

    #[test]
    fn rejects_cwd_escaping_the_project_root() {
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir,
            "hearth.yaml",
            "version: 1\nservices:\n  api:\n    run: { argv: [x] }\n    readiness: { kind: process }\n    cwd: '../escape'\n",
        );
        let err = load_catalog(dir.path()).unwrap_err();
        assert!(err.errors.iter().any(|e| e.contains("escapes the project root")), "{:?}", err.errors);
    }

    #[test]
    fn rejects_absolute_cwd() {
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir,
            "hearth.yaml",
            "version: 1\nservices:\n  api:\n    run: { argv: [x] }\n    readiness: { kind: process }\n    cwd: '/etc'\n",
        );
        let err = load_catalog(dir.path()).unwrap_err();
        assert!(err.errors.iter().any(|e| e.contains("must be a relative path")), "{:?}", err.errors);
    }

    #[test]
    fn missing_config_file_reports_no_config_found() {
        let dir = tempfile::tempdir().unwrap();
        let err = load_catalog(dir.path()).unwrap_err();
        assert!(err.errors.iter().any(|e| e.contains("no config file found")), "{:?}", err.errors);
    }

    #[test]
    fn parse_error_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir, "hearth.yaml", "services: [unterminated flow sequence");
        let err = load_catalog(dir.path()).unwrap_err();
        assert!(err.errors.iter().any(|e| e.contains("failed to parse")), "{:?}", err.errors);
    }

    #[test]
    fn leftover_config_ts_is_rejected_with_a_hint() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir, "hearth.config.ts", "export const catalog = {};\n");
        let err = load_catalog(dir.path()).unwrap_err();
        assert!(err.errors.iter().any(|e| e.contains("no longer accepted")), "{:?}", err.errors);
    }

    #[test]
    fn a_typescript_path_is_rejected_directly() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(&dir, "hearth.config.ts", "export const catalog = {};\n");
        let err = load_catalog_from_file(&path, dir.path()).unwrap_err();
        assert!(err.errors.iter().any(|e| e.contains("TypeScript catalog")), "{:?}", err.errors);
    }

    #[test]
    fn maps_readiness_timeout_ms_onto_the_run_profile() {
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir,
            "hearth.yaml",
            "version: 1\nservices:\n  api:\n    run: { argv: [x] }\n    readiness: { kind: tcp, port: 8080 }\n    readinessTimeoutMs: 30000\n",
        );
        let loaded = load_catalog(dir.path()).expect("should load");
        match &loaded.catalog.services[0].profiles.run {
            ServiceRunProfile::Verified { readiness_timeout_ms, .. } => assert_eq!(*readiness_timeout_ms, Some(30_000)),
            _ => panic!("expected verified"),
        }
    }

    #[test]
    fn command_readiness_with_cwd() {
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir,
            "hearth.yaml",
            "version: 1\nservices:\n  api:\n    run: { argv: [x] }\n    readiness: { kind: command, command: { argv: [check] }, cwd: sub }\n",
        );
        let loaded = load_catalog(dir.path()).expect("should load");
        match loaded.catalog.services[0].profiles.run.readiness() {
            ReadinessSpec::Command { cwd, .. } => assert_eq!(cwd.as_deref(), Some("sub")),
            _ => panic!("expected command readiness"),
        }
    }

    #[test]
    fn maps_a_declarative_preparation_command_alongside_readiness() {
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir,
            "hearth.yaml",
            "version: 1\nservices:\n  sync:\n    run: { argv: [task, sync] }\n    readiness: { kind: process }\n    preparationCommand: { command: { argv: [task, \"sync:prepare\"] }, cwd: infra }\n",
        );
        let loaded = load_catalog(dir.path()).expect("should load");
        match &loaded.catalog.services[0].profiles.run {
            ServiceRunProfile::Verified { preparation_command, .. } => {
                assert_eq!(preparation_command.as_ref().unwrap().command, CommandSpec::Argv { argv: vec!["task".to_string(), "sync:prepare".to_string()] });
                assert_eq!(preparation_command.as_ref().unwrap().cwd.as_deref(), Some("infra"));
            }
            _ => panic!("expected a verified run profile"),
        }
    }

    #[test]
    fn rejects_a_malformed_preparation_command() {
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir,
            "hearth.yaml",
            "version: 1\nservices:\n  sync:\n    run: { argv: [task, sync] }\n    readiness: { kind: process }\n    preparationCommand: \"not-an-object\"\n",
        );
        let err = load_catalog(dir.path()).unwrap_err();
        assert!(err.errors.iter().any(|e| e.contains("preparationCommand")), "{:?}", err.errors);
    }

    #[test]
    fn rejects_a_typod_key_but_allows_x_prefixed_ones() {
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir,
            "hearth.yaml",
            "version: 1\nx-common: &c { kind: process }\nservices:\n  api:\n    command: [x]\n    readiness: *c\n",
        );
        let err = load_catalog(dir.path()).unwrap_err();
        assert_eq!(
            err.errors,
            vec![
                "services.api has unknown key \"command\" (known: label, kind, ownership, env, container, cwd, run, stop, build, readiness, readinessTimeoutMs, preparationCommand, ports, urls)"
                    .to_string()
            ]
        );
    }

    #[test]
    fn cross_service_validation_still_runs_after_mapping() {
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir,
            "hearth.yaml",
            "version: 1\nservices:\n  a:\n    run: { argv: [x] }\n    readiness: { kind: tcp, port: 8080 }\n  b:\n    run: { argv: [y] }\n    readiness: { kind: tcp, port: 8080 }\n",
        );
        let err = load_catalog(dir.path()).unwrap_err();
        assert!(err.errors.iter().any(|e| e.contains("is shared by")), "{:?}", err.errors);
    }

    /// `shared:` expands into generated `ownership: external` services — run is the one-shot
    /// `hearthd shared attach` task, readiness is the `hearthd shared probe` command, and the
    /// service joins `all` so `hearthd start all` covers it.
    #[test]
    fn shared_entries_expand_into_generated_external_services() {
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir,
            "hearth.yaml",
            "version: 1\nshared:\n  postgres: \"16.4\"\n  redis: { version: \"7.2\" }\ngroups: { all: [api] }\nservices:\n  api:\n    run: { argv: [x] }\n    readiness: { kind: process }\n",
        );
        let catalog = load_catalog(dir.path()).unwrap().catalog;
        assert_eq!(catalog.services.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(), vec!["api", "postgres", "redis"]);
        let postgres = catalog.services.iter().find(|s| s.id == "postgres").unwrap();
        assert_eq!(postgres.ownership, Some(crate::catalog::ServiceOwnership::External));
        assert_eq!(postgres.kind, Some(crate::catalog::ServiceKind::Infrastructure));
        assert_eq!(postgres.label.as_deref(), Some("postgres@16.4 (shared)"));
        let crate::catalog::ServiceRunProfile::Verified { command, readiness, .. } = &postgres.profiles.run else { panic!("shared entries must be verified") };
        let crate::catalog::CommandSpec::Argv { argv } = &command.command else { panic!("attach must be argv") };
        assert_eq!(&argv[argv.len() - 3..], ["shared", "attach", "postgres@16.4"]);
        let crate::catalog::ReadinessSpec::Command { command: probe, .. } = readiness else { panic!("probe must be a command readiness") };
        let crate::catalog::CommandSpec::Argv { argv: probe_argv } = probe else { panic!("probe must be argv") };
        assert_eq!(&probe_argv[probe_argv.len() - 3..], ["shared", "probe", "postgres@16.4"]);
        assert!(command.docker_stop_command.is_some(), "stop must detach");
        assert_eq!(catalog.groups.get("all").unwrap(), &vec!["api".to_string(), "postgres".to_string(), "redis".to_string()]);
    }

    #[test]
    fn a_project_may_declare_only_shared_services() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir, "hearth.yaml", "version: 1\nshared:\n  redis: \"7.2\"\n");
        let catalog = load_catalog(dir.path()).unwrap().catalog;
        assert_eq!(catalog.services.len(), 1);
    }

    #[test]
    fn shared_entries_reject_bad_names_and_missing_versions() {
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir,
            "hearth.yaml",
            "version: 1\nshared:\n  \"bad name\": \"1.0\"\n  redis: {}\nservices:\n  api:\n    run: { argv: [x] }\n    readiness: { kind: process }\n",
        );
        let errors = load_catalog(dir.path()).expect_err("must not load").errors;
        assert!(errors.iter().any(|e| e.contains("invalid service name")), "{errors:?}");
        assert!(errors.iter().any(|e| e.contains("version")), "{errors:?}");
    }

    #[test]
    fn a_shared_key_colliding_with_a_service_id_is_a_duplicate() {
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir,
            "hearth.yaml",
            "version: 1\nshared:\n  api: \"1.0\"\nservices:\n  api:\n    run: { argv: [x] }\n    readiness: { kind: process }\n",
        );
        let errors = load_catalog(dir.path()).expect_err("must not load").errors;
        assert!(errors.iter().any(|e| e.contains("duplicate service api")), "{errors:?}");
    }

    /// A missing `readiness` used to be swallowed by an `and_then` on the absent key: no error was
    /// recorded, the service was then skipped by the `let Some(profile_run) … else { continue }`
    /// guard, and the catalog loaded *successfully* without it. A typo'd key silently deleted a
    /// service from the catalog.
    #[test]
    fn a_service_missing_readiness_fails_the_load_instead_of_vanishing() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir, "hearth.yaml", "version: 1\nservices:\n  api:\n    run: { argv: [x] }\n");
        let error = load_catalog(dir.path()).expect_err("must not load");
        let text = format!("{error:?}");
        assert!(text.contains("readiness"), "the error must name the missing field, got: {text}");
    }

    /// Document order, not alphabetical: the TS loader iterates insertion order, and this ordering
    /// is user-visible in `/v1/catalog`, `hearthd status` and both TUIs.
    #[test]
    fn services_keep_their_document_order() {
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir,
            "hearth.yaml",
            "version: 1\nservices:\n  zebra:\n    run: { argv: [x] }\n    readiness: { kind: process }\n  alpha:\n    run: { argv: [x] }\n    readiness: { kind: process }\n  middle:\n    run: { argv: [x] }\n    readiness: { kind: process }\n",
        );
        let loaded = load_catalog(dir.path()).expect("loads");
        let ids: Vec<&str> = loaded.catalog.services.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, vec!["zebra", "alpha", "middle"], "alphabetizing here diverges from the TS loader");
    }

    /// An out-of-range port used to be wrapped into a u16 (`70000` -> `4464`), loading a catalog
    /// that advertised a port nobody authored.
    #[test]
    fn an_out_of_range_port_is_an_error_not_a_silent_truncation() {
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir,
            "hearth.yaml",
            "version: 1\nservices:\n  api:\n    run: { argv: [x] }\n    readiness: { kind: process }\n    ports: [{ port: 70000, label: http }]\n",
        );
        let error = load_catalog(dir.path()).expect_err("must not load");
        assert!(format!("{error:?}").contains("65535"), "{error:?}");
    }

    #[test]
    fn maps_service_urls_in_both_the_string_and_object_forms() {
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir,
            "hearth.yaml",
            "version: 1\nservices:\n  api:\n    run: { argv: [x] }\n    readiness: { kind: process }\n    urls:\n      - http://127.0.0.1:8080\n      - { url: \"https://{tailnetHost}:8443\", label: admin, requiresRunning: false }\n",
        );
        let urls = load_catalog(dir.path()).expect("loads").catalog.services[0].urls.clone().expect("urls");
        assert_eq!(urls[0], ServiceUrl { url: "http://127.0.0.1:8080".into(), label: None, requires_running: None });
        assert_eq!(urls[1], ServiceUrl { url: "https://{tailnetHost}:8443".into(), label: Some("admin".into()), requires_running: Some(false) });
    }

    /// A typo'd placeholder must fail the load rather than render a dead link, with the same
    /// message the TS loader produces for the same input.
    #[test]
    fn rejects_an_unknown_placeholder_and_a_non_http_url() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir, "hearth.yaml", "version: 1\nservices:\n  api:\n    run: { argv: [x] }\n    readiness: { kind: process }\n    urls: [\"https://{tailnethost}:1\", \"ftp://x\"]\n");
        let errors = load_catalog(dir.path()).expect_err("must not load").errors;
        assert_eq!(
            errors,
            vec!["api:urls[0] has unknown placeholder {tailnethost} (known: {tailnetHost})".to_string(), "api:urls[1] must be an http:// or https:// URL".to_string()]
        );
    }
}
