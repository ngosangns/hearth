//! Builds the smp daemon's in-memory `ServiceCatalog` from the instance registry — the daemon's
//! catalog is derived state, not a yaml file (a consumer authors a `ServiceCatalog` value and
//! passes it in; nothing in the crate requires a file-backed catalog).
use std::collections::HashMap;
use std::path::Path;

use crate::catalog::{
    CommandSpec, PreparationCommand, ReadinessSpec, ServiceCatalog, ServiceCommand,
    ServiceDefinition, ServiceId, ServiceKind, ServiceOwnership, ServicePort, ServiceProfiles,
    ServiceRunProfile, StartFailurePolicy,
};

use super::registry::SharedInstance;
use super::render::{
    check_vars, instance_vars, render_command_checked, render_env, render_readiness,
};
use super::SHARED_RUNTIME_DIRECTORY_NAME;

/// First boot of a JVM or WiredTiger can outlast the supervisor's 10s default. Install itself
/// happens before `start`, so this bound is only the process becoming ready.
const SHARED_INSTANCE_READINESS_TIMEOUT_MS: u64 = 120_000;

/// Instances in `installing`/`failed` state still synthesize a definition: a failed install is
/// retried by the next attach, and the service must exist in the catalog for the supervisor to
/// start it once install succeeds.
pub fn synthesize_catalog(
    root: &Path,
    instances: &[SharedInstance],
) -> Result<ServiceCatalog, crate::shared::SharedError> {
    let mut services = Vec::new();
    for instance in instances {
        services.push(synthesize_service(root, instance)?);
    }
    Ok(ServiceCatalog {
        services,
        groups: HashMap::from([(
            "all".to_string(),
            instances.iter().map(|i| i.id()).collect::<Vec<ServiceId>>(),
        )]),
        group_tree: Vec::new(),
        compose_file: None,
        runtime_directory: Some(SHARED_RUNTIME_DIRECTORY_NAME.to_string()),
        start_failure_policy: StartFailurePolicy::StopOnFirstFailureKeepStarted,
        private_file_guard: None,
    })
}

