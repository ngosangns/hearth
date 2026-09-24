//! Builds the smp daemon's in-memory `ServiceCatalog` from the instance registry — the daemon's
//! catalog is derived state, not a yaml file (a consumer authors a `ServiceCatalog` value and
//! passes it in; nothing in the crate requires a file-backed catalog).
use std::collections::HashMap;
use std::path::Path;

use crate::catalog::{
    CommandSpec, ReadinessSpec, ServiceCatalog, ServiceCommand, ServiceDefinition, ServiceId, ServiceKind, ServiceOwnership,
    ServicePort, ServiceProfiles, ServiceRunProfile, StartFailurePolicy,
};

use super::registry::SharedInstance;
use super::render::{instance_vars, render_command, render_env, render_readiness};
use super::SHARED_RUNTIME_DIRECTORY_NAME;

/// Instances in `installing`/`failed` state still synthesize a definition: a failed install is
/// retried by the next attach, and the service must exist in the catalog for the supervisor to
/// start it once install succeeds.
pub fn synthesize_catalog(root: &Path, instances: &[SharedInstance]) -> Result<ServiceCatalog, crate::shared::SharedError> {
    let mut services = Vec::new();
    for instance in instances {
        services.push(synthesize_service(root, instance)?);
    }
    Ok(ServiceCatalog {
        services,
        groups: HashMap::from([("all".to_string(), instances.iter().map(|i| i.id()).collect::<Vec<ServiceId>>())]),
        compose_file: None,
        runtime_directory: Some(SHARED_RUNTIME_DIRECTORY_NAME.to_string()),
        start_failure_policy: StartFailurePolicy::StopOnFirstFailureKeepStarted,
        private_file_guard: None,
    })
}

pub fn synthesize_service(root: &Path, instance: &SharedInstance) -> Result<ServiceDefinition, crate::shared::SharedError> {
    let vars = instance_vars(instance, root);
    let readiness = render_readiness(&instance.recipe.readiness, &vars)?;
    let environment = instance.recipe.env.as_ref().map(|e| render_env(e, &vars)).filter(|e| !e.is_empty());
    Ok(ServiceDefinition {
        id: instance.id(),
        label: Some(format!("{}@{} (shared)", instance.name, instance.version)),
        kind: Some(ServiceKind::Infrastructure),
        ownership: Some(ServiceOwnership::Daemon),
        profiles: ServiceProfiles {
            run: ServiceRunProfile::Verified {
                command: ServiceCommand {
                    command: render_command(&instance.recipe.run, &vars),
                    // The data dir as cwd: recipe binaries write their state under {dataDir}.
                    cwd: instance.data_dir(root).display().to_string(),
                    environment,
                    container_name: None,
                    docker_stop_command: instance.recipe.stop.as_ref().map(|s| render_command(s, &vars)),
                },
                readiness,
                readiness_timeout_ms: None,
                preparation: None,
                preparation_command: None,
            },
            build: None,
        },
        ports: Some(vec![ServicePort { port: instance.port, label: "shared".to_string(), requires_running: None }]),
        urls: None,
    })
}

/// First installs download+extract a tarball inside the attach task — the generated readiness probe
/// must out-wait that, not the usual 10s default.
pub const SHARED_READINESS_TIMEOUT_MS: u64 = 600_000;

/// The project-side service a `shared:` yaml entry expands to (see `docs/shared-services.md`).
/// `ownership: external` + `command` readiness makes its run command a one-shot task
/// (`hearthd shared attach`) and its probe (`hearthd shared probe`, exit 0 iff the instance is
/// ready AND this project is attached) the adoption signal `syncExternalServices` polls. `stop` is
/// `hearthd shared detach` — released via the probe going false, never by killing the singleton.
/// `exe` is the hearthd binary's own path so the commands never depend on PATH.
pub fn project_service_entry(id: ServiceId, instance: &str, exe: &Path) -> ServiceDefinition {
    let exe = exe.display().to_string();
    ServiceDefinition {
        label: Some(format!("{instance} (shared)")),
        id,
        kind: Some(ServiceKind::Infrastructure),
        ownership: Some(ServiceOwnership::External),
        profiles: ServiceProfiles {
            run: ServiceRunProfile::Verified {
                command: ServiceCommand {
                    command: CommandSpec::Argv { argv: vec![exe.clone(), "shared".to_string(), "attach".to_string(), instance.to_string()] },
                    cwd: ".".to_string(),
                    environment: None,
                    container_name: None,
                    docker_stop_command: Some(CommandSpec::Argv { argv: vec![exe.clone(), "shared".to_string(), "detach".to_string(), instance.to_string()] }),
                },
                readiness: ReadinessSpec::Command {
                    command: CommandSpec::Argv { argv: vec![exe, "shared".to_string(), "probe".to_string(), instance.to_string()] },
                    cwd: None,
                },
                readiness_timeout_ms: Some(SHARED_READINESS_TIMEOUT_MS),
                preparation: None,
                preparation_command: None,
            },
            build: None,
        },
        ports: None,
        urls: None,
    }
}
