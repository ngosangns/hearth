//! A thin generic engine (tcp probe / command-exec probe / path-exists probe) driving a
//! caller-supplied check list. Nothing project-specific belongs here.
use std::net::{SocketAddr, TcpStream};
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use crate::catalog::ServiceCatalog;
use crate::platform::{
    current_platform, is_supported_hearth_platform, unsupported_platform_message,
};
use crate::validate_catalog;

#[derive(Debug, Clone)]
pub struct DoctorCheckResult {
    pub name: String,
    pub ok: bool,
    pub detail: String,
}

#[derive(Debug, Clone)]
pub struct DoctorReport {
    pub ok: bool,
    pub checks: Vec<DoctorCheckResult>,
    pub unresolved_profiles: Vec<String>,
}

pub struct CommandResult {
    pub ok: bool,
    pub output: String,
}

pub trait DoctorAdapter {
    fn command(&self, command: &str, args: &[String]) -> CommandResult;
    fn path(&self, path: &str) -> bool;
    fn port(&self, port: u16) -> bool;
    fn platform(&self) -> String {
        current_platform().to_string()
    }
}

fn tcp_probe(port: u16) -> bool {
    let addr: SocketAddr = ([127, 0, 0, 1], port).into();
    TcpStream::connect_timeout(&addr, Duration::from_millis(250)).is_ok()
}

pub struct DefaultDoctorAdapter;

impl DoctorAdapter for DefaultDoctorAdapter {
    fn command(&self, command: &str, args: &[String]) -> CommandResult {
        // `command -v`-style checks must see the daemon's own PATH — the caller's bare launchd
        // PATH lacks the tool dirs `env::with_known_tool_directories` appends.
        let path = crate::env::with_known_tool_directories(
            std::env::var_os("PATH").as_deref(),
            std::env::var_os("HOME").as_deref(),
        );
        match Command::new(command).args(args).env("PATH", path).output() {
            Ok(output) => {
                let stdout = String::from_utf8_lossy(&output.stdout);
                let stderr = String::from_utf8_lossy(&output.stderr);
                CommandResult {
                    ok: output.status.success(),
                    output: format!("{stdout}\n{stderr}").trim().to_string(),
                }
            }
            Err(_) => CommandResult {
                ok: false,
                output: String::new(),
            },
        }
    }

    fn path(&self, path: &str) -> bool {
        Path::new(path).exists()
    }

    fn port(&self, port: u16) -> bool {
        tcp_probe(port)
    }
}

// `Send + Sync` (not just `Fn`): `hearth-mcp`'s server needs its whole `LocalctlOptions` — and
// therefore this type, transitively — to cross an `Arc<dyn HearthMcpClient + Send + Sync>`
// boundary. No existing caller constructs one of these closures today, so widening the bound is
// free.
pub type DoctorCheckPredicate = Box<dyn Fn(&CommandResult) -> bool + Send + Sync>;
pub type DoctorCheckDetailFormatter = Box<dyn Fn(&CommandResult) -> String + Send + Sync>;

pub struct DoctorCommandCheck {
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    pub ok: Option<DoctorCheckPredicate>,
    pub detail: Option<DoctorCheckDetailFormatter>,
}

pub struct DoctorPathCheck {
    pub name: String,
    pub path: String,
}

pub struct DoctorPortCheck {
    pub name: String,
    pub port: u16,
}

#[derive(Default)]
pub struct DoctorChecks {
    pub commands: Vec<DoctorCommandCheck>,
    pub paths: Vec<DoctorPathCheck>,
    pub ports: Vec<DoctorPortCheck>,
}

