//! Declarative catalog authoring — port of `src/core/config-file.ts`. A `hearth.yaml` (or
//! `.yml`/`.json`) file sitting in a project's root is mapped onto the same `ServiceCatalog` every
//! other entry point takes. TypeScript catalogs (`hearth.config.ts`) are not accepted.
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::catalog::{
    validate_catalog, CommandSpec, ReadinessSpec, ServiceCatalog, ServiceDefinition, ServiceKind,
    ServiceOwnership, ServicePort, ServiceProfiles, ServiceRunProfile, ServiceUrl,
    StartFailurePolicy,
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
    CONFIG_FILE_NAMES
        .iter()
        .map(|name| root.join(name))
        .find(|path| path.exists())
}

pub fn load_catalog(root: &Path) -> Result<LoadedCatalog, ConfigFileLoadError> {
    let path = find_config_file(root).ok_or_else(|| {
        let mut message = format!(
            "no config file found in {} (looked for {})",
            root.display(),
            CONFIG_FILE_NAMES.join(", ")
        );
        if root.join("hearth.config.ts").exists() {
            message.push_str(
                "; hearth.config.ts is no longer accepted — author a hearth.yaml instead",
            );
        }
        ConfigFileLoadError {
            path: None,
            errors: vec![message],
        }
    })?;
    load_catalog_from_file(&path, root)
}

