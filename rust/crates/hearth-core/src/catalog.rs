//! Public config/catalog API. A consumer authors one
//! `ServiceCatalog` value describing its own services and passes it to every other entry point;
//! nothing in this crate imports a catalog directly.
use std::collections::HashMap;

use serde::{Deserialize, Serialize};

pub type ServiceId = String;

/// How to invoke a command. `Argv` is injection-safe (spawned directly, no shell); `Shell` supports
/// the chaining some build tools need — set `exec: true` when the shell command itself execs into
/// the long-running process, so the shell doesn't linger as a wrapper.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum CommandSpec {
    Argv {
        argv: Vec<String>,
    },
    Shell {
        shell: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        exec: Option<bool>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServiceCommand {
    pub command: CommandSpec,
    pub cwd: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub environment: Option<HashMap<String, String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub container_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub docker_stop_command: Option<CommandSpec>,
}

/// The `custom` variant (an in-process closure) can't be represented in the JSON-serializable
/// `ReadinessSpec` used for wire transfer / config-file authoring — it only exists as a Rust-side
/// construction for a programmatically-built catalog (mirrors `{kind:"command"}` being the
/// JSON-serializable stand-in for it, per `catalog.ts`'s own doc comment).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum ReadinessSpec {
    Process,
    Tcp {
        port: u16,
    },
    Http {
        url: String,
    },
    Container,
    Tailnet,
    /// A declarative, JSON-serializable stand-in for a custom probe (exit code 0 = ready, anything
    /// else = not-ready-yet — never "failed", so it retries the same way tcp/http do until the
    /// readiness timeout). `cwd` is relative to the manager's root; omitted defaults to the root
    /// itself.
    Command {
        command: CommandSpec,
        #[serde(skip_serializing_if = "Option::is_none")]
        cwd: Option<String>,
    },
    /// The run command is the job. Exit 0 settles `succeeded` with desired `stopped`; any other
    /// exit is `failed`. There is no liveness probe after the process is gone.
    Exit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ServiceOwnership {
    Daemon,
    External,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ServiceKind {
    Application,
    Infrastructure,
}

/// A declarative, JSON-serializable stand-in for a bespoke `PreparationAdapter` (exit code 0 =
/// prepared, anything else = failed) — exists for exactly the same reason `ReadinessSpec::Command`
/// exists: a config-file-authored catalog has no closures, and this needs to travel over `POST
/// /v1/manager/reload` or a YAML file as plain JSON. Runs via `ProbeAdapter::command` (the same
/// adapter `ReadinessSpec::Command` readiness already uses), independently of the opaque
/// `preparation` marker list on `ServiceRunProfile` — a service may use either, both, or neither.
/// `cwd` is relative to the manager's root; omitted defaults to the root itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PreparationCommand {
    pub command: CommandSpec,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// Services that share a `serialization_key` run their preparation command one at a time —
    /// same idea, same mechanism (`KeyedLock`), as `ServiceBuildProfile::serialization_key`. Set
    /// this when preparation touches shared, non-concurrency-safe state (e.g. a check-then-generate
    /// shared cert/config file with no locking of its own).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub serialization_key: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "commandStatus",
    rename_all = "lowercase",
    rename_all_fields = "camelCase"
)]
pub enum ServiceRunProfile {
    Verified {
        command: ServiceCommand,
        readiness: ReadinessSpec,
        #[serde(skip_serializing_if = "Option::is_none")]
        readiness_timeout_ms: Option<u64>,
        /// Deprecated: opaque preparation markers from the TS era — no catalog producer sets them.
        /// Use `preparation_command`. Kept only so older wire payloads still deserialize.
        #[serde(skip_serializing_if = "Option::is_none")]
        preparation: Option<Vec<String>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        preparation_command: Option<PreparationCommand>,
    },
    Unresolved {
        readiness: ReadinessSpec,
        #[serde(skip_serializing_if = "Option::is_none")]
        readiness_timeout_ms: Option<u64>,
        /// Deprecated: opaque preparation markers from the TS era — no catalog producer sets them.
        /// Use `preparation_command`. Kept only so older wire payloads still deserialize.
        #[serde(skip_serializing_if = "Option::is_none")]
        preparation: Option<Vec<String>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        preparation_command: Option<PreparationCommand>,
    },
}

impl ServiceRunProfile {
    pub fn readiness(&self) -> &ReadinessSpec {
        match self {
            ServiceRunProfile::Verified { readiness, .. } => readiness,
            ServiceRunProfile::Unresolved { readiness, .. } => readiness,
        }
    }