pub fn synthesize_service(
    root: &Path,
    instance: &SharedInstance,
) -> Result<ServiceDefinition, crate::shared::SharedError> {
    let vars = instance_vars(instance, root);
    let readiness = render_readiness(&instance.recipe.readiness, &vars)?;
    let environment = match instance.recipe.env.as_ref() {
        Some(env) => {
            for value in env.values() {
                check_vars(value, &vars, "env")?;
            }
            Some(render_env(env, &vars)).filter(|e| !e.is_empty())
        }
        None => None,
    };
    let preparation_command = match instance.recipe.prepare.as_ref() {
        Some(command) => Some(PreparationCommand {
            command: render_command_checked(command, &vars, "prepare")?,
            cwd: None,
            serialization_key: None,
        }),
        None => None,
    };
    let mut ports = vec![ServicePort {
        port: instance.port,
        label: "shared".to_string(),
        requires_running: None,
    }];
    for (index, port) in instance.extra_ports.iter().enumerate() {
        let label = instance
            .recipe
            .extra_port_labels
            .get(index)
            .cloned()
            .unwrap_or_else(|| format!("port{}", index + 2));
        ports.push(ServicePort {
            port: *port,
            label,
            requires_running: None,
        });
    }
    Ok(ServiceDefinition {
        id: instance.id(),
        label: Some(format!("{}@{} (shared)", instance.name, instance.version)),
        kind: Some(ServiceKind::Infrastructure),
        ownership: Some(ServiceOwnership::Daemon),
        disabled: false,
        profiles: ServiceProfiles {
            run: ServiceRunProfile::Verified {
                command: ServiceCommand {
                    command: render_command_checked(&instance.recipe.run, &vars, "run")?,
                    // The data dir as cwd: recipe binaries write their state under {dataDir}.
                    cwd: instance.data_dir(root).display().to_string(),
                    environment,
                    container_name: None,
                    docker_stop_command: instance
                        .recipe
                        .stop
                        .as_ref()
                        .map(|spec| render_command_checked(spec, &vars, "stop"))
                        .transpose()?,
                },
                readiness,
                readiness_timeout_ms: Some(SHARED_INSTANCE_READINESS_TIMEOUT_MS),
                preparation: None,
                preparation_command,
            },
            build: None,
        },
        ports: Some(ports),
        urls: None,
        artifact: None,
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
/// `attach_args` append to the attach argv (forwarded to the recipe's provision commands);
/// `preparation` runs project-side before every attach — e.g. rendering the conf the provision
/// step will publish.
pub fn project_service_entry(
    id: ServiceId,
    instance: &str,
    exe: &Path,
    preparation: Option<crate::catalog::PreparationCommand>,
    attach_args: Vec<String>,
    urls: Option<Vec<crate::catalog::ServiceUrl>>,
) -> ServiceDefinition {
    let exe = exe.display().to_string();
    let mut attach_argv = vec![
        exe.clone(),
        "shared".to_string(),
        "attach".to_string(),
        instance.to_string(),
    ];
    attach_argv.extend(attach_args);
    ServiceDefinition {
        label: Some(format!("{instance} (shared)")),
        id,
        kind: Some(ServiceKind::Infrastructure),
        ownership: Some(ServiceOwnership::External),
        disabled: false,
        profiles: ServiceProfiles {
            run: ServiceRunProfile::Verified {
                command: ServiceCommand {
                    command: CommandSpec::Argv {
                        argv: attach_argv,
                    },
                    cwd: ".".to_string(),
                    environment: None,
                    container_name: None,
                    docker_stop_command: Some(CommandSpec::Argv {
                        argv: vec![
                            exe.clone(),
                            "shared".to_string(),
                            "detach".to_string(),
                            instance.to_string(),
                        ],
                    }),
                },
                readiness: ReadinessSpec::Command {
                    command: CommandSpec::Argv {
                        argv: vec![
                            exe,
                            "shared".to_string(),
                            "probe".to_string(),
                            instance.to_string(),
                        ],
                    },
                    cwd: None,
                },
                readiness_timeout_ms: Some(SHARED_READINESS_TIMEOUT_MS),
                preparation: None,
                preparation_command: preparation,
            },
            build: None,
        },
        ports: None,
        urls,
        artifact: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shared::registry::InstallState;
    use crate::shared::remote::{SharedArtifact, SharedRecipe};
    use std::collections::{BTreeMap, HashMap};

    fn instance(extra: Vec<u16>) -> SharedInstance {
        SharedInstance {
            name: "minio".to_string(),
            version: "1".to_string(),
            port: 43110,
            extra_ports: extra,
            install_state: InstallState::Installed,
            install_error: None,
            recipe: SharedRecipe {
                artifacts: HashMap::from([(
                    "darwin-arm64".to_string(),
                    SharedArtifact {
                        url: Some("file:///x".to_string()),
                        sha256: "abc".to_string(),
                        script: None,
                        script_args: vec![],
                    },
                )]),
                run: CommandSpec::Argv {
                    argv: vec![
                        "{installDir}/bin/minio".to_string(),
                        "--address".to_string(),
                        "127.0.0.1:{port}".to_string(),
                        "--console-address".to_string(),
                        "127.0.0.1:{port2}".to_string(),
                    ],
                },
                stop: None,
                readiness: ReadinessSpec::Http {
                    url: "http://127.0.0.1:{port}/minio/health/live".to_string(),
                },
                provision: vec![],
                deprovision: vec![],
                connection: None,
                env: Some(HashMap::from([(
                    "MINIO_ROOT_USER".to_string(),
                    "hearth".to_string(),
                )])),
                prepare: Some(CommandSpec::Argv {
                    argv: vec![
                        "{installDir}/bin/hearth-prepare".to_string(),
                        "{dataDir}".to_string(),
                        "{port}".to_string(),
                    ],
                }),
                additional_ports: 1,
                ports: Vec::new(),
                extra_port_labels: vec!["console".to_string()],
            },
            attachments: BTreeMap::new(),
        }
    }

    #[test]
    fn maps_prepare_and_the_extra_port() {
        let service = synthesize_service(Path::new("/shared"), &instance(vec![43111])).unwrap();
        let ServiceRunProfile::Verified {
            command,
            preparation_command,
            readiness,
            readiness_timeout_ms,
            ..
        } = &service.profiles.run
        else {
            panic!("shared instance must be a verified run profile")
        };
        let CommandSpec::Argv { argv } = &command.command else {
            panic!("argv run")
        };
        assert_eq!(argv[2], "127.0.0.1:43110");
        assert_eq!(argv[4], "127.0.0.1:43111");
        let PreparationCommand {
            command: prepare, ..
        } = preparation_command.as_ref().expect("prepare mapped");
        let CommandSpec::Argv { argv: prepare_argv } = prepare else {
            panic!("argv prepare")
        };
        assert_eq!(
            prepare_argv[0],
            "/shared/installs/minio/1/bin/hearth-prepare"
        );
        assert_eq!(prepare_argv[1], "/shared/instances/minio@1");
        assert!(
            matches!(readiness, ReadinessSpec::Http { url } if url == "http://127.0.0.1:43110/minio/health/live")
        );
        assert_eq!(
            *readiness_timeout_ms,
            Some(SHARED_INSTANCE_READINESS_TIMEOUT_MS)
        );
        assert_eq!(
            command
                .environment
                .as_ref()
                .unwrap()
                .get("MINIO_ROOT_USER")
                .map(String::as_str),
            Some("hearth")
        );
        let ports = service.ports.as_ref().expect("ports");
        assert_eq!(ports[0].port, 43110);
        assert_eq!(ports[1].label, "console");
        assert_eq!(ports[1].port, 43111);
    }

    #[test]
    fn rejects_port2_when_no_extra_port_was_reserved() {
        let err = synthesize_service(Path::new("/shared"), &instance(vec![])).unwrap_err();
        assert!(err.0.contains("{port2}"), "{err}");
    }
}