pub fn load_catalog_from_file(
    path: &Path,
    root: &Path,
) -> Result<LoadedCatalog, ConfigFileLoadError> {
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

    let is_yaml = matches!(
        path.extension().and_then(|e| e.to_str()),
        Some("yaml") | Some("yml")
    );
    let raw: Value = if is_yaml {
        // JSON is valid YAML, so this one parser handles both .yaml/.yml and any .json file that
        // happens to reach this branch — mirrors `Bun.YAML.parse` being used for both in the TS
        // source. `.json` files are still routed through `serde_json` below for a stricter parse.
        let yaml_value: serde_yaml::Value =
            serde_yaml::from_str(&text).map_err(|e| ConfigFileLoadError {
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

    let catalog = map_config_file(&raw, root).map_err(|errors| ConfigFileLoadError {
        path: Some(path.to_path_buf()),
        errors,
    })?;
    let validation = validate_catalog(&catalog);
    if !validation.errors.is_empty() {
        return Err(ConfigFileLoadError {
            path: Some(path.to_path_buf()),
            errors: validation.errors,
        });
    }
    Ok(LoadedCatalog {
        catalog,
        path: path.to_path_buf(),
    })
}

// -------------------------------------------------------------------------------------------
// Declarative shape -> ServiceCatalog
// -------------------------------------------------------------------------------------------

const READINESS_KINDS: [&str; 7] = [
    "process",
    "tcp",
    "http",
    "container",
    "tailnet",
    "command",
    "exit",
];
const SERVICE_KINDS: [&str; 2] = ["application", "infrastructure"];
const OWNERSHIPS: [&str; 2] = ["daemon", "external"];

fn is_string_array(value: &Value) -> bool {
    value
        .as_array()
        .map(|a| a.iter().all(Value::is_string))
        .unwrap_or(false)
}

fn is_string_record(value: &Value) -> bool {
    value
        .as_object()
        .map(|o| o.values().all(Value::is_string))
        .unwrap_or(false)
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
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// Depth-first group member expansion. `visiting` is the chain of group names currently being
/// expanded — re-entering one is a cycle and a load error, not silent truncation.
fn expand_group_members<'a>(
    group: &str,
    members: &'a [String],
    declared: &HashMap<&'a str, &'a [String]>,
    service_ids: &std::collections::HashSet<&'a str>,
    visiting: &mut Vec<&'a str>,
    out: &mut Vec<String>,
    errors: &mut Vec<String>,
) {
    for member in members {
        // Service ids win over same-named groups, matching `targets()` resolution precedence.
        if service_ids.contains(member.as_str()) {
            if !out.contains(member) {
                out.push(member.clone());
            }
            continue;
        }
        match declared.get(member.as_str()) {
            Some(sub_members) => {
                if visiting.contains(&member.as_str()) {
                    let mut chain: Vec<&str> = visiting.clone();
                    chain.push(member.as_str());
                    errors.push(format!("group cycle: {}", chain.join(" -> ")));
                    continue;
                }
                visiting.push(member.as_str());
                expand_group_members(
                    member,
                    sub_members,
                    declared,
                    service_ids,
                    visiting,
                    out,
                    errors,
                );
                visiting.pop();
            }
            None => errors.push(format!(
                "group {group} references unknown service or group {member}"
            )),
        }
    }
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
            items
                .iter()
                .map(scalar_to_string)
                .collect::<Option<Vec<String>>>()
        });
        match argv {
            Some(argv) if !argv.is_empty() => Some(ReadCommand {
                spec: CommandSpec::Argv { argv },
            }),
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
        Some(ReadCommand {
            spec: CommandSpec::Shell {
                shell: shell.to_string(),
                exec,
            },
        })
    }
}

fn tcp_port(value: &Value) -> Option<u16> {
    match value.as_i64() {
        Some(port) if (1..=u16::MAX as i64).contains(&port) => Some(port as u16),
        _ => None,
    }
}

/// `path` on `kind: http` is an absolute request path. Braces stay out: this expands to a concrete
/// URL at load, and a `{port}` left in the path would not be rendered again.
fn valid_health_path(path: &str) -> bool {
    path.starts_with('/')
        && !path.contains(char::is_whitespace)
        && !path.contains("://")
        && !path.contains('{')
        && !path.contains('}')
}

fn read_http_readiness(
    obj: &serde_json::Map<String, Value>,
    path: &str,
    primary_port: Option<u16>,
    errors: &mut Vec<String>,
) -> Option<ReadinessSpec> {
    if obj.contains_key("url") {
        if obj.contains_key("path") || obj.contains_key("port") {
            errors.push(format!(
                "{path} must not set url together with path or port"
            ));
            return None;
        }
        let Some(url) = obj
            .get("url")
            .and_then(Value::as_str)
            .filter(|url| !url.is_empty())
        else {
            errors.push(format!("{path}.url must be a non-empty string"));
            return None;
        };
        return Some(ReadinessSpec::Http {
            url: url.to_string(),
        });
    }
    let request_path = match obj.get("path") {
        None => "/health".to_string(),
        Some(Value::String(request_path)) if valid_health_path(request_path) => {
            request_path.clone()
        }
        Some(Value::String(_)) => {
            errors.push(format!("{path}.path must be an absolute path"));
            return None;
        }
        Some(_) => {
            errors.push(format!("{path}.path must be a string"));
            return None;
        }
    };
    let port = match obj.get("port") {
        None => primary_port,
        Some(value) => match tcp_port(value) {
            Some(port) => Some(port),
            None => {
                errors.push(format!("{path}.port must be a positive integer"));
                return None;
            }
        },
    };
    match port {
        Some(port) => Some(ReadinessSpec::Http {
            url: format!("http://127.0.0.1:{port}{request_path}"),
        }),
        None => {
            errors.push(format!("{path} needs a port"));
            None
        }
    }
}

fn read_readiness(
    value: &Value,
    path: &str,
    primary_port: Option<u16>,
    errors: &mut Vec<String>,
) -> Option<ReadinessSpec> {
    let obj = value.as_object();
    let kind = obj.and_then(|o| o.get("kind")).and_then(Value::as_str);
    let Some(kind) = kind else {
        errors.push(format!(
            "{path}.kind is required (one of {})",
            READINESS_KINDS.join(", ")
        ));
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
        "tcp" => match obj.get("port") {
            None => match primary_port {
                Some(port) => Some(ReadinessSpec::Tcp { port }),
                None => {
                    errors.push(format!("{path} needs a port"));
                    None
                }
            },
            Some(value) => match tcp_port(value) {
                Some(port) => Some(ReadinessSpec::Tcp { port }),
                None => {
                    errors.push(format!("{path}.port must be a positive integer"));
                    None
                }
            },
        },
        "http" => read_http_readiness(obj, path, primary_port, errors),
        "exit" => Some(ReadinessSpec::Exit),
        _ => {
            // kind === "command"
            let default_command = Value::Null;
            let command = read_command_spec(
                obj.get("command").unwrap_or(&default_command),
                &format!("{path}.command"),
                errors,
            )?;
            let cwd = match obj.get("cwd") {
                None => None,
                Some(Value::String(s)) => Some(s.clone()),
                Some(_) => {
                    errors.push(format!("{path}.cwd must be a string"));
                    return None;
                }
            };
            Some(ReadinessSpec::Command {
                command: command.spec,
                cwd,
            })
        }
    }
}

fn read_preparation_command(
    value: &Value,
    path: &str,
    errors: &mut Vec<String>,
) -> Option<crate::catalog::PreparationCommand> {
    let Some(obj) = value.as_object() else {
        errors.push(format!(
            "{path} must be an object with `command` (and optional `cwd`)"
        ));
        return None;
    };
    let command = read_command_spec(
        obj.get("command").unwrap_or(&Value::Null),
        &format!("{path}.command"),
        errors,
    );
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
    Some(crate::catalog::PreparationCommand {
        command: command?.spec,
        cwd,
        serialization_key,
    })
}

/// `urls` accepts a bare string or `{ url, label?, requiresRunning? }` per entry. Only the shape is
/// checked here; the URL format and its placeholders are checked by `validate_catalog`, which runs
/// for every catalog source, not just config files. Mirrors `readUrls` in the TS source.
fn read_urls(
    value: Option<&Value>,
    path: &str,
    errors: &mut Vec<String>,
) -> Option<Vec<ServiceUrl>> {
    let Some(value) = value else {
        return Some(Vec::new());
    };
    let Some(array) = value.as_array() else {
        errors.push(format!("{path} must be an array"));
        return None;
    };
    let mut urls = Vec::new();
    for (index, entry) in array.iter().enumerate() {
        let entry_path = format!("{path}[{index}]");
        if let Some(url) = entry.as_str() {
            urls.push(ServiceUrl {
                url: url.to_string(),
                label: None,
                requires_running: None,
            });
            continue;
        }
        let Some(url) = entry
            .as_object()
            .and_then(|o| o.get("url"))
            .and_then(Value::as_str)
        else {
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
        urls.push(ServiceUrl {
            url: url.to_string(),
            label,
            requires_running,
        });
    }
    Some(urls)
}

fn read_ports(
    value: Option<&Value>,
    path: &str,
    errors: &mut Vec<String>,
) -> Option<Vec<ServicePort>> {
    let Some(value) = value else {
        return Some(Vec::new());
    };
    let Some(array) = value.as_array() else {
        errors.push(format!("{path} must be an array"));
        return None;
    };
    let mut ports = Vec::new();
    for (index, entry) in array.iter().enumerate() {
        let entry_path = format!("{path}[{index}]");
        let obj = entry.as_object();
        let port = obj.and_then(|o| o.get("port")).and_then(Value::as_i64);
        let label = obj
            .and_then(|o| o.get("label"))
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty());
        let (Some(port), Some(label)) = (port, label) else {
            errors.push(format!(
                "{entry_path} must be {{ port: number, label: string }}"
            ));
            continue;
        };
        // An out-of-range port is an ERROR, not something to silently wrap into a u16. `70000 as
        // u16` is 4464 — the catalog would have loaded, advertised a port nobody asked for, and
        // the mistake would only surface as a confusing conflict much later.
        if !(1..=u16::MAX as i64).contains(&port) {
            errors.push(format!(
                "{entry_path}.port must be between 1 and 65535, got {port}"
            ));
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
        ports.push(ServicePort {
            port: port as u16,
            label: label.to_string(),
            requires_running,
        });
    }
    Some(ports)
}

/// `cwd` is authored relative to the project root and must stay inside it. This is a lexical check
/// on the resolved path, not a symlink-escape defense.
fn resolve_service_cwd(cwd: Option<&str>, path: &str, errors: &mut Vec<String>) -> Option<String> {
    let relative_cwd = cwd.unwrap_or(".");
    if Path::new(relative_cwd).is_absolute() {
        errors.push(format!(
            "{path}.cwd must be a relative path, got {relative_cwd}"
        ));
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
            errors.push(format!(
                "{path}.cwd escapes the project root: {relative_cwd}"
            ));
            return None;
        }
    }
    Some(relative_cwd.to_string())
}

/// A key outside these sets is a typo (e.g. `command:` for `run:`), which would otherwise be silently
/// dropped and surface much later as an unrelated error. `x-`-prefixed keys stay free for YAML anchors.
const TOP_LEVEL_KEYS: &[&str] = &[
    "version",
    "env",
    "envFile",
    "runtimeDirectory",
    "privateFileGuard",
    "groups",
    "services",
    "shared",
];
const SERVICE_KEYS: &[&str] = &[
    "label",
    "kind",
    "ownership",
    "disabled",
    "env",
    "container",
    "cwd",
    "run",
    "stop",
    "build",
    "readiness",
    "readinessTimeoutMs",
    "preparationCommand",
    "ports",
    "urls",
    "artifact",
];

const ARTIFACT_KEYS: &[&str] = &["version", "url", "script", "scriptArgs", "sha256"];

fn read_artifact(
    value: &Value,
    path: &str,
    errors: &mut Vec<String>,
) -> Option<crate::catalog::ServiceArtifact> {
    let Some(obj) = value.as_object() else {
        errors.push(format!("{path} must be an object"));
        return None;
    };
    check_known_keys(obj, ARTIFACT_KEYS, path, errors);
    let version = obj
        .get("version")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    if version.is_empty() {
        errors.push(format!("{path}.version must be a non-empty string"));
    }
    let url = obj
        .get("url")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let script = obj
        .get("script")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    if url.is_some() == script.is_some() {
        errors.push(format!("{path} needs exactly one of url or script"));
    }
    let sha256 = obj
        .get("sha256")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    if url.is_some() && sha256.is_none() {
        errors.push(format!("{path}.sha256 is required for a url artifact"));
    }
    let script_args = match obj.get("scriptArgs") {
        None => Vec::new(),
        Some(v) if is_string_array(v) => string_array(v),
        Some(_) => {
            errors.push(format!("{path}.scriptArgs must be an array of strings"));
            Vec::new()
        }
    };
    Some(crate::catalog::ServiceArtifact {
        version,
        url,
        script,
        script_args,
        sha256,
        install_dir: None,
        data_dir: None,
    })
}

/// `{var}` values available to a service declaring `artifact:` — `{port}` is the first declared
/// port (or the tcp readiness port when no `ports:` entries exist); `{port2}…` walk the rest.
fn artifact_vars(
    root: &Path,
    runtime_directory: &Path,
    id: &str,
    version: &str,
    ports: &[ServicePort],
    readiness: Option<&ReadinessSpec>,
) -> HashMap<String, String> {
    let mut vars = HashMap::from([
        (
            "installDir".to_string(),
            crate::paths::install_dir(runtime_directory, id, version)
                .display()
                .to_string(),
        ),
        (
            "dataDir".to_string(),
            crate::paths::service_data_dir(runtime_directory, id)
                .display()
                .to_string(),
        ),
        ("serviceId".to_string(), id.to_string()),
        ("projectRoot".to_string(), root.display().to_string()),
    ]);
    for (index, entry) in ports.iter().enumerate() {
        let name = if index == 0 {
            "port".to_string()
        } else {
            format!("port{}", index + 1)
        };
        vars.insert(name, entry.port.to_string());
    }
    if !vars.contains_key("port") {
        if let Some(ReadinessSpec::Tcp { port }) = readiness {
            vars.insert("port".to_string(), port.to_string());
        }
    }
    vars
}

/// Is `name` one of the vars an `artifact:` service can reference — `{installDir}`, `{dataDir}`,
/// `{serviceId}`, `{projectRoot}`, `{port}`, `{portN}`? Any other `{…}` is left alone (`awk '{print}'`).
fn is_artifact_var(name: &str) -> bool {
    matches!(name, "installDir" | "dataDir" | "serviceId" | "projectRoot")
        || name
            .strip_prefix("port")
            .is_some_and(|digits| digits.chars().all(|c| c.is_ascii_digit()))
}

/// Flags an artifact var the template references but `vars` does not define — always a bug (every
/// non-port var is defined once an `artifact:` exists; `{port}`/`{portN}` only when enough ports
/// are declared). Checked on the template, before rendering.
fn check_template(
    text: &str,
    vars: &HashMap<String, String>,
    path: &str,
    errors: &mut Vec<String>,
) {
    if let Some(var) = crate::catalog::unknown_template_var(text, |name| {
        !is_artifact_var(name) || vars.contains_key(name)
    }) {
        errors.push(format!("{path}: template var {{{var}}} has no value — declare it in ports (or a tcp readiness port for {{port}})"));
    }
}

fn render_artifact_command(
    spec: &CommandSpec,
    vars: &HashMap<String, String>,
    path: &str,
    errors: &mut Vec<String>,
) -> CommandSpec {
    for text in crate::shared::render::command_strings(spec) {
        check_template(text, vars, path, errors);
    }
    crate::shared::render::render_command(spec, vars)
}

/// Substitutes an artifact service's `{installDir}`/`{dataDir}`/`{port}`/`{portN}`/`{serviceId}`/
/// `{projectRoot}` vars everywhere they can appear, in place on the finished definition. `urls`
/// render but are not strict-checked — `{tailnetHost}` is a legal placeholder there.
fn render_service_artifact(
    root: &Path,
    runtime_directory: &Path,
    def: &mut crate::catalog::ServiceDefinition,
    artifact: &mut crate::catalog::ServiceArtifact,
    path: &str,
    errors: &mut Vec<String>,
) {
    use crate::shared::render::{render_env, render_str};
    let ports: &[ServicePort] = def.ports.as_deref().unwrap_or(&[]);
    let readiness_for_port = def.profiles.run.readiness().clone();
    let vars = artifact_vars(
        root,
        runtime_directory,
        &def.id,
        &artifact.version,
        ports,
        Some(&readiness_for_port),
    );
    artifact.install_dir = Some(vars["installDir"].clone());
    artifact.data_dir = Some(vars["dataDir"].clone());
    match &mut def.profiles.run {
        ServiceRunProfile::Verified {
            command,
            readiness,
            preparation_command,
            ..
        } => {
            command.command =
                render_artifact_command(&command.command, &vars, &format!("{path}.run"), errors);
            check_template(&command.cwd, &vars, &format!("{path}.cwd"), errors);
            command.cwd = render_str(&command.cwd, &vars);
            if let Some(env) = &mut command.environment {
                for value in env.values() {
                    check_template(value, &vars, &format!("{path}.env"), errors);
                }
                *env = render_env(env, &vars);
            }
            if let Some(stop) = &mut command.docker_stop_command {
                *stop = render_artifact_command(stop, &vars, &format!("{path}.stop"), errors);
            }
            // Artifact-var rules, not the recipe renderer's strict check: a readiness command may
            // legitimately carry `${HOME}` or `awk '{print}'`.
            match readiness {
                ReadinessSpec::Http { url } => {
                    check_template(url, &vars, &format!("{path}.readiness.url"), errors);
                    *url = render_str(url, &vars);
                }
                ReadinessSpec::Command { command, cwd } => {
                    *command = render_artifact_command(
                        command,
                        &vars,
                        &format!("{path}.readiness.command"),
                        errors,
                    );
                    if let Some(cwd) = cwd {
                        check_template(cwd, &vars, &format!("{path}.readiness.cwd"), errors);
                        *cwd = render_str(cwd, &vars);
                    }
                }
                ReadinessSpec::Process
                | ReadinessSpec::Tcp { .. }
                | ReadinessSpec::Container
                | ReadinessSpec::Tailnet
                | ReadinessSpec::Exit => {}
            }
            if let Some(prep) = preparation_command {
                prep.command = render_artifact_command(
                    &prep.command,
                    &vars,
                    &format!("{path}.preparationCommand"),
                    errors,
                );
                if let Some(cwd) = &mut prep.cwd {
                    check_template(
                        cwd,
                        &vars,
                        &format!("{path}.preparationCommand.cwd"),
                        errors,
                    );
                    *cwd = render_str(cwd, &vars);
                }
            }
        }
        ServiceRunProfile::Unresolved { .. } => {}
    }
    if let Some(build) = &mut def.profiles.build {
        build.command.command = render_artifact_command(
            &build.command.command,
            &vars,
            &format!("{path}.build"),
            errors,
        );
        build.command.cwd = render_str(&build.command.cwd, &vars);
        if let Some(env) = &mut build.command.environment {
            *env = render_env(env, &vars);
        }
    }
    if let Some(urls) = &mut def.urls {
        for entry in urls {
            entry.url = render_str(&entry.url, &vars);
        }
    }
}

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

fn check_known_keys(
    value: &serde_json::Map<String, Value>,
    known: &[&str],
    path: &str,
    errors: &mut Vec<String>,
) {
    for key in value.keys() {
        if !known.contains(&key.as_str()) && !key.starts_with("x-") {
            errors.push(format!(
                "{path} has unknown key \"{key}\" (known: {})",
                known.join(", ")
            ));
        }
    }
}

/// `shared:` keys become both service ids and smp instance names — restrict to the charset both
/// sides can carry safely (also a valid filename under `installs/`).
fn is_valid_shared_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphanumeric())
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

