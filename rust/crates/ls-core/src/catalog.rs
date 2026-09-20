//! Public config/catalog API — port of `src/core/catalog.ts`. A consumer authors one
//! `ServiceCatalog` value describing its own services and passes it to every other entry point;
//! nothing in this crate imports a catalog directly.
use std::collections::{HashMap, HashSet};

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

pub struct ReadinessProbeContext {
    pub service_id: ServiceId,
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "commandStatus", rename_all = "lowercase", rename_all_fields = "camelCase")]
pub enum ServiceRunProfile {
    Verified {
        command: ServiceCommand,
        readiness: ReadinessSpec,
        #[serde(skip_serializing_if = "Option::is_none")]
        readiness_timeout_ms: Option<u64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        preparation: Option<Vec<String>>,
    },
    Unresolved {
        readiness: ReadinessSpec,
        #[serde(skip_serializing_if = "Option::is_none")]
        readiness_timeout_ms: Option<u64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        preparation: Option<Vec<String>>,
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServicePort {
    pub port: u16,
    pub label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub requires_running: Option<bool>,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dependencies: Option<Vec<ServiceId>>,
    pub profiles: ServiceProfiles,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ports: Option<Vec<ServicePort>>,
}

impl ServiceDefinition {
    pub fn dependencies(&self) -> &[ServiceId] {
        self.dependencies.as_deref().unwrap_or(&[])
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServiceCatalog {
    pub services: Vec<ServiceDefinition>,
    pub groups: HashMap<String, Vec<ServiceId>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compose_file: Option<String>,
    /// Runtime state directory, relative to the manager's root. Default: `.local-services/runtime-v1`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub runtime_directory: Option<String>,
    pub start_failure_policy: StartFailurePolicy,
    /// O_NOFOLLOW + dev/ino private-file guard layer for every lock/state/log file operation.
    /// Default on.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub private_file_guard: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum StartFailurePolicy {
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
                errors.push(format!("group {} references unknown service {}", group_name, member));
            }
        }
    }

    for service in &catalog.services {
        for dependency in service.dependencies() {
            if !services.contains_key(dependency) {
                errors.push(format!("{} depends on unknown service {}", service.id, dependency));
            }
        }
    }

    let mut visiting: HashSet<&ServiceId> = HashSet::new();
    let mut visited: HashSet<&ServiceId> = HashSet::new();

    fn visit<'a>(
        service_id: &'a ServiceId,
        path: &mut Vec<&'a ServiceId>,
        services: &HashMap<&'a ServiceId, &'a ServiceDefinition>,
        visiting: &mut HashSet<&'a ServiceId>,
        visited: &mut HashSet<&'a ServiceId>,
        errors: &mut Vec<String>,
    ) {
        if visiting.contains(service_id) {
            let start = path.iter().position(|id| *id == service_id).unwrap_or(0);
            let mut cycle: Vec<&str> = path[start..].iter().map(|s| s.as_str()).collect();
            cycle.push(service_id.as_str());
            errors.push(format!("dependency cycle: {}", cycle.join(" -> ")));
            return;
        }
        if visited.contains(service_id) {
            return;
        }
        visiting.insert(service_id);
        if let Some(service) = services.get(service_id) {
            for dependency in service.dependencies() {
                if services.contains_key(dependency) {
                    path.push(service_id);
                    visit(dependency, path, services, visiting, visited, errors);
                    path.pop();
                }
            }
        }
        visiting.remove(service_id);
        visited.insert(service_id);
    }

    for service in &catalog.services {
        let mut path = Vec::new();
        visit(&service.id, &mut path, &services, &mut visiting, &mut visited, &mut errors);
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
        if let ReadinessSpec::Tcp { port } = profile.readiness() {
            if let Some(existing) = verified_ports.get(port) {
                if *existing != &service.id {
                    errors.push(format!("port {} is shared by {} and {}", port, existing, service.id));
                }
            }
            verified_ports.insert(*port, &service.id);
        }
    }

    CatalogValidation { errors, warnings }
}

#[derive(Debug, thiserror::Error)]
#[error("Invalid service catalog: {0}")]
pub struct InvalidCatalogError(String);