    pub fn is_verified(&self) -> bool {
        matches!(self, ServiceRunProfile::Verified { .. })
    }
}

/// Opt-in compile/build step run once before the run command starts. Services that share a
/// `serialization_key` run their builds one at a time — set this when the underlying build tool
/// (e.g. a shared Gradle daemon) cannot run concurrent builds safely.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServiceBuildProfile {
    pub command: ServiceCommand,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub serialization_key: Option<String>,
}

/// A versioned tarball installed before the service's run command starts — the same
/// download/script → sha256 → extract → `.hearth-installed` marker machinery shared services use
/// (`shared/install.rs`), scoped to this project instead of `~/.hearth/shared`: installs land in
/// `<runtimeDir>/installs/<serviceId>/<version>` and the service gets a persistent
/// `<runtimeDir>/data/<serviceId>`. The config-file parser renders `{installDir}`, `{dataDir}`,
/// `{port}`/`{port2}…` (the service's declared ports, or the tcp readiness port for `{port}`),
/// `{serviceId}` and `{projectRoot}` into run/stop/build/preparation/env/readiness/urls at load
/// time, so the resolved definition always carries literal paths.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServiceArtifact {
    /// Versions the install dir — bump it and the next start installs into a fresh directory.
    pub version: String,
    /// A downloadable tarball (`https://` or `file://`). Mutually exclusive with `script`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// A packager script run as `bash <script> <scriptArgs...> <out.tar.gz>` — a project-relative
    /// path or a URL. Mutually exclusive with `url`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub script: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub script_args: Vec<String>,
    /// Enforced for `url` artifacts; `script` output is never byte-reproducible (tar mtimes,
    /// compile variance), so no committed hash can match — the script itself is the trust
    /// boundary.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    /// Absolute paths the installer fills from its runtime directory when unset. File-loaded
    /// catalogs always resolve these at parse time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub install_dir: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_dir: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServicePort {
    pub port: u16,
    pub label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub requires_running: Option<bool>,
}

/// A URL where a service can be reached, surfaced by every client (CLI `urls`, TUI, web, MCP
/// `status`). `url` may contain placeholders from `SERVICE_URL_PLACEHOLDERS`, resolved by the
/// daemon at request time — `{tailnetHost}` becomes this machine's Tailscale DNS name, so a catalog
/// shared across machines never hardcodes one machine's hostname. `requires_running: Some(false)`
/// marks a URL that works even while this service is stopped. Mirrors `ServiceUrl` in the TS source.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServiceUrl {
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub requires_running: Option<bool>,
}

/// Placeholders a `ServiceUrl::url` may contain. Anything else in braces is a validation error, so a
/// typo'd placeholder fails the catalog load instead of rendering a dead link.
pub const SERVICE_URL_PLACEHOLDERS: [&str; 1] = ["tailnetHost"];

/// Every innermost `{…}` pair in `text` as `(byte offset of '{', body)`, in order of appearance.
/// The one brace tokenizer behind URL placeholders and command template vars.
fn brace_spans(text: &str) -> Vec<(usize, &str)> {
    let mut spans = Vec::new();
    let mut offset = 0;
    while let Some(open) = text[offset..].find('{') {
        let start = offset + open;
        let after = &text[start + 1..];
        match after.find(['{', '}']) {
            Some(close) if after.as_bytes()[close] == b'}' => {
                spans.push((start, &after[..close]));
                offset = start + 1 + close + 1;
            }
            Some(close) => offset = start + 1 + close,
            None => break,
        }
    }
    spans
}

/// Names of every `{placeholder}` in a URL template, in order of appearance.
pub fn service_url_placeholders(url: &str) -> Vec<&str> {
    brace_spans(url).into_iter().map(|(_, name)| name).collect()
}

/// Every `{name}` template var in a command/env/url template as `(byte offset of '{', name)`, in
/// order of appearance. Only an identifier-shaped body counts, and a `${NAME}` shell expansion
/// never does — so `${HOME}` and `awk '{print $1}'` pass through untouched.
pub fn template_var_spans(text: &str) -> Vec<(usize, &str)> {
    brace_spans(text)
        .into_iter()
        .filter(|(start, name)| {
            !text[..*start].ends_with('$')
                && name
                    .chars()
                    .next()
                    .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
                && name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.')
        })
        .collect()
}