pub fn run_doctor(
    catalog: &ServiceCatalog,
    checks: &DoctorChecks,
    adapter: &dyn DoctorAdapter,
) -> DoctorReport {
    let platform = adapter.platform();
    let platform_ok = is_supported_hearth_platform(&platform);
    let mut results = vec![DoctorCheckResult {
        name: "platform".to_string(),
        ok: platform_ok,
        detail: if platform_ok {
            platform.clone()
        } else {
            unsupported_platform_message(&platform)
        },
    }];

    for check in &checks.commands {
        let result = adapter.command(&check.command, &check.args);
        let ok = check.ok.as_ref().map(|f| f(&result)).unwrap_or(result.ok);
        let detail = check
            .detail
            .as_ref()
            .map(|f| f(&result))
            .unwrap_or_else(|| check.command.clone());
        results.push(DoctorCheckResult {
            name: check.name.clone(),
            ok,
            detail,
        });
    }
    for check in &checks.paths {
        results.push(DoctorCheckResult {
            name: check.name.clone(),
            ok: adapter.path(&check.path),
            detail: check.path.clone(),
        });
    }
    for check in &checks.ports {
        results.push(DoctorCheckResult {
            name: check.name.clone(),
            ok: adapter.port(check.port),
            detail: format!("127.0.0.1:{}", check.port),
        });
    }

    let validation = validate_catalog(catalog);
    let unresolved_profiles: Vec<String> = validation
        .warnings
        .iter()
        .filter(|w| w.contains("command is unresolved"))
        .cloned()
        .collect();

    DoctorReport {
        ok: results.iter().all(|c| c.ok) && validation.errors.is_empty(),
        checks: results,
        unresolved_profiles,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::{
        ServiceDefinition, ServiceProfiles, ServiceRunProfile, StartFailurePolicy,
    };
    use std::collections::HashMap;

    struct FakeAdapter {
        command_ok: bool,
        path_ok: bool,
        port_ok: bool,
    }

    impl DoctorAdapter for FakeAdapter {
        fn command(&self, _command: &str, _args: &[String]) -> CommandResult {
            CommandResult {
                ok: self.command_ok,
                output: "output".to_string(),
            }
        }
        fn path(&self, _path: &str) -> bool {
            self.path_ok
        }
        fn port(&self, _port: u16) -> bool {
            self.port_ok
        }
        fn platform(&self) -> String {
            "darwin".to_string()
        }
    }

    fn empty_catalog() -> ServiceCatalog {
        ServiceCatalog {
            services: vec![],
            groups: HashMap::new(),
            group_tree: Vec::new(),
            compose_file: None,
            runtime_directory: None,
            start_failure_policy: StartFailurePolicy::StopOnFirstFailureKeepStarted,
            private_file_guard: None,
        }
    }

    #[test]
    fn ok_report_when_every_check_passes() {
        let adapter = FakeAdapter {
            command_ok: true,
            path_ok: true,
            port_ok: true,
        };
        let checks = DoctorChecks {
            commands: vec![DoctorCommandCheck {
                name: "docker".to_string(),
                command: "docker".to_string(),
                args: vec![],
                ok: None,
                detail: None,
            }],
            paths: vec![DoctorPathCheck {
                name: "bin".to_string(),
                path: "/usr/bin".to_string(),
            }],
            ports: vec![DoctorPortCheck {
                name: "db".to_string(),
                port: 5432,
            }],
        };
        let report = run_doctor(&empty_catalog(), &checks, &adapter);
        assert!(report.ok);
        assert_eq!(report.checks.len(), 4); // platform + 3
    }

    #[test]
    fn not_ok_when_a_check_fails() {
        let adapter = FakeAdapter {
            command_ok: false,
            path_ok: true,
            port_ok: true,
        };
        let checks = DoctorChecks {
            commands: vec![DoctorCommandCheck {
                name: "docker".to_string(),
                command: "docker".to_string(),
                args: vec![],
                ok: None,
                detail: None,
            }],
            paths: vec![],
            ports: vec![],
        };
        let report = run_doctor(&empty_catalog(), &checks, &adapter);
        assert!(!report.ok);
    }

    #[test]
    fn reports_unresolved_profiles() {
        let adapter = FakeAdapter {
            command_ok: true,
            path_ok: true,
            port_ok: true,
        };
        let catalog = ServiceCatalog {
            services: vec![ServiceDefinition {
                id: "wip".to_string(),
                label: None,
                kind: None,
                ownership: None,
                disabled: false,
                profiles: ServiceProfiles {
                    run: ServiceRunProfile::Unresolved {
                        readiness: crate::catalog::ReadinessSpec::Process,
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
            groups: HashMap::new(),
            group_tree: Vec::new(),
            compose_file: None,
            runtime_directory: None,
            start_failure_policy: StartFailurePolicy::StopOnFirstFailureKeepStarted,
            private_file_guard: None,
        };
        let report = run_doctor(&catalog, &DoctorChecks::default(), &adapter);
        assert_eq!(
            report.unresolved_profiles,
            vec!["wip:run command is unresolved".to_string()]
        );
    }
}