pub fn dependency_levels(
    catalog: &ServiceCatalog,
    targets: &[ServiceId],
) -> Result<Vec<Vec<ServiceId>>, InvalidCatalogError> {
    let validation = validate_catalog(catalog);
    if !validation.errors.is_empty() {
        return Err(InvalidCatalogError(validation.errors.join("; ")));
    }
    let services: HashMap<&ServiceId, &ServiceDefinition> =
        catalog.services.iter().map(|s| (&s.id, s)).collect();

    let mut selected: HashSet<ServiceId> = HashSet::new();
    fn include(
        service_id: &ServiceId,
        services: &HashMap<&ServiceId, &ServiceDefinition>,
        selected: &mut HashSet<ServiceId>,
    ) {
        if selected.contains(service_id) {
            return;
        }
        selected.insert(service_id.clone());
        if let Some(service) = services.get(service_id) {
            for dependency in service.dependencies() {
                include(dependency, services, selected);
            }
        }
    }
    for target in targets {
        include(target, &services, &mut selected);
    }

    let mut remaining: HashMap<ServiceId, usize> = HashMap::new();
    for service_id in &selected {
        let count = services
            .get(service_id)
            .map(|s| s.dependencies().iter().filter(|d| selected.contains(*d)).count())
            .unwrap_or(0);
        remaining.insert(service_id.clone(), count);
    }

    let mut levels: Vec<Vec<ServiceId>> = Vec::new();
    while !remaining.is_empty() {
        let ready: Vec<ServiceId> = catalog
            .services
            .iter()
            .map(|s| s.id.clone())
            .filter(|id| remaining.get(id) == Some(&0))
            .collect();
        if ready.is_empty() {
            return Err(InvalidCatalogError("dependency cycle".to_string()));
        }
        levels.push(ready.clone());
        for id in &ready {
            remaining.remove(id);
        }
        let remaining_ids: Vec<ServiceId> = remaining.keys().cloned().collect();
        for id in remaining_ids {
            let count = services
                .get(&id)
                .map(|s| s.dependencies().iter().filter(|d| remaining.contains_key(*d)).count())
                .unwrap_or(0);
            remaining.insert(id, count);
        }
    }
    Ok(levels)
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
            dependencies: None,
            profiles: ServiceProfiles {
                run: ServiceRunProfile::Verified {
                    command: ServiceCommand {
                        command: CommandSpec::Argv { argv: vec![id.to_string()] },
                        cwd: ".".to_string(),
                        environment: None,
                        container_name: None,
                        docker_stop_command: None,
                    },
                    readiness: ReadinessSpec::Process,
                    readiness_timeout_ms: None,
                    preparation: None,
                },
                build: None,
            },
            ports: None,
        }
    }

    fn argv_service_with_deps(id: &str, dependencies: &[&str]) -> ServiceDefinition {
        ServiceDefinition {
            dependencies: Some(dependencies.iter().map(|d| d.to_string()).collect()),
            ..argv_service(id)
        }
    }

    fn tcp_service(id: &str, port: u16) -> ServiceDefinition {
        ServiceDefinition {
            id: id.to_string(),
            label: None,
            kind: None,
            ownership: None,
            dependencies: None,
            profiles: ServiceProfiles {
                run: ServiceRunProfile::Verified {
                    command: ServiceCommand {
                        command: CommandSpec::Argv { argv: vec![id.to_string()] },
                        cwd: ".".to_string(),
                        environment: None,
                        container_name: None,
                        docker_stop_command: None,
                    },
                    readiness: ReadinessSpec::Tcp { port },
                    readiness_timeout_ms: None,
                    preparation: None,
                },
                build: None,
            },
            ports: None,
        }
    }

    fn catalog(services: Vec<ServiceDefinition>, groups: &[(&str, &[&str])]) -> ServiceCatalog {
        ServiceCatalog {
            start_failure_policy: StartFailurePolicy::StopOnFirstFailureKeepStarted,
            groups: groups
                .iter()
                .map(|(name, members)| (name.to_string(), members.iter().map(|m| m.to_string()).collect()))
                .collect(),
            services,
            compose_file: None,
            runtime_directory: None,
            private_file_guard: None,
        }
    }

    #[test]
    fn accepts_a_well_formed_catalog_with_dependencies_and_groups() {
        let c = catalog(
            vec![argv_service("nginx"), argv_service_with_deps("api", &["nginx"])],
            &[("all", &["nginx", "api"])],
        );
        assert_eq!(validate_catalog(&c), CatalogValidation { errors: vec![], warnings: vec![] });
    }

    #[test]
    fn rejects_a_duplicate_service_id() {
        let c = catalog(vec![argv_service("api"), argv_service("api")], &[]);
        assert!(validate_catalog(&c).errors.contains(&"duplicate service api".to_string()));
    }

    #[test]
    fn rejects_a_group_referencing_an_unknown_service() {
        let c = catalog(vec![argv_service("api")], &[("all", &["ghost"])]);
        assert!(validate_catalog(&c)
            .errors
            .contains(&"group all references unknown service ghost".to_string()));
    }

    #[test]
    fn rejects_a_dependency_on_an_unknown_service() {
        let c = catalog(vec![argv_service_with_deps("api", &["ghost"])], &[]);
        assert!(validate_catalog(&c).errors.contains(&"api depends on unknown service ghost".to_string()));
    }

    #[test]
    fn detects_a_dependency_cycle_and_reports_the_cycle_path() {
        let c = catalog(vec![argv_service_with_deps("a", &["b"]), argv_service_with_deps("b", &["a"])], &[]);
        assert_eq!(validate_catalog(&c).errors, vec!["dependency cycle: a -> b -> a".to_string()]);
    }

    #[test]
    fn rejects_two_services_sharing_the_same_tcp_readiness_port() {
        let c = catalog(vec![tcp_service("a", 8080), tcp_service("b", 8080)], &[]);
        assert!(validate_catalog(&c).errors.contains(&"port 8080 is shared by a and b".to_string()));
    }

    #[test]
    fn warns_does_not_error_on_an_unresolved_command_status() {
        let c = catalog(
            vec![ServiceDefinition {
                id: "wip".to_string(),
                label: None,
                kind: None,
                ownership: None,
                dependencies: None,
                profiles: ServiceProfiles {
                    run: ServiceRunProfile::Unresolved { readiness: ReadinessSpec::Process, readiness_timeout_ms: None, preparation: None },
                    build: None,
                },
                ports: None,
            }],
            &[],
        );
        let validation = validate_catalog(&c);
        assert_eq!(validation.errors, Vec::<String>::new());
        assert_eq!(validation.warnings, vec!["wip:run command is unresolved".to_string()]);
    }

    #[test]
    fn rejects_an_invalid_build_timeout() {
        let mut service = argv_service("api");
        service.profiles.build = Some(ServiceBuildProfile {
            command: ServiceCommand {
                command: CommandSpec::Argv { argv: vec!["build".to_string()] },
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
        assert!(validate_catalog(&c).errors.contains(&"api:build has an invalid timeout".to_string()));
    }

    #[test]
    fn orders_a_diamond_dependency_graph_into_levels_by_depth() {
        let c = catalog(
            vec![
                argv_service("nginx"),
                argv_service_with_deps("mongo", &["nginx"]),
                argv_service_with_deps("redis", &["nginx"]),
                argv_service_with_deps("api", &["mongo", "redis"]),
            ],
            &[],
        );
        let levels = dependency_levels(&c, &["api".to_string()]).unwrap();
        assert_eq!(
            levels,
            vec![vec!["nginx".to_string()], vec!["mongo".to_string(), "redis".to_string()], vec!["api".to_string()]]
        );
    }

    #[test]
    fn includes_only_the_transitive_dependencies_of_the_requested_targets() {
        let c = catalog(
            vec![argv_service("nginx"), argv_service_with_deps("api", &["nginx"]), argv_service("unrelated")],
            &[],
        );
        let levels = dependency_levels(&c, &["api".to_string()]).unwrap();
        let flat: Vec<String> = levels.into_iter().flatten().collect();
        assert_eq!(flat, vec!["nginx".to_string(), "api".to_string()]);
    }

    #[test]
    fn throws_when_the_catalog_itself_is_invalid() {
        let c = catalog(vec![argv_service_with_deps("a", &["b"]), argv_service_with_deps("b", &["a"])], &[]);
        let err = dependency_levels(&c, &["a".to_string()]).unwrap_err();
        assert!(err.to_string().contains("Invalid service catalog"));
    }

    // Wire-format regression — see the equivalent tests in state.rs for why this matters.
    #[test]
    fn service_catalog_and_command_serialize_camel_case() {
        let c = catalog(vec![argv_service("api")], &[]);
        let json = serde_json::to_value(&c).unwrap();
        assert!(json.get("startFailurePolicy").is_some(), "{json:?}");

        let command = ServiceCommand {
            command: CommandSpec::Argv { argv: vec!["x".to_string()] },
            cwd: ".".to_string(),
            environment: None,
            container_name: Some("proj-db".to_string()),
            docker_stop_command: None,
        };
        let json = serde_json::to_value(&command).unwrap();
        assert!(json.get("containerName").is_some(), "{json:?}");

        let profile = ServiceRunProfile::Verified { command, readiness: ReadinessSpec::Process, readiness_timeout_ms: Some(1000), preparation: None };
        let json = serde_json::to_value(&profile).unwrap();
        assert!(json.get("readinessTimeoutMs").is_some(), "{json:?}");
        assert_eq!(json.get("commandStatus").and_then(|v| v.as_str()), Some("verified"));
    }
}