fn map_config_file(raw: &Value, root: &Path) -> Result<ServiceCatalog, Vec<String>> {
    let mut errors: Vec<String> = Vec::new();
    let Some(top) = raw.as_object() else {
        return Err(vec![
            "config file must contain a YAML/JSON object".to_string()
        ]);
    };

    check_known_keys(top, TOP_LEVEL_KEYS, "config file", &mut errors);
    if top.get("version").and_then(Value::as_i64) != Some(1) {
        errors.push(format!(
            "version must be 1, got {}",
            top.get("version").cloned().unwrap_or(Value::Null)
        ));
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
        let ok = groups
            .as_object()
            .map(|g| g.values().all(is_string_array))
            .unwrap_or(false);
        if !ok {
            errors.push("groups must be a map of string to string[]".to_string());
        }
    }
    // Present non-objects are type errors. Emptiness applies only to absent keys and empty
    // objects, so a non-empty sibling map cannot drop `services: []` or `shared: []`.
    let mut shared_not_object = false;
    if let Some(shared) = top.get("shared") {
        match shared.as_object() {
            None => {
                shared_not_object = true;
                errors.push(
                    "shared must be a map of service name to version (or { version })".to_string(),
                );
            }
            Some(entries) => {
                for (name, value) in entries {
                    let path = format!("shared.{name}");
                    if !is_valid_shared_name(name) {
                        errors.push(format!("{path} has an invalid service name (expected [a-zA-Z0-9][a-zA-Z0-9._-]*)"));
                    }
                    match value {
                        Value::String(_) => {}
                        Value::Object(o) => {
                            for key in o.keys() {
                                if !["version", "preparationCommand", "attachArgs", "urls"]
                                    .contains(&key.as_str())
                                {
                                    errors.push(format!("{path} has unknown key \"{key}\" (known: version, preparationCommand, attachArgs, urls)"));
                                }
                            }
                            match o.get("version") {
                                Some(Value::String(s)) if !s.is_empty() => {}
                                _ => errors
                                    .push(format!("{path}.version must be a non-empty string")),
                            }
                            if let Some(v) = o.get("attachArgs") {
                                if !is_string_array(v) {
                                    errors.push(format!(
                                        "{path}.attachArgs must be an array of strings"
                                    ));
                                }
                            }
                        }
                        _ => errors.push(format!(
                            "{path} must be a version string or an object with version"
                        )),
                    }
                }
            }
        }
    }
    let services_not_object = match top.get("services") {
        Some(value) if value.as_object().is_none() => {
            errors.push("services must be a map of service id to service definition".to_string());
            true
        }
        _ => false,
    };
    let services_raw = top.get("services").and_then(Value::as_object);
    let shared_raw = top.get("shared").and_then(Value::as_object);
    if !services_not_object
        && !shared_not_object
        && services_raw.map(|s| s.is_empty()).unwrap_or(true)
        && shared_raw.map(|s| s.is_empty()).unwrap_or(true)
    {
        errors.push(
            "services must be a non-empty map of service id to service definition".to_string(),
        );
    }
    if !errors.is_empty() {
        return Err(errors);
    }

    // Resolved before the service loop: `artifact:` services render `{installDir}`/`{dataDir}`
    // from it at parse time.
    let runtime_directory_path = crate::paths::resolve_runtime_directory(
        root,
        top.get("runtimeDirectory").and_then(Value::as_str),
    );

    let global_env = top.get("env").map(string_record).unwrap_or_default();
    let file_env = match top.get("envFile").and_then(Value::as_str) {
        Some(env_file) => {
            let path = if Path::new(env_file).is_absolute() {
                PathBuf::from(env_file)
            } else {
                root.join(env_file)
            };
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
                errors.push(format!(
                    "{svc_path}.kind must be one of {}",
                    SERVICE_KINDS.join(", ")
                ));
                None
            }
        };
        let ownership: Option<ServiceOwnership> = match obj.get("ownership") {
            None => None,
            Some(Value::String(s)) if s == "daemon" => Some(ServiceOwnership::Daemon),
            Some(Value::String(s)) if s == "external" => Some(ServiceOwnership::External),
            Some(_) => {
                errors.push(format!(
                    "{svc_path}.ownership must be one of {}",
                    OWNERSHIPS.join(", ")
                ));
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
        let artifact = obj
            .get("artifact")
            .and_then(|v| read_artifact(v, &format!("{svc_path}.artifact"), &mut errors));
        if artifact.is_some() {
            if ownership == Some(ServiceOwnership::External) {
                errors.push(format!("{svc_path}.artifact needs a daemon-owned service — external units never spawn, so never install"));
            }
            if obj.get("run").is_none() {
                errors.push(format!(
                    "{svc_path}.artifact needs a run command to install for"
                ));
            }
        }

        let cwd = resolve_service_cwd(
            obj.get("cwd").and_then(Value::as_str),
            &svc_path,
            &mut errors,
        );
        let ports = read_ports(obj.get("ports"), &format!("{svc_path}.ports"), &mut errors);
        // `kind: http` / `kind: tcp` with no port of their own use the first declared port.
        let primary_port = ports
            .as_ref()
            .and_then(|ports| ports.first().map(|entry| entry.port));
        // Unconditional, even when the key is absent: `read_readiness` is what reports
        // "readiness.kind is required", and an `and_then` on `get("readiness")` would swallow that
        // — the service would then be skipped below with no error at all, silently vanishing from
        // a catalog that otherwise loaded fine.
        let readiness = read_readiness(
            obj.get("readiness").unwrap_or(&Value::Null),
            &format!("{svc_path}.readiness"),
            primary_port,
            &mut errors,
        );
        let urls = read_urls(obj.get("urls"), &format!("{svc_path}.urls"), &mut errors);
        let readiness_timeout_ms = read_positive_u64(
            obj.get("readinessTimeoutMs"),
            &format!("{svc_path}.readinessTimeoutMs"),
            &mut errors,
        );
        let preparation_command = obj.get("preparationCommand").and_then(|v| {
            read_preparation_command(v, &format!("{svc_path}.preparationCommand"), &mut errors)
        });

        let mut profile_run: Option<ServiceRunProfile> = None;
        match obj.get("run") {
            None => {
                if let Some(readiness) = readiness.clone() {
                    profile_run = Some(ServiceRunProfile::Unresolved {
                        readiness,
                        readiness_timeout_ms,
                        preparation: None,
                        preparation_command: preparation_command.clone(),
                    });
                }
            }
            Some(run_value) => {
                let run = read_command_spec(run_value, &format!("{svc_path}.run"), &mut errors);
                let stop = obj
                    .get("stop")
                    .and_then(|s| read_command_spec(s, &format!("{svc_path}.stop"), &mut errors));
                if let (Some(run), Some(readiness), Some(cwd)) =
                    (run, readiness.clone(), cwd.clone())
                {
                    let service_env = obj.get("env").map(string_record).unwrap_or_default();
                    let mut environment = base_env.clone();
                    environment.extend(service_env);
                    profile_run = Some(ServiceRunProfile::Verified {
                        command: crate::catalog::ServiceCommand {
                            command: run.spec,
                            cwd,
                            environment: if environment.is_empty() {
                                None
                            } else {
                                Some(environment)
                            },
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
                let build =
                    read_command_spec(build_value, &format!("{svc_path}.build"), &mut errors);
                let timeout_ms = match build_obj.get("timeoutMs") {
                    None => None,
                    Some(v) => match v.as_i64() {
                        Some(n) if n > 0 => Some(n as u64),
                        _ => {
                            errors.push(format!(
                                "{svc_path}.build.timeoutMs must be a positive integer"
                            ));
                            None
                        }
                    },
                };
                let serialization_key = match build_obj.get("serializationKey") {
                    None => None,
                    Some(Value::String(s)) => Some(s.clone()),
                    Some(_) => {
                        errors.push(format!(
                            "{svc_path}.build.serializationKey must be a string"
                        ));
                        None
                    }
                };
                if let (Some(build), Some(cwd)) = (build, cwd.clone()) {
                    profile_build = Some(crate::catalog::ServiceBuildProfile {
                        command: crate::catalog::ServiceCommand {
                            command: build.spec,
                            cwd,
                            environment: None,
                            container_name: None,
                            docker_stop_command: None,
                        },
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
        let disabled = match obj.get("disabled") {
            None | Some(Value::Bool(false)) => false,
            Some(Value::Bool(true)) => true,
            Some(_) => {
                errors.push(format!("{svc_path}.disabled must be a boolean"));
                false
            }
        };
        let mut definition = ServiceDefinition {
            id: id.clone(),
            label: obj.get("label").and_then(Value::as_str).map(str::to_string),
            kind,
            ownership,
            disabled,
            profiles: ServiceProfiles {
                run: profile_run,
                build: profile_build,
            },
            ports: ports.filter(|p| !p.is_empty()),
            urls: urls.filter(|u| !u.is_empty()),
            artifact: None,
        };
        if let Some(mut artifact) = artifact {
            render_service_artifact(
                root,
                &runtime_directory_path,
                &mut definition,
                &mut artifact,
                &svc_path,
                &mut errors,
            );
            definition.artifact = Some(artifact);
        }
        services.push(definition);
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
                Value::Object(o) => o
                    .get("version")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                _ => continue, // shape errors already recorded above
            };
            if !is_valid_shared_name(name) || version.is_empty() {
                continue;
            }
            // Optional per-entry hooks: `preparationCommand` runs project-side before each attach
            // (e.g. rendering the conf a recipe provision publishes); `attachArgs` append to the
            // attach argv and land on the recipe's provision commands. `{dataDir}`/`{serviceId}`/
            // `{projectRoot}` render like an artifact service — ports live in the smp registry,
            // not this file, so they are not substituted here.
            let shared_vars: HashMap<String, String> = HashMap::from([
                (
                    "dataDir".to_string(),
                    crate::paths::service_data_dir(&runtime_directory_path, name)
                        .display()
                        .to_string(),
                ),
                ("serviceId".to_string(), name.clone()),
                ("projectRoot".to_string(), root.display().to_string()),
            ]);
            let mut preparation_command = None;
            let mut attach_args: Vec<String> = Vec::new();
            if let Value::Object(o) = value {
                let svc_path = format!("shared.{name}");
                preparation_command = o
                    .get("preparationCommand")
                    .and_then(|v| {
                        read_preparation_command(
                            v,
                            &format!("{svc_path}.preparationCommand"),
                            &mut errors,
                        )
                    })
                    .map(|mut prep| {
                        prep.command =
                            crate::shared::render::render_command(&prep.command, &shared_vars);
                        if let Some(cwd) = &mut prep.cwd {
                            *cwd = crate::shared::render::render_str(cwd, &shared_vars);
                        }
                        prep
                    });
                attach_args = string_array(o.get("attachArgs").unwrap_or(&Value::Null))
                    .iter()
                    .map(|a| crate::shared::render::render_str(a, &shared_vars))
                    .collect();
            }
            let urls = value
                .as_object()
                .and_then(|o| read_urls(o.get("urls"), &format!("shared.{name}.urls"), &mut errors))
                .map(|entries| {
                    entries
                        .into_iter()
                        .map(|mut entry| {
                            entry.url = crate::shared::render::render_str(&entry.url, &shared_vars);
                            entry
                        })
                        .collect()
                });
            let instance = crate::shared::instance_id(name, &version);
            services.push(crate::shared::synthesize::project_service_entry(
                name.clone(),
                &instance,
                &exe,
                preparation_command,
                attach_args,
                urls,
            ));
            shared_ids.push(name.clone());
        }
    }
    // `groups:` values may name other groups — members expand depth-first in declaration order,
    // deduplicated on first occurrence. A name shared by a service and a group resolves as the
    // service, matching `hearthd <target>` precedence.
    let declared_groups: Vec<(String, Vec<String>)> = top
        .get("groups")
        .and_then(Value::as_object)
        .map(|g| {
            g.iter()
                .map(|(k, v)| (k.clone(), string_array(v)))
                .collect()
        })
        .unwrap_or_default();
    let declared_map: HashMap<&str, &[String]> = declared_groups
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_slice()))
        .collect();
    let service_ids: std::collections::HashSet<&str> =
        services.iter().map(|s| s.id.as_str()).collect();
    let mut groups: HashMap<String, Vec<String>> = HashMap::new();
    for (name, members) in &declared_groups {
        let mut resolved: Vec<String> = Vec::new();
        let mut visiting: Vec<&str> = vec![name.as_str()];
        expand_group_members(
            name,
            members,
            &declared_map,
            &service_ids,
            &mut visiting,
            &mut resolved,
            &mut errors,
        );
        groups.insert(name.clone(), resolved);
    }
    // Shared services join the conventional `all` group when the project defines one — so
    // `hearthd start all` brings them up too. No `all` group → no implicit membership.
    if let Some(all) = groups.get_mut("all") {
        for id in &shared_ids {
            if !all.contains(id) {
                all.push(id.clone());
            }
        }
    }
    // Disabled services never join a group target — `hearthd start <group>` skips them. They stay
    // in `group_tree` (display membership is still meaningful) and as direct `start <id>` targets,
    // where the operation itself is what gets rejected.
    let disabled_ids: std::collections::HashSet<&str> = services
        .iter()
        .filter(|s| s.disabled)
        .map(|s| s.id.as_str())
        .collect();
    for members in groups.values_mut() {
        members.retain(|id| !disabled_ids.contains(id.as_str()));
    }
    if !errors.is_empty() {
        return Err(errors);
    }

    Ok(ServiceCatalog {
        start_failure_policy: StartFailurePolicy::StopOnFirstFailureKeepStarted,
        services,
        groups,
        group_tree: declared_groups
            .into_iter()
            .map(|(name, members)| crate::catalog::CatalogGroup { name, members })
            .collect(),
        compose_file: None,
        runtime_directory: top
            .get("runtimeDirectory")
            .and_then(Value::as_str)
            .map(str::to_string),
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
                CommandSpec::Argv { argv } => {
                    assert_eq!(argv, &vec!["sleep".to_string(), "30".to_string()])
                }
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
                CommandSpec::Argv { argv } => assert_eq!(
                    argv,
                    &vec!["sleep".to_string(), "30".to_string(), "true".to_string()]
                ),
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
        write(
            &dir,
            "hearth.json",
            r#"{"version":1,"services":{"json_svc":{"run":{"argv":["x"]},"readiness":{"kind":"process"}}}}"#,
        );
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
        assert!(
            err.errors.iter().any(|e| e.contains("exactly one of")),
            "{:?}",
            err.errors
        );
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
        assert!(
            err.errors.iter().any(|e| e.contains("must be one of")),
            "{:?}",
            err.errors
        );
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
        assert!(
            err.errors
                .iter()
                .any(|e| e.contains("escapes the project root")),
            "{:?}",
            err.errors
        );
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
        assert!(
            err.errors
                .iter()
                .any(|e| e.contains("must be a relative path")),
            "{:?}",
            err.errors
        );
    }

    #[test]
    fn missing_config_file_reports_no_config_found() {
        let dir = tempfile::tempdir().unwrap();
        let err = load_catalog(dir.path()).unwrap_err();
        assert!(
            err.errors
                .iter()
                .any(|e| e.contains("no config file found")),
            "{:?}",
            err.errors
        );
    }

    #[test]
    fn parse_error_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir, "hearth.yaml", "services: [unterminated flow sequence");
        let err = load_catalog(dir.path()).unwrap_err();
        assert!(
            err.errors.iter().any(|e| e.contains("failed to parse")),
            "{:?}",
            err.errors
        );
    }

    #[test]
    fn leftover_config_ts_is_rejected_with_a_hint() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir, "hearth.config.ts", "export const catalog = {};\n");
        let err = load_catalog(dir.path()).unwrap_err();
        assert!(
            err.errors.iter().any(|e| e.contains("no longer accepted")),
            "{:?}",
            err.errors
        );
    }

    #[test]
    fn a_typescript_path_is_rejected_directly() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(&dir, "hearth.config.ts", "export const catalog = {};\n");
        let err = load_catalog_from_file(&path, dir.path()).unwrap_err();
        assert!(
            err.errors.iter().any(|e| e.contains("TypeScript catalog")),
            "{:?}",
            err.errors
        );
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
            ServiceRunProfile::Verified {
                readiness_timeout_ms,
                ..
            } => assert_eq!(*readiness_timeout_ms, Some(30_000)),
            _ => panic!("expected verified"),
        }
    }

    #[test]
    fn http_and_tcp_shorthand_expand_to_a_concrete_probe() {
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir,
            "hearth.yaml",
            r#"
version: 1
services:
  api:
    run: { argv: [x] }
    ports: [{ port: 8080, label: http }]
    readiness: { kind: http }
  metrics:
    run: { argv: [y] }
    readiness: { kind: http, port: 6060, path: /metrics }
  nats:
    run: { argv: [z] }
    ports: [{ port: 4222, label: nats }]
    readiness: { kind: tcp }
  legacy:
    run: { argv: [w] }
    readiness: { kind: http, url: "http://127.0.0.1:9090/health" }
"#,
        );
        let loaded = load_catalog(dir.path()).expect("shorthand should load");
        let readiness = |id: &str| {
            loaded
                .catalog
                .services
                .iter()
                .find(|service| service.id == id)
                .unwrap()
                .profiles
                .run
                .readiness()
                .clone()
        };
        assert!(
            matches!(readiness("api"), ReadinessSpec::Http { url } if url == "http://127.0.0.1:8080/health")
        );
        assert!(
            matches!(readiness("metrics"), ReadinessSpec::Http { url } if url == "http://127.0.0.1:6060/metrics")
        );
        assert!(matches!(
            readiness("nats"),
            ReadinessSpec::Tcp { port: 4222 }
        ));
        assert!(
            matches!(readiness("legacy"), ReadinessSpec::Http { url } if url == "http://127.0.0.1:9090/health")
        );
    }

    #[test]
    fn shorthand_without_a_port_or_with_url_and_path_fails_the_load() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir, "hearth.yaml", "version: 1\nservices:\n  api:\n    run: { argv: [x] }\n    readiness: { kind: http }\n");
        let err = load_catalog(dir.path()).unwrap_err();
        assert!(
            err.errors
                .iter()
                .any(|error| error.contains("needs a port")),
            "{:?}",
            err.errors
        );

        write(
            &dir,
            "hearth.yaml",
            "version: 1\nservices:\n  api:\n    run: { argv: [x] }\n    readiness: { kind: tcp }\n",
        );
        let err = load_catalog(dir.path()).unwrap_err();
        assert!(
            err.errors
                .iter()
                .any(|error| error.contains("needs a port")),
            "{:?}",
            err.errors
        );

        write(
            &dir,
            "hearth.yaml",
            "version: 1\nservices:\n  api:\n    run: { argv: [x] }\n    readiness: { kind: http, url: \"http://127.0.0.1:1/health\", path: /health }\n",
        );
        let err = load_catalog(dir.path()).unwrap_err();
        assert!(
            err.errors
                .iter()
                .any(|error| error.contains("must not set url together with path")),
            "{:?}",
            err.errors
        );

        write(&dir, "hearth.yaml", "version: 1\nservices:\n  api:\n    run: { argv: [x] }\n    readiness: { kind: http, port: 8080, path: health }\n");
        let err = load_catalog(dir.path()).unwrap_err();
        assert!(
            err.errors
                .iter()
                .any(|error| error.contains("absolute path")),
            "{:?}",
            err.errors
        );
    }

    #[test]
    fn exit_readiness_loads_and_external_ownership_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir, "hearth.yaml", "version: 1\nservices:\n  fe:\n    cwd: viclass\n    run: { argv: [task, fe] }\n    readiness: { kind: exit }\n");
        let loaded = load_catalog(dir.path()).expect("exit readiness should load");
        assert!(matches!(
            loaded.catalog.services[0].profiles.run.readiness(),
            ReadinessSpec::Exit
        ));

        write(
            &dir,
            "hearth.yaml",
            "version: 1\nservices:\n  fe:\n    ownership: external\n    run: { argv: [task, fe] }\n    readiness: { kind: exit }\n",
        );
        let err = load_catalog(dir.path()).unwrap_err();
        assert!(
            err.errors
                .iter()
                .any(|error| error.contains("daemon-owned")),
            "{:?}",
            err.errors
        );
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
            ServiceRunProfile::Verified {
                preparation_command,
                ..
            } => {
                assert_eq!(
                    preparation_command.as_ref().unwrap().command,
                    CommandSpec::Argv {
                        argv: vec!["task".to_string(), "sync:prepare".to_string()]
                    }
                );
                assert_eq!(
                    preparation_command.as_ref().unwrap().cwd.as_deref(),
                    Some("infra")
                );
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
        assert!(
            err.errors.iter().any(|e| e.contains("preparationCommand")),
            "{:?}",
            err.errors
        );
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
                "services.api has unknown key \"command\" (known: label, kind, ownership, disabled, env, container, cwd, run, stop, build, readiness, readinessTimeoutMs, preparationCommand, ports, urls, artifact)"
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
        assert!(
            err.errors.iter().any(|e| e.contains("is shared by")),
            "{:?}",
            err.errors
        );
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
        assert_eq!(
            catalog
                .services
                .iter()
                .map(|s| s.id.as_str())
                .collect::<Vec<_>>(),
            vec!["api", "postgres", "redis"]
        );
        let postgres = catalog
            .services
            .iter()
            .find(|s| s.id == "postgres")
            .unwrap();
        assert_eq!(
            postgres.ownership,
            Some(crate::catalog::ServiceOwnership::External)
        );
        assert_eq!(
            postgres.kind,
            Some(crate::catalog::ServiceKind::Infrastructure)
        );
        assert_eq!(postgres.label.as_deref(), Some("postgres@16.4 (shared)"));
        let crate::catalog::ServiceRunProfile::Verified {
            command, readiness, ..
        } = &postgres.profiles.run
        else {
            panic!("shared entries must be verified")
        };
        let crate::catalog::CommandSpec::Argv { argv } = &command.command else {
            panic!("attach must be argv")
        };
        assert_eq!(
            &argv[argv.len() - 3..],
            ["shared", "attach", "postgres@16.4"]
        );
        let crate::catalog::ReadinessSpec::Command { command: probe, .. } = readiness else {
            panic!("probe must be a command readiness")
        };
        let crate::catalog::CommandSpec::Argv { argv: probe_argv } = probe else {
            panic!("probe must be argv")
        };
        assert_eq!(
            &probe_argv[probe_argv.len() - 3..],
            ["shared", "probe", "postgres@16.4"]
        );
        assert!(command.docker_stop_command.is_some(), "stop must detach");
        assert_eq!(
            catalog.groups.get("all").unwrap(),
            &vec![
                "api".to_string(),
                "postgres".to_string(),
                "redis".to_string()
            ]
        );
    }

    /// `shared:` entries may carry `preparationCommand` (project-side hook before every attach),
    /// `attachArgs` (appended to the attach argv, forwarded to recipe provision commands), and
    /// `urls`. `{dataDir}`/`{serviceId}`/`{projectRoot}` render like artifact services.
    #[test]
    fn shared_entries_support_prep_attach_args_and_urls() {
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir,
            "hearth.yaml",
            "version: 1\nshared:\n  nginx:\n    version: \"1.30.5\"\n    preparationCommand: { command: { argv: [prep.sh, \"{dataDir}\"] } }\n    attachArgs: [\"{dataDir}/conf.d\", fixed]\n    urls: [{ url: \"https://x.local/{serviceId}\", label: d }]\nservices:\n  api:\n    run: { argv: [x] }\n    readiness: { kind: process }\n",
        );
        let catalog = load_catalog(dir.path()).unwrap().catalog;
        let nginx = catalog.services.iter().find(|s| s.id == "nginx").unwrap();
        let crate::catalog::ServiceRunProfile::Verified {
            command,
            preparation_command,
            ..
        } = &nginx.profiles.run
        else {
            panic!()
        };
        let crate::catalog::CommandSpec::Argv { argv } = &command.command else {
            panic!()
        };
        let data_dir =
            crate::paths::service_data_dir(&dir.path().join(".hearth/runtime-v1"), "nginx")
                .display()
                .to_string();
        assert_eq!(
            &argv[argv.len() - 5..],
            [
                "shared",
                "attach",
                "nginx@1.30.5",
                &format!("{data_dir}/conf.d"),
                "fixed"
            ],
            "{argv:?}"
        );
        let prep = preparation_command.as_ref().expect("prep must render");
        let crate::catalog::CommandSpec::Argv { argv: prep_argv } = &prep.command else {
            panic!()
        };
        assert_eq!(prep_argv, &vec!["prep.sh".to_string(), data_dir]);
        assert_eq!(nginx.urls.as_ref().unwrap()[0].url, "https://x.local/nginx");
    }

    #[test]
    fn shared_entries_reject_unknown_keys() {
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir,
            "hearth.yaml",
            "version: 1\nshared:\n  nginx: { version: \"1.30.5\", bogus: 1 }\nservices:\n  api:\n    run: { argv: [x] }\n    readiness: { kind: process }\n",
        );
        let err = load_catalog(dir.path()).unwrap_err();
        assert!(
            err.errors
                .iter()
                .any(|e| e.contains("unknown key \"bogus\"")),
            "{:?}",
            err.errors
        );
    }

    /// `groups:` members may name other groups — expansion is depth-first in declaration order,
    /// deduplicated on first occurrence, and `groupTree` keeps the raw declared members for
    /// grouped display.
    #[test]
    fn groups_may_reference_other_groups() {
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir,
            "hearth.yaml",
            "version: 1\ngroups:\n  infra: [db, cache]\n  app: [api]\n  all: [infra, app, db]\nservices:\n  db:\n    run: { argv: [x] }\n    readiness: { kind: process }\n  cache:\n    run: { argv: [x] }\n    readiness: { kind: process }\n  api:\n    run: { argv: [x] }\n    readiness: { kind: process }\n",
        );
        let catalog = load_catalog(dir.path()).unwrap().catalog;
        assert_eq!(
            catalog.groups["all"],
            vec!["db".to_string(), "cache".to_string(), "api".to_string()]
        );
        assert_eq!(
            catalog.groups["infra"],
            vec!["db".to_string(), "cache".to_string()]
        );
        let all = catalog.group_tree.iter().find(|g| g.name == "all").unwrap();
        assert_eq!(
            all.members,
            vec!["infra".to_string(), "app".to_string(), "db".to_string()]
        );
    }

    #[test]
    fn group_cycles_are_load_errors_not_hangs() {
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir,
            "hearth.yaml",
            "version: 1\ngroups:\n  a: [b]\n  b: [a]\nservices:\n  api:\n    run: { argv: [x] }\n    readiness: { kind: process }\n",
        );
        let err = load_catalog(dir.path()).unwrap_err();
        assert!(
            err.errors
                .iter()
                .any(|e| e.contains("group cycle: a -> b -> a")
                    || e.contains("group cycle: b -> a -> b")),
            "{:?}",
            err.errors
        );
    }

    /// A member that is neither a service nor a group fails the load.
    #[test]
    fn group_members_must_be_services_or_groups() {
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir,
            "hearth.yaml",
            "version: 1\ngroups:\n  app: [api, ghost]\nservices:\n  api:\n    run: { argv: [x] }\n    readiness: { kind: process }\n",
        );
        let err = load_catalog(dir.path()).unwrap_err();
        assert!(
            err.errors
                .iter()
                .any(|e| e.contains("group app references unknown service or group ghost")),
            "{:?}",
            err.errors
        );
    }

    /// `disabled: true` keeps the service in the catalog and in `groupTree` (display membership),
    /// but group targets expand past it — `hearthd start <group>` never touches it.
    #[test]
    fn disabled_services_leave_group_targets() {
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir,
            "hearth.yaml",
            "version: 1\ngroups:\n  all: [a, b]\n  app: [b]\nservices:\n  a:\n    disabled: true\n    run: { argv: [x] }\n    readiness: { kind: process }\n  b:\n    run: { argv: [y] }\n    readiness: { kind: process }\n",
        );
        let catalog = load_catalog(dir.path()).unwrap().catalog;
        assert!(catalog.services[0].disabled);
        assert!(!catalog.services[1].disabled);
        assert_eq!(catalog.groups["all"], vec!["b".to_string()]);
        assert_eq!(catalog.groups["app"], vec!["b".to_string()]);
        assert_eq!(
            catalog.group_tree[0].members,
            vec!["a".to_string(), "b".to_string()]
        );
    }

    #[test]
    fn services_or_shared_that_are_not_objects_fail_the_load() {
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir,
            "hearth.yaml",
            "version: 1\nservices: []\nshared:\n  redis: \"7.2\"\n",
        );
        let errors = load_catalog(dir.path()).expect_err("services array").errors;
        assert!(
            errors.iter().any(|e| e.contains("services must be a map")),
            "{errors:?}"
        );
        assert!(
            errors.iter().all(|e| !e.contains("non-empty")),
            "the emptiness check must not replace the type error: {errors:?}"
        );

        write(
            &dir,
            "hearth.yaml",
            "version: 1\nservices: \"api\"\nshared:\n  redis: \"7.2\"\n",
        );
        let errors = load_catalog(dir.path())
            .expect_err("services string")
            .errors;
        assert!(
            errors.iter().any(|e| e.contains("services must be a map")),
            "{errors:?}"
        );

        write(
            &dir,
            "hearth.yaml",
            "version: 1\nshared: []\nservices:\n  api:\n    run: { argv: [x] }\n    readiness: { kind: process }\n",
        );
        let errors = load_catalog(dir.path()).expect_err("shared array").errors;
        assert!(
            errors.iter().any(|e| e.contains("shared must be a map")),
            "{errors:?}"
        );

        write(
            &dir,
            "hearth.yaml",
            "version: 1\nservices: {}\nshared:\n  redis: \"7.2\"\n",
        );
        load_catalog(dir.path()).expect("empty services object with a shared entry");
        write(
            &dir,
            "hearth.yaml",
            "version: 1\nshared: {}\nservices:\n  api:\n    run: { argv: [x] }\n    readiness: { kind: process }\n",
        );
        load_catalog(dir.path()).expect("empty shared object with a service");
    }

    #[test]
    fn a_project_may_declare_only_shared_services() {
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir,
            "hearth.yaml",
            "version: 1\nshared:\n  redis: \"7.2\"\n",
        );
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
        assert!(
            errors.iter().any(|e| e.contains("invalid service name")),
            "{errors:?}"
        );
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
        assert!(
            errors.iter().any(|e| e.contains("duplicate service api")),
            "{errors:?}"
        );
    }

    /// A missing `readiness` used to be swallowed by an `and_then` on the absent key: no error was
    /// recorded, the service was then skipped by the `let Some(profile_run) … else { continue }`
    /// guard, and the catalog loaded *successfully* without it. A typo'd key silently deleted a
    /// service from the catalog.
    #[test]
    fn a_service_missing_readiness_fails_the_load_instead_of_vanishing() {
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir,
            "hearth.yaml",
            "version: 1\nservices:\n  api:\n    run: { argv: [x] }\n",
        );
        let error = load_catalog(dir.path()).expect_err("must not load");
        let text = format!("{error:?}");
        assert!(
            text.contains("readiness"),
            "the error must name the missing field, got: {text}"
        );
    }

    /// Document order, not alphabetical: the TS loader iterates insertion order, and this ordering
    /// is user-visible in `/v1/catalog`, `hearthd status`, and the TUI.
    #[test]
    fn services_keep_their_document_order() {
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir,
            "hearth.yaml",
            "version: 1\nservices:\n  zebra:\n    run: { argv: [x] }\n    readiness: { kind: process }\n  alpha:\n    run: { argv: [x] }\n    readiness: { kind: process }\n  middle:\n    run: { argv: [x] }\n    readiness: { kind: process }\n",
        );
        let loaded = load_catalog(dir.path()).expect("loads");
        let ids: Vec<&str> = loaded
            .catalog
            .services
            .iter()
            .map(|s| s.id.as_str())
            .collect();
        assert_eq!(
            ids,
            vec!["zebra", "alpha", "middle"],
            "alphabetizing here diverges from the TS loader"
        );
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
        let urls = load_catalog(dir.path()).expect("loads").catalog.services[0]
            .urls
            .clone()
            .expect("urls");
        assert_eq!(
            urls[0],
            ServiceUrl {
                url: "http://127.0.0.1:8080".into(),
                label: None,
                requires_running: None
            }
        );
        assert_eq!(
            urls[1],
            ServiceUrl {
                url: "https://{tailnetHost}:8443".into(),
                label: Some("admin".into()),
                requires_running: Some(false)
            }
        );
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
            vec![
                "api:urls[0] has unknown placeholder {tailnethost} (known: {tailnetHost})"
                    .to_string(),
                "api:urls[1] must be an http:// or https:// URL".to_string()
            ]
        );
    }

    #[test]
    fn parses_artifact_and_renders_vars_into_run_env_and_urls() {
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir,
            "hearth.yaml",
            r#"
version: 1
services:
  db:
    artifact:
      version: "16.4"
      url: "https://example.test/pg.tar.gz"
      sha256: "abc123"
    run: { argv: ["{installDir}/bin/postgres", "-D", "{dataDir}", "-p", "{port}"] }
    env: { PGDATA: "{dataDir}" }
    readiness: { kind: tcp, port: 5432 }
    urls:
      - "http://127.0.0.1:{port}/"
"#,
        );
        let loaded = load_catalog(dir.path()).expect("should load");
        let service = &loaded.catalog.services[0];
        let artifact = service.artifact.as_ref().expect("artifact");
        let runtime = dir.path().join(".hearth/runtime-v1");
        let install_dir = runtime.join("installs/db/16.4");
        assert_eq!(artifact.version, "16.4");
        assert_eq!(
            artifact.install_dir.as_deref(),
            Some(install_dir.to_str().unwrap())
        );
        assert_eq!(
            artifact.data_dir.as_deref(),
            Some(runtime.join("data/db").to_str().unwrap())
        );
        match &service.profiles.run {
            ServiceRunProfile::Verified { command, .. } => match &command.command {
                CommandSpec::Argv { argv } => {
                    assert_eq!(argv[0], format!("{}/bin/postgres", install_dir.display()));
                    assert_eq!(argv[1], "-D");
                    assert_eq!(argv[2], format!("{}", runtime.join("data/db").display()));
                    assert_eq!(argv[4], "5432");
                }
                _ => panic!("expected argv"),
            },
            _ => panic!("expected verified"),
        }
        let env = match &service.profiles.run {
            ServiceRunProfile::Verified { command, .. } => command.environment.clone().unwrap(),
            _ => panic!(),
        };
        assert_eq!(
            env["PGDATA"],
            format!("{}", runtime.join("data/db").display())
        );
        assert_eq!(
            service.urls.as_ref().unwrap()[0].url,
            "http://127.0.0.1:5432/"
        );
    }

    #[test]
    fn artifact_port_vars_come_from_declared_ports() {
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir,
            "hearth.yaml",
            r#"
version: 1
services:
  db:
    artifact: { version: "1.0", script: "pack.sh" }
    run: { argv: ["x", "{port}", "{port2}"] }
    readiness: { kind: process }
    ports:
      - { port: 1000, label: client }
      - { port: 1001, label: admin }
"#,
        );
        let loaded = load_catalog(dir.path()).expect("should load");
        match &loaded.catalog.services[0].profiles.run {
            ServiceRunProfile::Verified { command, .. } => match &command.command {
                CommandSpec::Argv { argv } => assert_eq!(
                    argv,
                    &vec!["x".to_string(), "1000".to_string(), "1001".to_string()]
                ),
                _ => panic!("expected argv"),
            },
            _ => panic!("expected verified"),
        }
    }

    #[test]
    fn rejects_artifact_shape_and_context_errors() {
        let cases = [
            // version required
            (
                "artifact: { url: 'file:///x.tgz', sha256: abc }",
                "version must be a non-empty",
            ),
            // url xor script
            (
                "artifact: { version: '1', url: 'file:///x.tgz', sha256: abc, script: pack.sh }",
                "exactly one of url or script",
            ),
            ("artifact: { version: '1' }", "exactly one of url or script"),
            // url needs sha256
            (
                "artifact: { version: '1', url: 'file:///x.tgz' }",
                "sha256 is required",
            ),
            // scriptArgs must be strings
            (
                "artifact: { version: '1', script: pack.sh, scriptArgs: [1] }",
                "scriptArgs must be an array",
            ),
        ];
        for (snippet, expected) in cases {
            let dir = tempfile::tempdir().unwrap();
            write(
                &dir,
                "hearth.yaml",
                &format!("version: 1\nservices:\n  db:\n    {snippet}\n    run: {{ argv: [x] }}\n    readiness: {{ kind: process }}\n"),
            );
            let errors = load_catalog(dir.path()).expect_err("must not load").errors;
            assert!(
                errors.iter().any(|e| e.contains(expected)),
                "{expected:?} not in {errors:?}"
            );
        }
    }

    #[test]
    fn rejects_artifact_on_external_or_runless_services() {
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir,
            "hearth.yaml",
            "version: 1\nservices:\n  ext:\n    ownership: external\n    artifact: { version: '1', script: pack.sh }\n    readiness: { kind: process }\n",
        );
        let errors = load_catalog(dir.path()).expect_err("must not load").errors;
        assert!(
            errors.iter().any(|e| e.contains("daemon-owned")),
            "{errors:?}"
        );
        assert!(
            errors.iter().any(|e| e.contains("needs a run command")),
            "{errors:?}"
        );
    }

    #[test]
    fn rejects_an_unresolvable_port_var() {
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir,
            "hearth.yaml",
            "version: 1\nservices:\n  db:\n    artifact: { version: '1', script: pack.sh }\n    run: { argv: [x, \"{port}\"] }\n    readiness: { kind: process }\n",
        );
        let errors = load_catalog(dir.path()).expect_err("must not load").errors;
        assert!(
            errors
                .iter()
                .any(|e| e.contains("{port}") && e.contains("no value")),
            "{errors:?}"
        );
    }

    #[test]
    fn leaves_unknown_braces_alone_in_artifact_commands() {
        // Shell commands legitimately carry braces (awk '{print}') — only the artifact var names
        // are rendered/checked.
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir,
            "hearth.yaml",
            "version: 1\nservices:\n  db:\n    artifact: { version: '1', script: pack.sh }\n    run: { shell: \"echo awk '{print $1}' && sleep 1\" }\n    readiness: { kind: process }\n",
        );
        load_catalog(dir.path()).expect("braces that aren't artifact vars must pass");
    }

    #[test]
    fn artifact_readiness_commands_keep_shell_expansions() {
        // `${HOME}` and `awk '{print}'` in a readiness command used to fail the load as an unknown
        // template var; a real missing var after an unrelated brace pair must still be caught.
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir,
            "hearth.yaml",
            "version: 1\nservices:\n  db:\n    artifact: { version: '1', script: pack.sh }\n    run: { argv: [x] }\n    readiness: { kind: command, command: { shell: \"test -d ${HOME} && awk '{print}' {dataDir}/ok\" } }\n",
        );
        load_catalog(dir.path()).expect("shell expansions are not template vars");

        write(
            &dir,
            "hearth.yaml",
            "version: 1\nservices:\n  db:\n    artifact: { version: '1', script: pack.sh }\n    run: { argv: [x] }\n    readiness: { kind: command, command: { shell: \"awk '{print}' {port2}\" } }\n",
        );
        let errors = load_catalog(dir.path()).expect_err("must not load").errors;
        assert!(
            errors
                .iter()
                .any(|e| e.contains("{port2}") && e.contains("readiness")),
            "{errors:?}"
        );
    }

    #[test]
    fn artifact_readiness_and_preparation_cwd_render_like_run_cwd() {
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir,
            "hearth.yaml",
            "version: 1\nservices:\n  db:\n    artifact: { version: '1', script: pack.sh }\n    cwd: \"{dataDir}\"\n    run: { argv: [x] }\n    ports: [{ port: 5432, label: db }]\n    readiness: { kind: command, command: { argv: [check] }, cwd: \"{dataDir}/{port}\" }\n    preparationCommand: { command: { argv: [prep, \"awk '{print}'\"] }, cwd: \"{port}\" }\n",
        );
        let loaded = load_catalog(dir.path()).expect("cwd templates should render");
        let data_dir = loaded.catalog.services[0]
            .artifact
            .as_ref()
            .unwrap()
            .data_dir
            .clone()
            .unwrap();
        match &loaded.catalog.services[0].profiles.run {
            ServiceRunProfile::Verified {
                command,
                readiness,
                preparation_command,
                ..
            } => {
                assert_eq!(command.cwd, data_dir);
                let rendered = format!("{data_dir}/5432");
                match readiness {
                    ReadinessSpec::Command { cwd, .. } => {
                        assert_eq!(cwd.as_deref(), Some(rendered.as_str()))
                    }
                    _ => panic!("expected command readiness"),
                }
                let prep = preparation_command.as_ref().unwrap();
                assert_eq!(prep.cwd.as_deref(), Some("5432"));
                match &prep.command {
                    CommandSpec::Argv { argv } => assert_eq!(argv[1], "awk '{print}'"),
                    _ => panic!("expected argv"),
                }
            }
            _ => panic!("expected verified"),
        }

        write(
            &dir,
            "hearth.yaml",
            "version: 1\nservices:\n  db:\n    artifact: { version: '1', script: pack.sh }\n    run: { argv: [x] }\n    readiness: { kind: command, command: { argv: [check] }, cwd: \"{port}\" }\n    preparationCommand: { command: { argv: [prep] }, cwd: \"awk '{print}' {port}\" }\n",
        );
        let errors = load_catalog(dir.path()).expect_err("missing port").errors;
        assert!(
            errors
                .iter()
                .any(|e| e.contains("readiness.cwd") && e.contains("{port}")),
            "{errors:?}"
        );
        assert!(
            errors
                .iter()
                .any(|e| e.contains("preparationCommand.cwd") && e.contains("{port}")),
            "{errors:?}"
        );
    }
}