/// Names of every `{name}` template var in `text` — see `template_var_spans`.
pub fn template_vars(text: &str) -> Vec<&str> {
    template_var_spans(text)
        .into_iter()
        .map(|(_, name)| name)
        .collect()
}

/// The first template var in `text` that `is_known` rejects.
pub fn unknown_template_var(text: &str, is_known: impl Fn(&str) -> bool) -> Option<&str> {
    template_vars(text).into_iter().find(|name| !is_known(name))
}

/// A service URL with its placeholders substituted, as served by `GET /v1/urls`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResolvedServiceUrl {
    pub service_id: ServiceId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    pub url: String,
    /// `false` only when the catalog said the URL works while the service is stopped.
    pub requires_running: bool,
}

/// A URL that could not be resolved because a placeholder had no value on this machine (e.g.
/// `{tailnetHost}` with Tailscale not running). Reported rather than dropped silently, so a client
/// can explain why a link it expected is missing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UnresolvedServiceUrl {
    pub service_id: ServiceId,
    pub url: String,
    pub placeholder: String,
}

/// Substitutes every placeholder in every service URL, in catalog order. `lookup` answers one
/// placeholder name; it is injected so this stays testable without a real Tailscale.
pub fn resolve_service_urls(
    catalog: &ServiceCatalog,
    lookup: impl Fn(&str) -> Option<String>,
) -> (Vec<ResolvedServiceUrl>, Vec<UnresolvedServiceUrl>) {
    let mut resolved = Vec::new();
    let mut unresolved = Vec::new();
    for service in &catalog.services {
        'entries: for entry in service.urls.iter().flatten() {
            let mut url = entry.url.clone();
            for name in service_url_placeholders(&entry.url) {
                match lookup(name) {
                    Some(value) => url = url.replace(&format!("{{{name}}}"), &value),
                    None => {
                        unresolved.push(UnresolvedServiceUrl {
                            service_id: service.id.clone(),
                            url: entry.url.clone(),
                            placeholder: name.to_string(),
                        });
                        continue 'entries;
                    }
                }
            }
            resolved.push(ResolvedServiceUrl {
                service_id: service.id.clone(),
                label: entry.label.clone(),
                url,
                requires_running: entry.requires_running != Some(false),
            });
        }
    }
    (resolved, unresolved)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceProfiles {
    pub run: ServiceRunProfile,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub build: Option<ServiceBuildProfile>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceDefinition {
    pub id: ServiceId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<ServiceKind>,
    /// `Daemon` (default): this manager owns start/stop. `External`: this manager never spawns or
    /// stops the service itself but periodically probes its readiness and adopts/releases it into
    /// its own state machine when detected.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ownership: Option<ServiceOwnership>,
    /// `true`: the service stays in the catalog (visible, loadable) but accepts no
    /// start/stop/restart — direct operations are rejected and group targets expand past it.
    #[serde(default)]
    pub disabled: bool,
    pub profiles: ServiceProfiles,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ports: Option<Vec<ServicePort>>,
    /// Where this service can be reached — see `ServiceUrl`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub urls: Option<Vec<ServiceUrl>>,
    /// A versioned tarball this service installs into the project runtime dir before starting —
    /// see `ServiceArtifact`. Daemon-owned services only; `external` ones never spawn and so never
    /// install.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact: Option<ServiceArtifact>,
}

/// A `groups:` entry as declared — `members` may name services or other groups (nested groups
/// expand into `ServiceCatalog::groups` at load; this keeps the declaration so clients can show
/// direct membership rather than the flattened superset).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogGroup {
    pub name: String,
    pub members: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServiceCatalog {
    pub services: Vec<ServiceDefinition>,
    /// Group name → flattened service ids, nested group members expanded depth-first.
    pub groups: HashMap<String, Vec<ServiceId>>,
    /// Declaration order + raw members (group names included) for grouped display.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub group_tree: Vec<CatalogGroup>,
    /// Reserved: a TS-era field no catalog producer sets and nothing reads. Kept so existing
    /// struct literals across the workspace and older wire payloads keep compiling/loading.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compose_file: Option<String>,
    /// Runtime state directory, relative to the manager's root. Default: `.hearth/runtime-v1`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub runtime_directory: Option<String>,
    /// Single-variant today; defaulted so a catalog that omits it still loads.
    #[serde(default)]
    pub start_failure_policy: StartFailurePolicy,
    /// O_NOFOLLOW + dev/ino private-file guard layer for every lock/state/log file operation.
    /// Default on.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub private_file_guard: Option<bool>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum StartFailurePolicy {
    #[default]
    StopOnFirstFailureKeepStarted,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CatalogValidation {
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
}

pub fn validate_catalog(catalog: &ServiceCatalog) -> CatalogValidation {
    let mut errors = Vec::new();
    let mut warnings = Vec::new();
    let mut services: HashMap<&ServiceId, &ServiceDefinition> = HashMap::new();
    for service in &catalog.services {
        if services.contains_key(&service.id) {
            errors.push(format!("duplicate service {}", service.id));
        }
        services.insert(&service.id, service);
    }

    let mut group_names: Vec<&String> = catalog.groups.keys().collect();
    group_names.sort();
    for group_name in group_names {
        for member in &catalog.groups[group_name] {
            if !services.contains_key(member) {
                errors.push(format!(
                    "group {} references unknown service {}",
                    group_name, member
                ));
            }
        }
    }

    let mut verified_ports: HashMap<u16, &ServiceId> = HashMap::new();
    for service in &catalog.services {
        let profile = &service.profiles.run;
        match profile {
            ServiceRunProfile::Verified { .. } => {}
            ServiceRunProfile::Unresolved { .. } => {
                warnings.push(format!("{}:run command is unresolved", service.id));
            }
        }
        if let Some(build) = &service.profiles.build {
            if let Some(timeout_ms) = build.timeout_ms {
                if timeout_ms == 0 {
                    errors.push(format!("{}:build has an invalid timeout", service.id));
                }
            }
        }
        for (index, entry) in service.urls.iter().flatten().enumerate() {
            let place = format!("{}:urls[{index}]", service.id);
            let well_formed = (entry.url.starts_with("http://")
                || entry.url.starts_with("https://"))
                && entry.url.len() > entry.url.find("://").map_or(0, |i| i + 3)
                && !entry.url.chars().any(char::is_whitespace);
            if !well_formed {
                errors.push(format!("{place} must be an http:// or https:// URL"));
                continue;
            }
            for name in service_url_placeholders(&entry.url) {
                if !SERVICE_URL_PLACEHOLDERS.contains(&name) {
                    let known = SERVICE_URL_PLACEHOLDERS
                        .iter()
                        .map(|k| format!("{{{k}}}"))
                        .collect::<Vec<_>>()
                        .join(", ");
                    errors.push(format!(
                        "{place} has unknown placeholder {{{name}}} (known: {known})"
                    ));
                }
            }
            if entry
                .label
                .as_deref()
                .is_some_and(|label| label.trim().is_empty())
            {
                errors.push(format!("{place}.label must be a non-empty string"));
            }
        }
        if matches!(profile.readiness(), ReadinessSpec::Exit) {
            if matches!(service.ownership, Some(ServiceOwnership::External)) {
                errors.push(format!(
                    "{}: readiness exit requires a daemon-owned service",
                    service.id
                ));
            }
            if let ServiceRunProfile::Verified { command, .. } = profile {
                if is_container_command(command) {
                    errors.push(format!(
                        "{}: readiness exit cannot run a container command",
                        service.id
                    ));
                }
            }
        }
        if let ReadinessSpec::Tcp { port } = profile.readiness() {
            if let Some(existing) = verified_ports.get(port) {
                if *existing != &service.id {
                    errors.push(format!(
                        "port {} is shared by {} and {}",
                        port, existing, service.id
                    ));
                }
            }
            verified_ports.insert(*port, &service.id);
        }
    }

    CatalogValidation { errors, warnings }
}

pub fn is_container_command(command: &ServiceCommand) -> bool {
    command.container_name.is_some()
}

// Ported 1:1 from `test/core/catalog.test.ts`.
#[cfg(test)]
mod tests {
    use super::*;

    fn argv_service(id: &str) -> ServiceDefinition {
        ServiceDefinition {
            id: id.to_string(),
            label: None,
            kind: None,
            ownership: None,
            disabled: false,
            profiles: ServiceProfiles {
                run: ServiceRunProfile::Verified {
                    command: ServiceCommand {
                        command: CommandSpec::Argv {
                            argv: vec![id.to_string()],
                        },
                        cwd: ".".to_string(),
                        environment: None,
                        container_name: None,
                        docker_stop_command: None,
                    },
                    readiness: ReadinessSpec::Process,
                    readiness_timeout_ms: None,
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

    fn tcp_service(id: &str, port: u16) -> ServiceDefinition {
        ServiceDefinition {
            id: id.to_string(),
            label: None,
            kind: None,
            ownership: None,
            disabled: false,
            profiles: ServiceProfiles {
                run: ServiceRunProfile::Verified {
                    command: ServiceCommand {
                        command: CommandSpec::Argv {
                            argv: vec![id.to_string()],
                        },
                        cwd: ".".to_string(),
                        environment: None,
                        container_name: None,
                        docker_stop_command: None,
                    },
                    readiness: ReadinessSpec::Tcp { port },
                    readiness_timeout_ms: None,
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

    fn catalog(services: Vec<ServiceDefinition>, groups: &[(&str, &[&str])]) -> ServiceCatalog {
        ServiceCatalog {
            start_failure_policy: StartFailurePolicy::StopOnFirstFailureKeepStarted,
            groups: groups
                .iter()
                .map(|(name, members)| {
                    (
                        name.to_string(),
                        members.iter().map(|m| m.to_string()).collect(),
                    )
                })
                .collect(),
            group_tree: Vec::new(),
            services,
            compose_file: None,
            runtime_directory: None,
            private_file_guard: None,
        }
    }

    #[test]
    fn accepts_a_well_formed_catalog_with_services_and_groups() {
        let c = catalog(
            vec![argv_service("nginx"), argv_service("api")],
            &[("all", &["nginx", "api"])],
        );
        assert_eq!(
            validate_catalog(&c),
            CatalogValidation {
                errors: vec![],
                warnings: vec![]
            }
        );
    }

    #[test]
    fn rejects_a_duplicate_service_id() {
        let c = catalog(vec![argv_service("api"), argv_service("api")], &[]);
        assert!(validate_catalog(&c)
            .errors
            .contains(&"duplicate service api".to_string()));
    }

    #[test]
    fn rejects_a_group_referencing_an_unknown_service() {
        let c = catalog(vec![argv_service("api")], &[("all", &["ghost"])]);
        assert!(validate_catalog(&c)
            .errors
            .contains(&"group all references unknown service ghost".to_string()));
    }

    #[test]
    fn rejects_two_services_sharing_the_same_tcp_readiness_port() {
        let c = catalog(vec![tcp_service("a", 8080), tcp_service("b", 8080)], &[]);
        assert!(validate_catalog(&c)
            .errors
            .contains(&"port 8080 is shared by a and b".to_string()));
    }

    #[test]
    fn warns_does_not_error_on_an_unresolved_command_status() {
        let c = catalog(
            vec![ServiceDefinition {
                id: "wip".to_string(),
                label: None,
                kind: None,
                ownership: None,
                disabled: false,
                profiles: ServiceProfiles {
                    run: ServiceRunProfile::Unresolved {
                        readiness: ReadinessSpec::Process,
                        readiness_timeout_ms: None,
                        preparation: None,
                        preparation_command: None,
                    },
                    build: None,
                },
                ports: None,
                urls: None,
                artifact: None,
            }],
            &[],
        );
        let validation = validate_catalog(&c);
        assert_eq!(validation.errors, Vec::<String>::new());
        assert_eq!(
            validation.warnings,
            vec!["wip:run command is unresolved".to_string()]
        );
    }

    #[test]
    fn rejects_an_invalid_build_timeout() {
        let mut service = argv_service("api");
        service.profiles.build = Some(ServiceBuildProfile {
            command: ServiceCommand {
                command: CommandSpec::Argv {
                    argv: vec!["build".to_string()],
                },
                cwd: ".".to_string(),
                environment: None,
                container_name: None,
                docker_stop_command: None,
            },
            timeout_ms: Some(0), // stand-in for TS's `-1`: u64 can't represent a negative timeout,
            // so the type system itself rules out the non-integer/negative cases the TS check
            // guards against; 0 is the one remaining representable "invalid" value.
            serialization_key: None,
        });
        let c = catalog(vec![service], &[]);
        assert!(validate_catalog(&c)
            .errors
            .contains(&"api:build has an invalid timeout".to_string()));
    }

    #[test]
    fn exit_readiness_rejects_external_ownership_and_container_commands() {
        let mut external = argv_service("fe");
        external.ownership = Some(ServiceOwnership::External);
        if let ServiceRunProfile::Verified { readiness, .. } = &mut external.profiles.run {
            *readiness = ReadinessSpec::Exit;
        }
        let mut catalog = ServiceCatalog {
            services: vec![external],
            groups: HashMap::new(),
            group_tree: Vec::new(),
            compose_file: None,
            runtime_directory: None,
            start_failure_policy: StartFailurePolicy::StopOnFirstFailureKeepStarted,
            private_file_guard: None,
        };
        assert!(validate_catalog(&catalog)
            .errors
            .iter()
            .any(|error| error.contains("daemon-owned")));

        let mut container = argv_service("job");
        if let ServiceRunProfile::Verified {
            command, readiness, ..
        } = &mut container.profiles.run
        {
            command.container_name = Some("job".to_string());
            *readiness = ReadinessSpec::Exit;
        }
        catalog.services = vec![container];
        assert!(validate_catalog(&catalog)
            .errors
            .iter()
            .any(|error| error.contains("container")));
    }

    // Wire-format regression — see the equivalent tests in state.rs for why this matters.
    #[test]
    fn service_catalog_and_command_serialize_camel_case() {
        let c = catalog(vec![argv_service("api")], &[]);
        let json = serde_json::to_value(&c).unwrap();
        assert!(json.get("startFailurePolicy").is_some(), "{json:?}");

        let command = ServiceCommand {
            command: CommandSpec::Argv {
                argv: vec!["x".to_string()],
            },
            cwd: ".".to_string(),
            environment: None,
            container_name: Some("proj-db".to_string()),
            docker_stop_command: None,
        };
        let json = serde_json::to_value(&command).unwrap();
        assert!(json.get("containerName").is_some(), "{json:?}");

        let profile = ServiceRunProfile::Verified {
            command,
            readiness: ReadinessSpec::Process,
            readiness_timeout_ms: Some(1000),
            preparation: None,
            preparation_command: None,
        };
        let json = serde_json::to_value(&profile).unwrap();
        assert!(json.get("readinessTimeoutMs").is_some(), "{json:?}");
        assert_eq!(
            json.get("commandStatus").and_then(|v| v.as_str()),
            Some("verified")
        );
    }

    #[test]
    fn resolves_placeholders_and_reports_the_ones_it_cannot() {
        let mut api = tcp_service("api", 18080);
        api.urls = Some(vec![
            ServiceUrl {
                url: "http://127.0.0.1:8080".into(),
                label: None,
                requires_running: None,
            },
            ServiceUrl {
                url: "https://{tailnetHost}:8443".into(),
                label: Some("admin".into()),
                requires_running: Some(false),
            },
        ]);
        let catalog = catalog(vec![api], &[]);

        let (urls, unresolved) = resolve_service_urls(&catalog, |name| {
            (name == "tailnetHost").then(|| "box.tail.ts.net".to_string())
        });
        assert!(unresolved.is_empty());
        assert_eq!(urls[0].url, "http://127.0.0.1:8080");
        assert!(urls[0].requires_running, "requiresRunning defaults to true");
        assert_eq!(urls[1].url, "https://box.tail.ts.net:8443");
        assert_eq!(urls[1].label.as_deref(), Some("admin"));
        assert!(!urls[1].requires_running);

        // No Tailscale on this machine: the templated URL is reported, not silently dropped.
        let (urls, unresolved) = resolve_service_urls(&catalog, |_| None);
        assert_eq!(urls.len(), 1);
        assert_eq!(
            unresolved,
            vec![UnresolvedServiceUrl {
                service_id: "api".into(),
                url: "https://{tailnetHost}:8443".into(),
                placeholder: "tailnetHost".into()
            }]
        );
    }

    #[test]
    fn extracts_placeholder_names_in_order() {
        assert_eq!(
            service_url_placeholders("https://{tailnetHost}:{port}/x"),
            vec!["tailnetHost", "port"]
        );
        assert!(service_url_placeholders("http://127.0.0.1:1").is_empty());
    }

    #[test]
    fn template_vars_scan_every_brace_and_skip_shell_expansions() {
        assert_eq!(template_vars("{a b} {port2}"), vec!["port2"]);
        assert_eq!(
            template_vars("cd ${HOME} && awk '{print $1}' {dataDir}"),
            vec!["dataDir"]
        );
        assert_eq!(template_vars("{{installDir}}"), vec!["installDir"]);
        assert_eq!(
            unknown_template_var("{port} {projectDB}", |n| n == "port"),
            Some("projectDB")
        );
        assert_eq!(
            unknown_template_var("${HOME}/{port}", |n| n == "port"),
            None
        );
    }
}
