//! `ProcessSupervisor` — the start/stop/restart/readiness/adoption state machine. It touches the
//! OS only through the `ProcessAdapter`/`ProbeAdapter`/`PreparationAdapter`/`Host` traits, so it
//! runs against fakes in its unit tests; `default_adapters.rs` is the production implementation.
use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use crate::catalog::{
    is_container_command, CommandSpec, ReadinessSpec, ServiceCatalog, ServiceCommand,
    ServiceDefinition, ServiceId, ServiceOwnership, ServiceRunProfile,
};
use crate::state::{
    ActualServiceState, DesiredServiceState, ProcessIdentity, ReadinessKind, ServiceLifecycleState,
    ServiceReadiness,
};

use crate::sync::KeyedLock;

use super::duplicates::{executable_matches, paths_equal};
use super::fingerprint::{normalize_command_fingerprint, normalize_observed_command_fingerprint};
use super::process_tree::{
    has_departed_members, merge_known_descendants, process_tree_alive, secondary_process_groups,
    ProcessTreeEntry, ProcessTreeSnapshot,
};
use super::types::{
    Host, Inspection, ManagedProcess, ObservedProcess, OnOutput, OutputSource, OutputTail,
    ProcessRecord, ProcessSignal, SpawnInput, StartOptions, SupervisorError, SupervisorOptions,
};

/// `Running` is the in-flight state of a `readiness: exit` command (and may appear in an older
/// `state.json`). It stays active so the row is reconciled rather than ignored.
pub const ACTIVE_STATES: [ActualServiceState; 7] = [
    ActualServiceState::QueuedStart,
    ActualServiceState::Preparing,
    ActualServiceState::Starting,
    ActualServiceState::Running,
    ActualServiceState::RunningUnready,
    ActualServiceState::Ready,
    ActualServiceState::Stopping,
];

fn is_active_state(state: ActualServiceState) -> bool {
    ACTIVE_STATES.contains(&state)
}

/// How much of an externally-owned container's existing log to replay when a tail attaches — its
/// retained history could be days old, so the follow is bounded to a useful recent window.
const EXTERNAL_LOG_BACKLOG_LINES: u64 = 200;

/// How often a live leader's tree is snapshotted. Children (air's server, a shell background job)
/// can appear after spawn; the snapshot used when the leader exits has to be from while it was
/// still alive, because the tree walk refuses a pid whose start identity is already gone.
const TREE_SAMPLE_INTERVAL: Duration = Duration::from_millis(200);

/// After a one-shot task exits 0, how long the probe may still be catching up. The task already
/// waited for the instance; the pack-sized readiness budget must not keep running after that.
const TASK_EXIT_SETTLE_MS: i64 = 5_000;

/// A "task" command has no long-lived managed process to track — it runs once to bring external
/// state into the desired shape (generalizes a tailnet-serve-config runner).
fn is_task_command(readiness: &ReadinessSpec) -> bool {
    matches!(readiness, ReadinessSpec::Tailnet)
}

/// An `ownership: external` service whose readiness is a `command` probe is also a task: its run
/// command is a one-shot "bring the external thing up" trigger (e.g. the generated
/// `hearth shared attach` for a `shared:` entry), not the service process itself. Only the
/// external+command combination is treated this way — a daemon-owned service with `command`
/// readiness still runs a real long-lived process.
fn is_external_task(definition: Option<&ServiceDefinition>, profile: &VerifiedProfile) -> bool {
    matches!(
        definition.and_then(|d| d.ownership),
        Some(ServiceOwnership::External)
    ) && matches!(profile.readiness, ReadinessSpec::Command { .. })
}

fn readiness_kind_of(readiness: &ReadinessSpec) -> ReadinessKind {
    match readiness {
        ReadinessSpec::Process => ReadinessKind::Process,
        ReadinessSpec::Tcp { .. } => ReadinessKind::Tcp,
        ReadinessSpec::Http { .. } => ReadinessKind::Http,
        ReadinessSpec::Container => ReadinessKind::Container,
        ReadinessSpec::Tailnet => ReadinessKind::Tailnet,
        ReadinessSpec::Command { .. } => ReadinessKind::Command,
        ReadinessSpec::Exit => ReadinessKind::Exit,
    }
}

fn profile_for(
    catalog: &ServiceCatalog,
    service_id: &str,
) -> Result<VerifiedProfile, SupervisorError> {
    let service = catalog.services.iter().find(|s| s.id == service_id);
    match service.map(|s| &s.profiles.run) {
        Some(ServiceRunProfile::Verified {
            command,
            readiness,
            readiness_timeout_ms,
            preparation,
            preparation_command,
        }) => Ok(VerifiedProfile {
            command: command.clone(),
            readiness: readiness.clone(),
            readiness_timeout_ms: *readiness_timeout_ms,
            preparation: preparation.clone(),
            preparation_command: preparation_command.clone(),
        }),
        _ => Err(SupervisorError(format!("Unsupported service {service_id}"))),
    }
}

fn definition_for<'a>(
    catalog: &'a ServiceCatalog,
    service_id: &str,
) -> Option<&'a ServiceDefinition> {
    catalog.services.iter().find(|s| s.id == service_id)
}

/// A resolved, owned copy of a `Verified` run profile's fields — avoids re-borrowing the catalog
/// (which the supervisor doesn't otherwise hold onto) across `await` points.
#[derive(Clone)]
pub struct VerifiedProfile {
    pub command: ServiceCommand,
    pub readiness: ReadinessSpec,
    pub readiness_timeout_ms: Option<u64>,
    pub preparation: Option<Vec<String>>,
    pub preparation_command: Option<crate::catalog::PreparationCommand>,
}

struct Active {
    command: ServiceCommand,
    identity: ProcessIdentity,
    stopped: bool,
    /// Set by the spawned process's exit watcher the moment it exits. `on_exit` waits for the
    /// per-service lock. A `readiness: exit` start holds that lock until the process exits, so it
    /// reads this flag instead of `ps`. The continuous probe loop does not hold the lock across a tick.
    exited: Arc<AtomicBool>,
    /// Written by the watcher *before* `exited` flips. The readiness wait holds the per-service
    /// queue, so it can read the code here; `on_exit` only runs once that wait releases the queue.
    exit_code: Arc<AtomicI32>,
    /// Tree last observed while the leader's start identity still matched. `on_exit` signals only
    /// secondary groups from this snapshot — the dead leader's pgid may already have been recycled.
    last_tree: Arc<Mutex<Vec<ProcessTreeEntry>>>,
}

/// Tri-state patch for an optional field: keep the previous value, clear it, or set a new one.
#[derive(Clone, Default)]
pub enum Patch<T> {
    #[default]
    Keep,
    Clear,
    Set(T),
}
impl<T: Clone> Patch<T> {
    fn apply(&self, previous: Option<T>) -> Option<T> {
        match self {
            Patch::Keep => previous,
            Patch::Clear => None,
            Patch::Set(v) => Some(v.clone()),
        }
    }
}

#[derive(Default)]
pub struct Changes {
    pub desired_state: Option<DesiredServiceState>,
    pub current_operation_id: Patch<String>,
    pub identity: Patch<ProcessIdentity>,
    pub readiness_kind: Patch<ReadinessKind>,
    pub readiness_detail: Patch<String>,
    pub exited_at: Patch<String>,
    pub exit_code: Patch<i32>,
    pub error: Patch<String>,
}

/// Chunks a service's log forwarder may have queued before new ones are dropped (and counted).
const LOG_FORWARD_CAPACITY: usize = 1024;

/// A bounded forwarding channel: a chatty service outrunning `Host::append_log` (a file write plus
/// an event publish per chunk) drops chunks rather than growing daemon memory without limit. The
/// drop count is written into the log as a marker once the forwarder catches up.
#[derive(Clone)]
struct LogForwarder {
    sender: tokio::sync::mpsc::Sender<String>,
    dropped: Arc<AtomicU64>,
}

enum ReadinessOutcome {
    Ready,
    Superseded,
    Exited { message: String },
}

/// Removes this service's continuous-probe registration when the loop task ends, including
/// when the runtime drops it. A later arm can start a new loop; an older loop must not clear
/// a newer token.
struct ProbeLoopGuard {
    supervisor: Arc<ProcessSupervisor>,
    service_id: ServiceId,
    token: u64,
}

impl Drop for ProbeLoopGuard {
    fn drop(&mut self) {
        let mut loops = self.supervisor.probe_loops.lock().unwrap();
        if loops.get(&self.service_id) == Some(&self.token) {
            loops.remove(&self.service_id);
        }
    }
}

pub struct ProcessSupervisor {
    host: Arc<dyn Host>,
    options: SupervisorOptions,
    active: Mutex<HashMap<ServiceId, Active>>,
    tokens: Mutex<HashMap<ServiceId, u64>>,
    queues: KeyedLock<ServiceId>,
    build_aborts: Mutex<HashMap<ServiceId, CancellationToken>>,
    build_serials: KeyedLock<String>,
    preparation_serials: KeyedLock<String>,
    output_tails: Mutex<HashMap<ServiceId, OutputTail>>,
    /// One ordered log-forwarding channel per service. Chunks used to be forwarded by spawning a
    /// task each, which then contended for the same per-service append lock and won it in arbitrary
    /// order — so two chunks emitted in order could land in the log file reversed, or interleaved
    /// mid-line. A single draining task per service preserves emission order by construction.
    log_forwarders: Mutex<HashMap<ServiceId, LogForwarder>>,
    /// Token of the continuous readiness loop, so a second arm for the same start (reconcile
    /// after the loop is already running) does not probe twice.
    probe_loops: Mutex<HashMap<ServiceId, u64>>,
    compose_start_tail: tokio::sync::Mutex<()>,
}

impl ProcessSupervisor {
    pub fn new(host: Arc<dyn Host>, options: SupervisorOptions) -> Arc<Self> {
        Arc::new(Self {
            host,
            options,
            active: Mutex::new(HashMap::new()),
            tokens: Mutex::new(HashMap::new()),
            queues: KeyedLock::new(),
            build_aborts: Mutex::new(HashMap::new()),
            build_serials: KeyedLock::new(),
            preparation_serials: KeyedLock::new(),
            output_tails: Mutex::new(HashMap::new()),
            log_forwarders: Mutex::new(HashMap::new()),
            probe_loops: Mutex::new(HashMap::new()),
            compose_start_tail: tokio::sync::Mutex::new(()),
        })
    }

    // ---------------------------------------------------------------------------------------
    // Public API
    // ---------------------------------------------------------------------------------------

    pub async fn start(
        self: &Arc<Self>,
        service_id: &ServiceId,
        operation_id: Option<String>,
    ) -> Result<(), SupervisorError> {
        self.start_with_options(service_id, operation_id, StartOptions::default())
            .await
    }

    /// `StartOptions::kill_unowned` is the wire-level echo of a user's explicit "yes, kill the
    /// process holding my port" — it must only ever come from a confirmed client request.
    pub async fn start_with_options(
        self: &Arc<Self>,
        service_id: &ServiceId,
        operation_id: Option<String>,
        options: StartOptions,
    ) -> Result<(), SupervisorError> {
        let sup = self.clone();
        let id = service_id.clone();
        self.queues
            .run(service_id, || async move {
                sup.start_locked(&id, operation_id, options).await
            })
            .await
    }

    pub async fn restart(
        self: &Arc<Self>,
        service_id: &ServiceId,
        operation_id: Option<String>,
    ) -> Result<(), SupervisorError> {
        self.cancel(service_id);
        self.abort_build(service_id);
        let sup = self.clone();
        let id = service_id.clone();
        let op = operation_id.clone();
        self.queues
            .run(service_id, || async move {
                // Captured before stop clears the identity. An `exec` service's live command is
                // the settled program, not the shell text, and a duplicate of that program has
                // the same fingerprint.
                let fingerprints = sup.duplicate_fingerprints(&id);
                let cwd = sup.service_cwd(&id);
                // A row with no process and no `stop` command used to fail here, so a group
                // restart of `externally-owned` services only reported the error and never
                // started them. Stop still refuses that case on its own. Restart starts it.
                // A holder that is this service is recorded first so the stop below actually
                // replaces it; a different program is not signalled.
                if sup.restart_needs_stop(&id) || sup.note_service_holder(&id).await {
                    sup.stop_locked(&id, op.clone()).await?;
                }
                sup.reap_duplicate_service_processes(&id, &fingerprints, &cwd)
                    .await?;
                sup.start_locked(&id, op, StartOptions::default()).await
            })
            .await
    }

    pub async fn stop(
        self: &Arc<Self>,
        service_id: &ServiceId,
        operation_id: Option<String>,
    ) -> Result<(), SupervisorError> {
        self.cancel(service_id);
        self.abort_build(service_id);
        let sup = self.clone();
        let id = service_id.clone();
        self.queues
            .run(service_id, || async move {
                sup.stop_locked(&id, operation_id).await
            })
            .await
    }

    pub async fn start_group(
        self: &Arc<Self>,
        group: &str,
        operation_id: Option<String>,
    ) -> Result<(), SupervisorError> {
        let members = self
            .host
            .catalog()
            .groups
            .get(group)
            .cloned()
            .ok_or_else(|| SupervisorError(format!("Unknown service group {group}")))?;
        for service_id in members {
            self.start(&service_id, operation_id.clone()).await?;
        }
        Ok(())
    }

    pub fn begin_shutdown(&self) {
        let mut ids: std::collections::HashSet<ServiceId> =
            self.tokens.lock().unwrap().keys().cloned().collect();
        ids.extend(self.build_aborts.lock().unwrap().keys().cloned());
        ids.extend(self.host.service_states().into_iter().map(|s| s.service_id));
        for id in ids {
            self.cancel(&id);
            self.abort_build(&id);
        }
    }

    pub async fn shutdown(self: &Arc<Self>) {
        self.begin_shutdown();
        let daemon_owned: std::collections::HashSet<ServiceId> = self
            .host
            .catalog()
            .services
            .iter()
            .filter(|s| !matches!(s.ownership, Some(ServiceOwnership::External)))
            .map(|s| s.id.clone())
            .collect();

        let to_stop: Vec<ServiceId> = self
            .host
            .service_states()
            .into_iter()
            .filter(|s| daemon_owned.contains(&s.service_id) && is_active_state(s.actual_state))
            .map(|s| s.service_id)
            .collect();
        for id in to_stop {
            let _ = self.stop(&id, None).await;
        }

        // A service can carry a verified POSIX identity from a prior generation while its current
        // actualState says "externally-owned" (e.g. a port conflict was detected at last start) —
        // the pass above skips it since that state isn't "active", but a lingering owned process
        // must still be reaped on shutdown so it never leaks past this manager's lifetime.
        let to_reap: Vec<ServiceId> = self
            .host
            .service_states()
            .into_iter()
            .filter(|s| daemon_owned.contains(&s.service_id))
            .map(|s| s.service_id)
            .collect();
        for id in to_reap {
            let sup = self.clone();
            let sid = id.clone();
            self.queues
                .run(&id, || async move {
                    sup.reap_persisted_posix_identity(&sid).await
                })
                .await;
        }

        self.detach_all_output();
    }

    /// Stops every log follower. Container followers are real child processes (`docker logs -f`)
    /// in their own process group — they would outlive the daemon as orphans streaming to a dead
    /// pipe. Every shutdown runs this, including the leave-services one: the next daemon attaches
    /// its own followers to the services it re-adopts.
    pub fn detach_all_output(&self) {
        let tails: Vec<OutputTail> = self
            .output_tails
            .lock()
            .unwrap()
            .drain()
            .map(|(_, tail)| tail)
            .collect();
        for tail in tails {
            tail.stop();
        }
    }

    async fn reap_persisted_posix_identity(self: &Arc<Self>, service_id: &ServiceId) {
        let Some(state) = self.state(service_id) else {
            return;
        };
        let Some(identity) = &state.identity else {
            return;
        };
        if matches!(identity, ProcessIdentity::Docker(_)) || !self.identity_matches_state(&state) {
            return;
        }
        self.terminate_persisted_posix_identity(identity).await;
    }

    async fn terminate_persisted_posix_identity(self: &Arc<Self>, identity: &ProcessIdentity) {
        // `!= Some(true)`: a probe that can't answer is not license to signal.
        if self.observed_matches(identity).await != Some(true) {
            return;
        }
        let ProcessIdentity::Posix(posix) = identity else {
            return;
        };
        let known = self.known_descendants(&posix.service_id, posix.pid);
        if let Err(error) = self
            .terminate_posix_tree(posix.pid, &posix.start_identity, posix.pgid, &known)
            .await
        {
            self.host.record_background_error(
                "supervisor.terminate",
                &format!("{}: {}", posix.service_id, error.0),
            );
        }
    }

    pub async fn status(self: &Arc<Self>, service_id: &ServiceId) -> Result<(), SupervisorError> {
        let sup = self.clone();
        let id = service_id.clone();
        self.queues
            .run(service_id, || async move {
                let Some(state) = sup.state(&id) else {
                    return Ok(());
                };
                if state.identity.is_none() || !is_active_state(state.actual_state) {
                    return Ok(());
                }
                // `== Some(false)`: an unverifiable probe must not orphan a live process.
                // Ready ↔ running-unready belongs to the continuous probe loop. Probing here
                // would race it and publish a second transition for the same tick.
                if sup.owns(&state).await == Some(false) {
                    sup.orphan(&state, None).await;
                }
                Ok(())
            })
            .await
    }

    pub async fn reconcile(self: &Arc<Self>) {
        for state in self.host.service_states() {
            // A persisted `externally-owned` row has no identity, so the loop below skips it.
            // Daemon startup only reconciles. Without this pass a dead holder leaves
            // "Port N is held by pid …" on screen forever, and a holder that is this service
            // (the program `exec` settled into) stays an error instead of being adopted.
            if state.actual_state == ActualServiceState::ExternallyOwned {
                let sup = self.clone();
                let service_id = state.service_id.clone();
                self.queues
                    .run(&service_id.clone(), || async move {
                        let Some(state) = sup.state(&service_id) else {
                            return;
                        };
                        if state.actual_state != ActualServiceState::ExternallyOwned {
                            return;
                        }
                        sup.reconcile_externally_owned(&state).await;
                    })
                    .await;
                continue;
            }
            if state.identity.is_none() || !is_active_state(state.actual_state) {
                continue;
            }
            let sup = self.clone();
            let service_id = state.service_id.clone();
            self.queues
                .run(&service_id.clone(), || async move {
                    let identity = state.identity.clone().unwrap();
                    match sup.options.process.inspect(&identity).await {
                        Inspection::Unknown => {
                            // The probe could not answer (a wedged docker/ps): decide nothing —
                            // marking a live service failed here would spawn a duplicate.
                            return;
                        }
                        Inspection::Gone
                        | Inspection::Observed(ObservedProcess { alive: false, .. }) => {
                            let next_state = if state.desired_state == DesiredServiceState::Running
                            {
                                ActualServiceState::Failed
                            } else {
                                ActualServiceState::Stopped
                            };
                            sup.transition(
                                &service_id,
                                state.generation,
                                next_state,
                                ServiceReadiness::Failed,
                                Changes {
                                    error: Patch::Set(
                                        "Managed process is no longer alive".to_string(),
                                    ),
                                    exited_at: Patch::Set(sup.options.clock.now()),
                                    ..Default::default()
                                },
                            )
                            .await;
                            return;
                        }
                        Inspection::Observed(_) => {}
                    }
                    if !sup.identity_matches_state(&state) {
                        sup.orphan(&state, None).await;
                        return;
                    }
                    match sup.observed_matches(&identity).await {
                        Some(true) => {}
                        // A definitively-mismatched identity is someone else's process — orphan
                        // it. `None` means the probe failed: skip rather than orphan a live one.
                        Some(false) => {
                            sup.orphan(&state, None).await;
                            return;
                        }
                        None => return,
                    }
                    let updated_identity = with_manager_instance(identity, sup.host.instance_id());
                    let profile = profile_for(&sup.host.catalog(), &service_id).ok();
                    // A one-time command is `running` until it exits. Reconcile must not park it in
                    // `running-unready`, which is the "not ready yet" state of a probed server.
                    let exit_job = profile
                        .as_ref()
                        .is_some_and(|profile| matches!(profile.readiness, ReadinessSpec::Exit));
                    sup.transition(
                        &service_id,
                        state.generation,
                        if exit_job {
                            ActualServiceState::Running
                        } else {
                            ActualServiceState::RunningUnready
                        },
                        if exit_job {
                            ServiceReadiness::Unknown
                        } else {
                            ServiceReadiness::NotReady
                        },
                        Changes {
                            identity: Patch::Set(updated_identity.clone()),
                            error: Patch::Clear,
                            ..Default::default()
                        },
                    )
                    .await;
                    // Guard on a live tail rather than attaching unconditionally: `reconcile` runs
                    // once at bootstrap, but a start or external sync may already have attached one
                    // — re-attaching a container tail replays its `--since` backlog into the log.
                    if !sup.has_live_output_tail(&service_id) {
                        sup.attach_output(&service_id, output_source(&updated_identity, true));
                    }
                    let Some(profile) = profile else {
                        return;
                    };
                    let token = sup.current_token(&service_id);
                    // A spawned service already has an exit watcher. An adopted one (this daemon
                    // just attached to a live pid) does not — without one, a later crash stays
                    // `ready` until the next restart.
                    sup.arm_adopted_watch(
                        &service_id,
                        updated_identity.clone(),
                        profile.command.clone(),
                        state.generation,
                        token,
                    );
                    let _ = sup
                        .readiness_with_adopted(
                            &service_id,
                            &profile,
                            state.generation,
                            Some(updated_identity),
                            token,
                            None,
                            true,
                        )
                        .await;
                })
                .await;
        }
    }

    /// Re-read an `externally-owned` row. The port check that wrote it is not re-run on daemon
    /// startup otherwise, so a holder that has already exited stays on screen, and a holder that
    /// is this service's own `exec`'d program is never adopted.
    async fn reconcile_externally_owned(self: &Arc<Self>, state: &ServiceLifecycleState) {
        let Ok(profile) = profile_for(&self.host.catalog(), &state.service_id) else {
            return;
        };
        let Some(port) = readiness_tcp_port(&profile.readiness) else {
            return;
        };
        match self.options.probes.port_in_use(port).await {
            None => {}
            Some(false) => {
                let running = state.desired_state == DesiredServiceState::Running;
                self.transition(
                    &state.service_id,
                    state.generation,
                    if running {
                        ActualServiceState::Failed
                    } else {
                        ActualServiceState::Stopped
                    },
                    if running {
                        ServiceReadiness::Failed
                    } else {
                        ServiceReadiness::Unknown
                    },
                    Changes {
                        error: if running {
                            Patch::Set("Managed process is no longer alive".to_string())
                        } else {
                            Patch::Clear
                        },
                        identity: Patch::Clear,
                        ..Default::default()
                    },
                )
                .await;
            }
            Some(true) => {
                let Some(holders) = self.options.probes.port_holders(port).await else {
                    return;
                };
                for holder in &holders {
                    if self.holder_is_service(holder, &profile).await {
                        self.adopt_port_holder(
                            &state.service_id,
                            state.generation,
                            &profile,
                            holder,
                        )
                        .await;
                        return;
                    }
                }
                if holders.is_empty() {
                    return;
                }
                let message = format!(
                    "Port {port} is held by {}",
                    self.describe_port_holders(port).await
                );
                if state.error.as_deref() != Some(message.as_str()) {
                    self.transition(
                        &state.service_id,
                        state.generation,
                        ActualServiceState::ExternallyOwned,
                        ServiceReadiness::Failed,
                        Changes {
                            error: Patch::Set(message),
                            ..Default::default()
                        },
                    )
                    .await;
                }
            }
        }
    }

    /// The listener is this service when its command is the `exec` target (same directory), or
    /// when a Gradle start script has become `java` and the install directory from
    /// `cd && exec build/install/<name>/bin/<name>` is still in that command line.
    async fn holder_is_service(
        &self,
        holder: &crate::supervisor::types::PortHolder,
        profile: &VerifiedProfile,
    ) -> bool {
        if holder.pid <= 1 {
            return false;
        }
        let fingerprints = exec_target_fingerprints(&profile.command);
        let cwd = process_cwd(&profile.command);
        if !fingerprints.is_empty() {
            if let Some(matches) = self
                .options
                .process
                .command_matches(&fingerprints, &[], &cwd)
                .await
            {
                if matches.iter().any(|matched| {
                    matched.pid == holder.pid && matched.start_identity == holder.start_identity
                }) {
                    return true;
                }
            }
        }
        install_dir_marker(&profile.command).is_some_and(|marker| holder.command.contains(&marker))
    }

    async fn adopt_port_holder(
        self: &Arc<Self>,
        service_id: &ServiceId,
        generation: u64,
        profile: &VerifiedProfile,
        holder: &crate::supervisor::types::PortHolder,
    ) {
        let stub = ProcessIdentity::Posix(crate::state::PosixProcessIdentity {
            manager_instance_id: self.host.instance_id(),
            service_id: service_id.clone(),
            generation,
            started_at: self.options.clock.now(),
            pid: holder.pid,
            pgid: holder.pgid,
            start_identity: holder.start_identity.clone(),
            command_fingerprint: String::new(),
        });
        let Inspection::Observed(ObservedProcess {
            alive: true,
            record: ProcessRecord::Posix(record),
        }) = self.options.process.inspect(&stub).await
        else {
            return;
        };
        if record.pid != holder.pid || record.start_identity != holder.start_identity {
            return;
        }
        let identity = ProcessIdentity::Posix(crate::state::PosixProcessIdentity {
            manager_instance_id: self.host.instance_id(),
            service_id: service_id.clone(),
            generation,
            started_at: self.options.clock.now(),
            pid: record.pid,
            pgid: record.pgid,
            start_identity: record.start_identity,
            command_fingerprint: record.command_fingerprint,
        });
        let token = self.current_token(service_id);
        self.transition(
            service_id,
            generation,
            ActualServiceState::RunningUnready,
            ServiceReadiness::NotReady,
            Changes {
                identity: Patch::Set(identity.clone()),
                error: Patch::Clear,
                ..Default::default()
            },
        )
        .await;
        if !self.has_live_output_tail(service_id) {
            self.attach_output(service_id, output_source(&identity, true));
        }
        self.arm_adopted_watch(
            service_id,
            identity.clone(),
            profile.command.clone(),
            generation,
            token,
        );
        let _ = self
            .readiness_with_adopted(
                service_id,
                profile,
                generation,
                Some(identity),
                token,
                None,
                true,
            )
            .await;
    }

    /// Polls readiness of every `ownership: External` service and adopts/releases it into this
    /// manager's own state machine when its external readiness appears/disappears.
    pub async fn sync_external_services(self: &Arc<Self>) {
        let services: Vec<ServiceDefinition> = self
            .host
            .catalog()
            .services
            .iter()
            .filter(|s| matches!(s.ownership, Some(ServiceOwnership::External)))
            .cloned()
            .collect();
        for service in services {
            if self.active.lock().unwrap().contains_key(&service.id) {
                continue;
            }
            let sup = self.clone();
            let service_id = service.id.clone();
            self.queues
                .run(&service.id.clone(), || async move {
                    if sup.active.lock().unwrap().contains_key(&service_id) {
                        return;
                    }
                    let state = sup.state(&service_id);
                    let ServiceRunProfile::Verified {
                        command, readiness, ..
                    } = &service.profiles.run
                    else {
                        return;
                    };
                    let ready = sup.probe(readiness, &service_id).await;
                    let actual = state.as_ref().map(|s| s.actual_state);
                    if ready
                        && matches!(
                            actual,
                            Some(ActualServiceState::Stopped)
                                | Some(ActualServiceState::Failed)
                                | None
                        )
                    {
                        let generation = state.as_ref().map(|s| s.generation).unwrap_or(0) + 1;
                        sup.transition(
                            &service_id,
                            generation,
                            ActualServiceState::Ready,
                            ServiceReadiness::Ready,
                            Changes {
                                readiness_kind: Patch::Set(readiness_kind_of(readiness)),
                                readiness_detail: Patch::Set(
                                    "adopted from external state".to_string(),
                                ),
                                desired_state: Some(DesiredServiceState::Running),
                                error: Patch::Clear,
                                exit_code: Patch::Clear,
                                exited_at: Patch::Clear,
                                identity: Patch::Clear,
                                ..Default::default()
                            },
                        )
                        .await;
                    }
                    // Attach once per live container, not once per adoption transition — a daemon
                    // restart drops every in-memory tail while adopted services stay `Ready`, so
                    // the map itself is the idempotency key, not the state edge.
                    if ready {
                        if let Some(container_name) = &command.container_name {
                            if !sup.has_live_output_tail(&service_id) {
                                sup.attach_output(
                                    &service_id,
                                    // Backlog capped: an external container may have been running
                                    // for days before this daemon ever adopted it.
                                    OutputSource::Container {
                                        container_name,
                                        since: None,
                                        tail: Some(EXTERNAL_LOG_BACKLOG_LINES),
                                    },
                                );
                            }
                        }
                    } else if let Some(state) = &state {
                        if matches!(
                            state.actual_state,
                            ActualServiceState::Ready | ActualServiceState::RunningUnready
                        ) {
                            sup.transition(
                                &service_id,
                                state.generation,
                                ActualServiceState::Stopped,
                                ServiceReadiness::Unknown,
                                Changes {
                                    desired_state: Some(DesiredServiceState::Stopped),
                                    exited_at: Patch::Set(sup.options.clock.now()),
                                    error: Patch::Clear,
                                    exit_code: Patch::Clear,
                                    identity: Patch::Clear,
                                    readiness_detail: Patch::Clear,
                                    ..Default::default()
                                },
                            )
                            .await;
                            sup.detach_output(&service_id);
                        }
                    }
                })
                .await;
        }
    }

    // ---------------------------------------------------------------------------------------
    // Locked start/stop
    // ---------------------------------------------------------------------------------------

    async fn start_locked(
        self: &Arc<Self>,
        service_id: &ServiceId,
        operation_id: Option<String>,
        options: StartOptions,
    ) -> Result<(), SupervisorError> {
        if (self.options.is_closing)() {
            return Ok(());
        }
        let profile = profile_for(&self.host.catalog(), service_id)?;
        let current = self.state(service_id);
        let existing_stopped = {
            let active = self.active.lock().unwrap();
            active
                .get(service_id)
                .map(|a| (a.stopped, a.exited.load(Ordering::SeqCst)))
        };
        if let Some((stopped, exited)) = existing_stopped {
            // `exited` is set by the watcher before it can take this queue. A start that still
            // holds the queue would otherwise report success over a process that is already dead.
            if !stopped
                && !exited
                && current.as_ref().map(|s| s.actual_state) != Some(ActualServiceState::Failed)
            {
                return Ok(());
            }
        }
        if existing_stopped.is_some() {
            self.finalize(service_id, true).await;
        }
        if let Some(current) = &current {
            if current.identity.is_some()
                && is_active_state(current.actual_state)
                && self.owns(current).await == Some(true)
            {
                return Ok(());
            }
        }
        let mut retained_identity = match &current {
            Some(state) if self.identity_matches_state(state) => match &state.identity {
                Some(ProcessIdentity::Posix(_)) => {
                    if self
                        .observed_matches(state.identity.as_ref().unwrap())
                        .await
                        == Some(true)
                    {
                        state.identity.clone()
                    } else {
                        None
                    }
                }
                _ => None,
            },
            _ => None,
        };
        // The stored identity missed (gone, or overwritten by a start that could not bind).
        // A title-rewritten copy of this binary in this directory is still the service.
        if retained_identity.is_none() {
            retained_identity = self.untracked_same_executable(service_id, &profile).await;
        }
        let generation = match &retained_identity {
            Some(ProcessIdentity::Posix(p)) => p.generation,
            _ => current.as_ref().map(|s| s.generation).unwrap_or(0) + 1,
        };
        let mut token = self.cancel(service_id);

        if let Some(retained) = retained_identity {
            let identity = with_manager_instance(retained, self.host.instance_id());
            // A live process is this service. A failing probe leaves it `running-unready` and
            // the continuous loop keeps checking; start does not kill it to force a fresh one.
            // A process that is already gone falls through and is spawned again.
            if self.owns_identity(&identity).await != Some(false) {
                let exit_job = matches!(profile.readiness, ReadinessSpec::Exit);
                self.transition(
                    service_id,
                    generation,
                    if exit_job {
                        ActualServiceState::Running
                    } else {
                        ActualServiceState::RunningUnready
                    },
                    if exit_job {
                        ServiceReadiness::Unknown
                    } else {
                        ServiceReadiness::NotReady
                    },
                    Changes {
                        desired_state: Some(DesiredServiceState::Running),
                        current_operation_id: operation_id
                            .clone()
                            .map(Patch::Set)
                            .unwrap_or(Patch::Keep),
                        identity: Patch::Set(identity.clone()),
                        error: Patch::Clear,
                        exit_code: Patch::Clear,
                        exited_at: Patch::Clear,
                        ..Default::default()
                    },
                )
                .await;
                self.attach_output(service_id, output_source(&identity, true));
                self.arm_adopted_watch(
                    service_id,
                    identity.clone(),
                    profile.command.clone(),
                    generation,
                    token,
                );
                return self
                    .readiness_with_adopted(
                        service_id,
                        &profile,
                        generation,
                        Some(identity),
                        token,
                        operation_id,
                        true,
                    )
                    .await;
            }
            token = self.cancel(service_id);
        }

        self.transition(
            service_id,
            generation,
            ActualServiceState::Preparing,
            ServiceReadiness::Unknown,
            Changes {
                desired_state: Some(DesiredServiceState::Running),
                current_operation_id: operation_id.clone().map(Patch::Set).unwrap_or(Patch::Keep),
                error: Patch::Clear,
                exit_code: Patch::Clear,
                exited_at: Patch::Clear,
                identity: Patch::Clear,
                // A fresh start: a previous run's detail ("adopted from external state", a probe
                // timeout) no longer describes this one.
                readiness_detail: Patch::Clear,
                ..Default::default()
            },
        )
        .await;

        // `artifact:` install runs first — preparation commands and builds may reference
        // `{installDir}`/`{dataDir}` contents, so the tarball has to be on disk before either.
        let definition = definition_for(&self.host.catalog(), service_id).cloned();
        if let Some(def) = definition.as_ref().filter(|def| def.artifact.is_some()) {
            let installer = self.options.artifact_installer.clone();
            match installer {
                Some(installer) => {
                    let sup = self.clone();
                    let sid = service_id.clone();
                    let on_output: OnOutput = Arc::new(move |data: &str| {
                        if sup.valid(&sid, generation, token) {
                            sup.append_output(&sid, data);
                        }
                    });
                    if let Err(error) = installer.install(def, on_output).await {
                        if !self.valid(service_id, generation, token) || (self.options.is_closing)()
                        {
                            return Ok(());
                        }
                        self.transition_if_current(
                            service_id,
                            generation,
                            token,
                            ActualServiceState::Failed,
                            ServiceReadiness::Failed,
                            Changes {
                                error: Patch::Set(format!("Install failed: {}", error.0)),
                                current_operation_id: operation_id
                                    .clone()
                                    .map(Patch::Set)
                                    .unwrap_or(Patch::Keep),
                                ..Default::default()
                            },
                        )
                        .await;
                        return Err(error);
                    }
                }
                None => {
                    self.transition_if_current(
                        service_id,
                        generation,
                        token,
                        ActualServiceState::Failed,
                        ServiceReadiness::Failed,
                        Changes {
                            error: Patch::Set(
                                "artifact installs are not supported by this host".to_string(),
                            ),
                            current_operation_id: operation_id
                                .clone()
                                .map(Patch::Set)
                                .unwrap_or(Patch::Keep),
                            ..Default::default()
                        },
                    )
                    .await;
                    return Err(SupervisorError(format!(
                        "{service_id}: artifact installs are not supported by this host"
                    )));
                }
            }
        }
        if !self.valid(service_id, generation, token) || (self.options.is_closing)() {
            return Ok(());
        }
        let mut preparation_failed = false;
        if let Some(preparation) = &profile.preparation {
            if !preparation.is_empty() {
                if let Some(adapter) = &self.options.preparation {
                    if adapter.prepare(service_id, preparation).await.is_err() {
                        preparation_failed = true;
                    }
                }
            }
        }
        // Independent of the opaque marker list above — a service may declare either, both, or
        // neither. Reuses `ProbeAdapter::command`, the same adapter `ReadinessSpec::Command`
        // readiness already uses, so a bespoke `PreparationAdapter` is no longer the only way to
        // express "run this before starting" in a catalog that has no closures (YAML).
        // `serialization_key` reuses the exact same `KeyedLock` primitive as build serialization —
        // services sharing a key never run their preparation command concurrently, for the same
        // reason a shared Gradle daemon can't take concurrent builds: some preparation work (a
        // check-then-generate shared cert/config file with no locking of its own) isn't
        // concurrency-safe either.
        if !preparation_failed {
            if let Some(prep_command) = &profile.preparation_command {
                let ok = match &prep_command.serialization_key {
                    Some(key) => {
                        self.serialized_preparation_command(
                            key,
                            &prep_command.command,
                            prep_command.cwd.as_deref(),
                        )
                        .await
                    }
                    None => {
                        self.options
                            .probes
                            .command(&prep_command.command, prep_command.cwd.as_deref())
                            .await
                    }
                };
                if ok != Some(true) {
                    preparation_failed = true;
                }
            }
        }
        if preparation_failed {
            self.transition_if_current(
                service_id,
                generation,
                token,
                ActualServiceState::Failed,
                ServiceReadiness::Failed,
                Changes {
                    error: Patch::Set("Preparation failed".to_string()),
                    current_operation_id: operation_id
                        .clone()
                        .map(Patch::Set)
                        .unwrap_or(Patch::Keep),
                    ..Default::default()
                },
            )
            .await;
            return Err(SupervisorError(format!(
                "Preparation failed for {service_id}"
            )));
        }
        if !self.valid(service_id, generation, token) || (self.options.is_closing)() {
            return Ok(());
        }
        if let Some(def) = &definition {
            if def.profiles.build.is_some() {
                self.build(service_id, def, generation, token, operation_id.clone())
                    .await?;
            }
        }
        if !self.valid(service_id, generation, token) || (self.options.is_closing)() {
            return Ok(());
        }
        if let ReadinessSpec::Tcp { port } = &profile.readiness {
            if let Some(true) = self.options.probes.port_in_use(*port).await {
                // `killUnowned` is the client's echo of "yes, kill whatever holds my port".
                // A restart already reaped processes that match this service's command and cwd.
                // A holder that does not match is signalled only through this confirmed path.
                let error = if options.kill_unowned {
                    self.reclaim_port(*port).await.err()
                } else {
                    None
                };
                match error {
                    None if !options.kill_unowned => {
                        let held_by = self.describe_port_holders(*port).await;
                        self.transition(
                            service_id,
                            generation,
                            ActualServiceState::ExternallyOwned,
                            ServiceReadiness::Failed,
                            Changes {
                                error: Patch::Set(format!("Port {port} is held by {held_by}")),
                                ..Default::default()
                            },
                        )
                        .await;
                        return Err(SupervisorError(format!("Port {port} is externally owned")));
                    }
                    None => {}
                    Some(error) => {
                        self.transition(
                            service_id,
                            generation,
                            ActualServiceState::ExternallyOwned,
                            ServiceReadiness::Failed,
                            Changes {
                                error: Patch::Set(error.0.clone()),
                                ..Default::default()
                            },
                        )
                        .await;
                        return Err(error);
                    }
                }
            }
        }
        self.spawn_and_wait(service_id, &profile, generation, token, operation_id)
            .await
    }

    /// Human-readable "pid 91600 (node dist/main)" for the refusal error and the client-side kill
    /// prompt; "an unowned process" when the probe can't resolve the holder (`port_holders`
    /// returning `None` means no adapter capability, not "nobody is there").
    async fn describe_port_holders(&self, port: u16) -> String {
        match self.options.probes.port_holders(port).await {
            Some(holders) if !holders.is_empty() => holders
                .iter()
                .map(|h| {
                    if h.command.is_empty() {
                        format!("pid {}", h.pid)
                    } else {
                        format!("pid {} ({})", h.pid, h.command)
                    }
                })
                .collect::<Vec<_>>()
                .join(", "),
            _ => "an unowned process".to_string(),
        }
    }

    /// SIGTERM each resolved port-holder, poll the port until it frees (or `termination_grace_ms`
    /// elapses), then a second pass with SIGKILL. Holders are re-resolved each pass — a squatter
    /// that died and had its pid recycled, or one replaced by a respawning wrapper, is never
    /// signalled on stale information (`signal_pid` re-verifies the lstart before sending).
    ///
    /// Only the resolved holder pids are signalled — never `killpg`: an unowned process's group
    /// membership is untrusted (a shared job can hold innocent sibling services; pgid 1 must never
    /// be signalled). A socket-holding child that outlives its signalled parent is caught by the
    /// next pass's re-resolution.
    async fn reclaim_port(self: &Arc<Self>, port: u16) -> Result<(), SupervisorError> {
        for signal in [ProcessSignal::Sigterm, ProcessSignal::Sigkill] {
            if let Some(holders) = self.options.probes.port_holders(port).await {
                for holder in holders {
                    self.options
                        .process
                        .signal_pid(holder.pid, &holder.start_identity, signal)
                        .await;
                }
            }
            let deadline = self.options.clock.now_millis() + self.options.termination_grace_ms;
            loop {
                if self.options.probes.port_in_use(port).await == Some(false) {
                    return Ok(());
                }
                if self.options.clock.now_millis() >= deadline {
                    break;
                }
                self.options
                    .clock
                    .sleep(self.options.readiness_backoff_ms)
                    .await;
            }
        }
        Err(SupervisorError(format!(
            "Port {port} is still held by {} after SIGKILL",
            self.describe_port_holders(port).await
        )))
    }

    fn duplicate_fingerprints(&self, service_id: &ServiceId) -> Vec<String> {
        let Ok(profile) = profile_for(&self.host.catalog(), service_id) else {
            return Vec::new();
        };
        let mut fingerprints = vec![normalize_command_fingerprint(&profile.command)];
        if let Some(identity) = self.state(service_id).and_then(|state| state.identity) {
            let extra = identity.command_fingerprint().to_string();
            if !extra.is_empty() && !fingerprints.iter().any(|fingerprint| fingerprint == &extra) {
                fingerprints.push(extra);
            }
        }
        fingerprints
    }

    fn service_cwd(&self, service_id: &ServiceId) -> String {
        profile_for(&self.host.catalog(), service_id)
            .map(|profile| profile.command.cwd)
            .unwrap_or_else(|_| ".".to_string())
    }

    /// Absolute argv0 of an argv command. A shell command and a bare name (`node`) are not
    /// usable as a title-rewrite identity: the settled fingerprint covers `shell` + `exec`,
    /// and a shared interpreter would pull in every other script in the directory.
    fn absolute_argv0(command: &ServiceCommand) -> Option<String> {
        match &command.command {
            CommandSpec::Argv { argv } => {
                let exe = argv.first()?;
                exe.starts_with('/').then(|| exe.clone())
            }
            CommandSpec::Shell { .. } => None,
        }
    }

    /// The service executable, when no other catalog service runs that same binary from the
    /// same directory. Redis rewrites `redis-server {dataDir}/redis.conf` into
    /// `redis-server 127.0.0.1:port`; the binary and the cwd are still the service.
    fn duplicate_executables(&self, service_id: &ServiceId) -> Vec<String> {
        let Ok(profile) = profile_for(&self.host.catalog(), service_id) else {
            return Vec::new();
        };
        let Some(exe) = Self::absolute_argv0(&profile.command) else {
            return Vec::new();
        };
        let catalog = self.host.catalog();
        let cwd = profile.command.cwd;
        let shared = catalog.services.iter().any(|other| {
            &other.id != service_id
                && profile_for(&catalog, &other.id).is_ok_and(|other_profile| {
                    Self::absolute_argv0(&other_profile.command).is_some_and(|other_exe| {
                        paths_equal(Path::new(&other_exe), Path::new(&exe))
                    }) && paths_equal(Path::new(&other_profile.command.cwd), Path::new(&cwd))
                })
        });
        if shared {
            Vec::new()
        } else {
            vec![exe]
        }
    }

    /// A live process of this service whose identity was lost (a title rewrite made the stored
    /// fingerprint miss, then a failed start overwrote it). Adopting it is what keeps a second
    /// copy from dying on `bind: Address already in use`. `None` when there is nothing to adopt
    /// or the table could not be read — a wedged `ps` must not fail an ordinary start.
    async fn untracked_same_executable(
        self: &Arc<Self>,
        service_id: &ServiceId,
        profile: &VerifiedProfile,
    ) -> Option<ProcessIdentity> {
        let executables = self.duplicate_executables(service_id);
        if executables.is_empty() {
            return None;
        }
        let matches = self
            .options
            .process
            .command_matches(&[], &executables, &profile.command.cwd)
            .await?;
        let matched = matches.into_iter().find(|matched| matched.pid > 1)?;
        let generation = self
            .state(service_id)
            .map(|state| state.generation)
            .unwrap_or(0)
            + 1;
        let stub = ProcessIdentity::Posix(crate::state::PosixProcessIdentity {
            manager_instance_id: self.host.instance_id(),
            service_id: service_id.clone(),
            generation,
            started_at: self.options.clock.now(),
            pid: matched.pid,
            pgid: matched.pid,
            start_identity: matched.start_identity.clone(),
            command_fingerprint: String::new(),
        });
        let Inspection::Observed(ObservedProcess {
            alive: true,
            record: ProcessRecord::Posix(record),
        }) = self.options.process.inspect(&stub).await
        else {
            return None;
        };
        if record.pid != matched.pid || record.start_identity != matched.start_identity {
            return None;
        }
        if !executable_matches(&record.command_line, &executables) {
            return None;
        }
        Some(ProcessIdentity::Posix(crate::state::PosixProcessIdentity {
            manager_instance_id: self.host.instance_id(),
            service_id: service_id.clone(),
            generation,
            started_at: self.options.clock.now(),
            pid: record.pid,
            pgid: record.pgid,
            start_identity: record.start_identity,
            command_fingerprint: record.command_fingerprint,
        }))
    }

    /// After the owned process is stopped: kill every other host process that is this same
    /// service (command fingerprint or the same absolute executable, plus cwd), including
    /// children that moved into their own process group. Each pid is signalled on its own.
    /// A shared pgid is not a group we own.
    async fn reap_duplicate_service_processes(
        self: &Arc<Self>,
        service_id: &ServiceId,
        fingerprints: &[String],
        cwd: &str,
    ) -> Result<(), SupervisorError> {
        let executables = self.duplicate_executables(service_id);
        if fingerprints.is_empty() && executables.is_empty() {
            return Ok(());
        }
        let Some(matches) = self
            .options
            .process
            .command_matches(fingerprints, &executables, cwd)
            .await
        else {
            return Err(SupervisorError(format!(
                "{service_id}: could not list processes; refusing to start beside an unchecked duplicate"
            )));
        };
        let mut entries = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for matched in matches {
            if matched.pid <= 1 {
                continue;
            }
            match self
                .process_tree(matched.pid, &matched.start_identity)
                .await
            {
                ProcessTreeSnapshot::Unknown => {
                    return Err(SupervisorError(format!(
                        "{service_id}: the process tree could not be read; refusing to start beside an unchecked duplicate"
                    )));
                }
                ProcessTreeSnapshot::Absent => {}
                ProcessTreeSnapshot::Present(tree) => {
                    for entry in tree {
                        if seen.insert(entry.pid) {
                            entries.push(entry);
                        }
                    }
                }
            }
        }
        if entries.is_empty() {
            return Ok(());
        }
        self.signal_entries(&entries, ProcessSignal::Sigterm).await;
        let deadline = self.options.clock.now_millis() + self.options.termination_grace_ms;
        loop {
            if self.options.clock.now_millis() >= deadline {
                break;
            }
            match self.process_tree_alive(&entries).await {
                Some(false) => return Ok(()),
                Some(true) => {
                    self.options
                        .clock
                        .sleep(self.options.readiness_backoff_ms)
                        .await
                }
                None => {
                    return Err(SupervisorError(format!(
                        "{service_id}: a duplicate process's liveness could not be verified"
                    )));
                }
            }
        }
        match self.process_tree_alive(&entries).await {
            Some(false) => Ok(()),
            None => Err(SupervisorError(format!(
                "{service_id}: a duplicate process's liveness could not be verified"
            ))),
            Some(true) => {
                self.signal_entries(&entries, ProcessSignal::Sigkill).await;
                match self.process_tree_alive(&entries).await {
                    Some(false) => Ok(()),
                    Some(true) => {
                        let pids: Vec<String> =
                            entries.iter().map(|entry| entry.pid.to_string()).collect();
                        Err(SupervisorError(format!(
                            "{service_id} still has a duplicate process (pid {})",
                            pids.join(", ")
                        )))
                    }
                    None => Err(SupervisorError(format!(
                        "{service_id}: a duplicate process's liveness could not be verified"
                    ))),
                }
            }
        }
    }

    async fn signal_entries(&self, entries: &[ProcessTreeEntry], signal: ProcessSignal) {
        for entry in entries {
            if entry.pid <= 1 {
                continue;
            }
            self.options
                .process
                .signal_pid(entry.pid, &entry.start_identity, signal)
                .await;
        }
    }

    /// True when restart has something to stop: an in-flight start, a recorded process, or a
    /// catalog `stop` command. An `externally-owned` or failed row with none of those is not a
    /// stop — restart goes on to start.
    fn restart_needs_stop(&self, service_id: &ServiceId) -> bool {
        let Some(state) = self.state(service_id) else {
            return false;
        };
        if state.actual_state == ActualServiceState::Stopped {
            return false;
        }
        if matches!(
            state.actual_state,
            ActualServiceState::QueuedStart
                | ActualServiceState::Preparing
                | ActualServiceState::Starting
        ) {
            return true;
        }
        if state.identity.is_some() {
            return true;
        }
        profile_for(&self.host.catalog(), service_id)
            .is_ok_and(|profile| profile.command.docker_stop_command.is_some())
    }

    /// The process listening on this service's port is the service itself (`exec` target in its
    /// directory, or the install directory in a Java command line). Record that identity so the
    /// following stop replaces it. A different program, or a port that is free, records nothing
    /// and is not signalled.
    async fn note_service_holder(&self, service_id: &ServiceId) -> bool {
        let Ok(profile) = profile_for(&self.host.catalog(), service_id) else {
            return false;
        };
        let Some(state) = self.state(service_id) else {
            return false;
        };
        if state.identity.is_some() {
            return false;
        }
        let Some(port) = readiness_tcp_port(&profile.readiness) else {
            return false;
        };
        if self.options.probes.port_in_use(port).await != Some(true) {
            return false;
        }
        let Some(holders) = self.options.probes.port_holders(port).await else {
            return false;
        };
        let mut chosen = None;
        for holder in holders {
            if self.holder_is_service(&holder, &profile).await {
                chosen = Some(holder);
                break;
            }
        }
        let Some(holder) = chosen else {
            return false;
        };
        let stub = ProcessIdentity::Posix(crate::state::PosixProcessIdentity {
            manager_instance_id: self.host.instance_id(),
            service_id: service_id.clone(),
            generation: state.generation,
            started_at: self.options.clock.now(),
            pid: holder.pid,
            pgid: holder.pgid,
            start_identity: holder.start_identity.clone(),
            command_fingerprint: String::new(),
        });
        let Inspection::Observed(ObservedProcess {
            alive: true,
            record: ProcessRecord::Posix(record),
        }) = self.options.process.inspect(&stub).await
        else {
            return false;
        };
        if record.pid != holder.pid || record.start_identity != holder.start_identity {
            return false;
        }
        let identity = ProcessIdentity::Posix(crate::state::PosixProcessIdentity {
            manager_instance_id: self.host.instance_id(),
            service_id: service_id.clone(),
            generation: state.generation,
            started_at: self.options.clock.now(),
            pid: record.pid,
            pgid: record.pgid,
            start_identity: record.start_identity,
            command_fingerprint: record.command_fingerprint,
        });
        // `Stopped` makes `stop_locked` return before it signals anything. The row has to leave
        // that state or a restart would adopt the listener and report success without replacing it.
        self.transition(
            service_id,
            state.generation,
            ActualServiceState::RunningUnready,
            ServiceReadiness::NotReady,
            Changes {
                identity: Patch::Set(identity),
                error: Patch::Clear,
                ..Default::default()
            },
        )
        .await;
        true
    }

    async fn stop_locked(
        self: &Arc<Self>,
        service_id: &ServiceId,
        operation_id: Option<String>,
    ) -> Result<(), SupervisorError> {
        let Some(state) = self.state(service_id) else {
            return Ok(());
        };
        if state.actual_state == ActualServiceState::Stopped {
            return Ok(());
        }
        // A finished `readiness: exit` command has nothing to signal. Stop succeeds and leaves
        // `succeeded` / `failed` as they are, so a group stop does not fail on a build that
        // already finished and does not wipe its exit code. In-flight and orphaned rows still
        // fall through — an orphaned process may still be alive.
        match self.exit_command_is_idle(service_id, &state).await {
            Ok(true) => return Ok(()),
            Ok(false) => {}
            Err(error) => return Err(error),
        }
        let has_active = self.active.lock().unwrap().contains_key(service_id);
        if state.identity.is_none() {
            // Only a start still in flight has nothing to stop yet. This used to also cover every
            // identity-less service while the daemon was closing — recording a `Ready` tailnet task
            // as stopped without running anything.
            if matches!(
                state.actual_state,
                ActualServiceState::QueuedStart
                    | ActualServiceState::Preparing
                    | ActualServiceState::Starting
            ) {
                self.transition(
                    service_id,
                    state.generation,
                    ActualServiceState::Stopped,
                    ServiceReadiness::Unknown,
                    Changes {
                        desired_state: Some(DesiredServiceState::Stopped),
                        current_operation_id: operation_id.map(Patch::Set).unwrap_or(Patch::Keep),
                        exited_at: Patch::Set(self.options.clock.now()),
                        error: Patch::Clear,
                        exit_code: Patch::Clear,
                        ..Default::default()
                    },
                )
                .await;
                return Ok(());
            }
            // Everything else reaching here is a service this manager holds no process for: an
            // adopted `ownership: external` unit, or a service whose port is held by a process it
            // does not own. `orphan()` used to be the fallthrough — recording "Process ownership
            // identity no longer matches" for a service that never had an identity — and
            // `externally-owned` returned success before even getting here, so Stop reported
            // success while nothing was stopped.
            return self.stop_unowned(service_id, &state, operation_id).await;
        }
        let owned = self.owns(&state).await;
        if owned != Some(true) {
            if owned.is_none() {
                // The probe could not answer — concluding "dead" would record `stopped` over a
                // possibly-still-running process, and concluding "not ours" would orphan a live
                // one. Fail the stop instead.
                return Err(SupervisorError(format!(
                    "{service_id} cannot be stopped: the process's liveness could not be verified"
                )));
            }
            let inspection = self
                .options
                .process
                .inspect(state.identity.as_ref().unwrap())
                .await;
            match inspection {
                Inspection::Unknown => {
                    return Err(SupervisorError(format!("{service_id} cannot be stopped: the process's liveness could not be verified")));
                }
                Inspection::Observed(ObservedProcess { alive: true, .. }) => {}
                Inspection::Gone | Inspection::Observed(_) => {
                    // The leader is already gone, so `terminate` will not walk its tree. Children
                    // that forked into their own group are still in the last snapshot taken while
                    // the leader was alive.
                    let (tree, leader_pgid) = {
                        let mut active = self.active.lock().unwrap();
                        let entry = active.remove(service_id);
                        let tree = entry
                            .as_ref()
                            .map(|active| active.last_tree.lock().unwrap().clone())
                            .unwrap_or_default();
                        let leader_pgid =
                            entry.as_ref().and_then(|active| match &active.identity {
                                ProcessIdentity::Posix(posix) => Some(posix.pgid),
                                ProcessIdentity::Docker(_) => None,
                            });
                        (tree, leader_pgid)
                    };
                    if let Some(pgid) = leader_pgid {
                        self.reap_secondary_groups(&tree, pgid).await;
                    }
                    self.transition(
                        service_id,
                        state.generation,
                        ActualServiceState::Stopped,
                        ServiceReadiness::Unknown,
                        Changes {
                            desired_state: Some(DesiredServiceState::Stopped),
                            exited_at: Patch::Set(self.options.clock.now()),
                            error: Patch::Clear,
                            exit_code: Patch::Clear,
                            ..Default::default()
                        },
                    )
                    .await;
                    return Ok(());
                }
            }
            // The process at this identity is alive but is no longer this manager's — a reused pid,
            // or a program that replaced it. Killing it would be killing someone else's process, so
            // the refusal is the answer: recording the mismatch and reporting success is the same
            // silent no-op the identity-less path above was fixed for.
            self.orphan(&state, operation_id).await;
            let described = match state.identity.as_ref().unwrap() {
                ProcessIdentity::Posix(posix) => format!("pid {}", posix.pid),
                ProcessIdentity::Docker(docker) => format!("container {}", docker.container_name),
            };
            return Err(SupervisorError(format!("{service_id} cannot be stopped: the process at {described} is no longer owned by this manager")));
        }
        self.transition(
            service_id,
            state.generation,
            ActualServiceState::Stopping,
            state.readiness,
            Changes {
                desired_state: Some(DesiredServiceState::Stopped),
                current_operation_id: operation_id.clone().map(Patch::Set).unwrap_or(Patch::Keep),
                ..Default::default()
            },
        )
        .await;
        if has_active {
            if let Some(active) = self.active.lock().unwrap().get_mut(service_id) {
                active.stopped = true;
            }
        }
        let profile = profile_for(&self.host.catalog(), service_id)?;
        self.terminate(state.identity.as_ref().unwrap(), &profile.command)
            .await?;
        if has_active {
            self.active.lock().unwrap().remove(service_id);
        }
        self.transition(
            service_id,
            state.generation,
            ActualServiceState::Stopped,
            ServiceReadiness::Unknown,
            Changes {
                desired_state: Some(DesiredServiceState::Stopped),
                exited_at: Patch::Set(self.options.clock.now()),
                error: Patch::Clear,
                exit_code: Patch::Clear,
                ..Default::default()
            },
        )
        .await;
        Ok(())
    }

    /// Stops a service this manager holds no process identity for — an adopted `ownership: external`
    /// unit (a docker/tailnet task it only observes), or a service whose port is held by a process it
    /// does not own. The catalog's `stop:` command is the only lever that exists for those, so it is
    /// what runs; with no such command, refusing loudly is the honest answer. Reporting success
    /// without stopping anything is what made Stop look broken in the GUI.
    async fn stop_unowned(
        self: &Arc<Self>,
        service_id: &ServiceId,
        state: &ServiceLifecycleState,
        operation_id: Option<String>,
    ) -> Result<(), SupervisorError> {
        let profile = profile_for(&self.host.catalog(), service_id)?;
        if profile.command.docker_stop_command.is_none() {
            let reason = match state.actual_state {
                ActualServiceState::ExternallyOwned => state.error.clone().unwrap_or_else(|| {
                    "its port is held by a process this manager does not own".to_string()
                }),
                _ => "it is owned externally".to_string(),
            };
            return Err(SupervisorError(format!("{service_id} cannot be stopped: {reason}, and its catalog declares no `stop` command")));
        }
        let sink: OnOutput = Arc::new(|_| {});
        match self
            .options
            .process
            .stop_container(&profile.command, sink)
            .await
        {
            Some(result) => result?,
            None => {
                return Err(SupervisorError(format!(
                "{service_id} cannot be stopped: this process adapter has no stop command support"
            )))
            }
        }
        // An adopted container carries a `docker logs --follow` tail; the container is gone now, so
        // the follower must not be left streaming into a dead pipe.
        self.detach_output(service_id);
        self.transition(
            service_id,
            state.generation,
            ActualServiceState::Stopped,
            ServiceReadiness::Unknown,
            Changes {
                desired_state: Some(DesiredServiceState::Stopped),
                current_operation_id: operation_id.map(Patch::Set).unwrap_or(Patch::Keep),
                exited_at: Patch::Set(self.options.clock.now()),
                error: Patch::Clear,
                exit_code: Patch::Clear,
                ..Default::default()
            },
        )
        .await;
        Ok(())
    }

    async fn spawn_and_wait(
        self: &Arc<Self>,
        service_id: &ServiceId,
        profile: &VerifiedProfile,
        generation: u64,
        token: u64,
        operation_id: Option<String>,
    ) -> Result<(), SupervisorError> {
        self.transition(
            service_id,
            generation,
            ActualServiceState::Starting,
            ServiceReadiness::NotReady,
            Changes {
                desired_state: Some(DesiredServiceState::Running),
                current_operation_id: operation_id.clone().map(Patch::Set).unwrap_or(Patch::Keep),
                identity: Patch::Clear,
                ..Default::default()
            },
        )
        .await;
        if !self.valid(service_id, generation, token) || (self.options.is_closing)() {
            return Ok(());
        }
        let fingerprint = normalize_command_fingerprint(&profile.command);
        let sup = self.clone();
        let sid = service_id.clone();
        let on_output: OnOutput = Arc::new(move |data: &str| {
            if sup.valid(&sid, generation, token) {
                sup.append_output(&sid, data);
            }
        });
        let input = SpawnInput {
            command: profile.command.clone(),
            command_fingerprint: fingerprint,
            service_id: service_id.clone(),
        };
        let is_container = is_container_command(&profile.command);
        let spawn_result = if is_container {
            let _guard = self.compose_start_tail.lock().await;
            self.options.process.spawn(input, on_output).await
        } else {
            self.options.process.spawn(input, on_output).await
        };
        let app: ManagedProcess = match spawn_result {
            Ok(app) => app,
            Err(error) => {
                if !self.valid(service_id, generation, token) || (self.options.is_closing)() {
                    return Ok(());
                }
                self.transition_if_current(
                    service_id,
                    generation,
                    token,
                    ActualServiceState::Failed,
                    ServiceReadiness::Failed,
                    Changes {
                        error: Patch::Set(error.0.clone()),
                        current_operation_id: operation_id.map(Patch::Set).unwrap_or(Patch::Keep),
                        identity: Patch::Clear,
                        ..Default::default()
                    },
                )
                .await;
                return Err(error);
            }
        };

        let catalog = self.host.catalog();
        let definition = definition_for(&catalog, service_id);
        if (is_task_command(&profile.readiness) || is_external_task(definition, profile))
            && !app.record.is_docker()
        {
            let ProcessRecord::Posix(record) = &app.record else {
                return Err(SupervisorError(format!(
                    "{service_id} task did not spawn a process"
                )));
            };
            // Not stored on the service: the run command is the one-shot trigger, not the service
            // process. It is kept only so a failed readiness can still stop this process.
            let task_identity = ProcessIdentity::Posix(crate::state::PosixProcessIdentity {
                manager_instance_id: self.host.instance_id(),
                service_id: service_id.clone(),
                generation,
                started_at: self.options.clock.now(),
                pid: record.pid,
                pgid: record.pgid,
                start_identity: record.start_identity.clone(),
                command_fingerprint: record.command_fingerprint.clone(),
            });
            self.transition(
                service_id,
                generation,
                ActualServiceState::RunningUnready,
                ServiceReadiness::NotReady,
                Changes {
                    identity: Patch::Clear,
                    ..Default::default()
                },
            )
            .await;
            let result = self
                .await_external_task(
                    service_id,
                    profile,
                    generation,
                    token,
                    operation_id.clone(),
                    app.exited,
                )
                .await;
            if result.is_err() || !self.valid(service_id, generation, token) {
                if let Err(error) = self.terminate(&task_identity, &profile.command).await {
                    self.report_terminate_failure(
                        service_id,
                        Err(SupervisorError(error.0.clone())),
                    );
                    if let Err(ready) = result {
                        return Err(SupervisorError(format!(
                            "{}; the task process could not be stopped: {}",
                            ready.0, error.0
                        )));
                    }
                    return Err(error);
                }
            }
            return result;
        }

        let identity = match &app.record {
            ProcessRecord::Docker(record) => {
                ProcessIdentity::Docker(crate::state::DockerContainerIdentity {
                    manager_instance_id: self.host.instance_id(),
                    service_id: service_id.clone(),
                    generation,
                    started_at: self.options.clock.now(),
                    container_name: record.container_name.clone(),
                    container_id: record.container_id.clone(),
                    container_started_at: record.container_started_at.clone(),
                    command_fingerprint: record.command_fingerprint.clone(),
                })
            }
            ProcessRecord::Posix(record) => {
                ProcessIdentity::Posix(crate::state::PosixProcessIdentity {
                    manager_instance_id: self.host.instance_id(),
                    service_id: service_id.clone(),
                    generation,
                    started_at: self.options.clock.now(),
                    pid: record.pid,
                    pgid: record.pgid,
                    start_identity: record.start_identity.clone(),
                    command_fingerprint: record.command_fingerprint.clone(),
                })
            }
        };
        if !self.valid(service_id, generation, token) {
            self.report_terminate_failure(
                service_id,
                self.terminate(&identity, &profile.command).await,
            );
            return Ok(());
        }
        self.track_spawned(
            service_id,
            profile.command.clone(),
            identity.clone(),
            generation,
            token,
            app.exited,
        );
        self.attach_output(service_id, output_source(&identity, false));
        let exit_job = matches!(profile.readiness, ReadinessSpec::Exit);
        self.transition(
            service_id,
            generation,
            if exit_job {
                ActualServiceState::Running
            } else {
                ActualServiceState::RunningUnready
            },
            if exit_job {
                ServiceReadiness::Unknown
            } else {
                ServiceReadiness::NotReady
            },
            Changes {
                identity: Patch::Set(identity),
                ..Default::default()
            },
        )
        .await;
        self.readiness(
            service_id,
            profile,
            generation,
            self.state(service_id).and_then(|s| s.identity),
            token,
            operation_id,
        )
        .await
    }

    /// Readiness for a one-shot task (`hearth shared attach`). The long readiness budget applies
    /// only while that process is still running — an artifact pack can use all of it. Once the
    /// process exits, a non-zero code fails the service immediately, and exit 0 gets a short
    /// settle for the probe. Leaving the loop to poll until the pack budget is what made a
    /// failed shared restart look stuck.
    async fn await_external_task(
        self: &Arc<Self>,
        service_id: &ServiceId,
        profile: &VerifiedProfile,
        generation: u64,
        token: u64,
        operation_id: Option<String>,
        mut exited: tokio::sync::oneshot::Receiver<i32>,
    ) -> Result<(), SupervisorError> {
        let readiness =
            self.await_task_probe(service_id, profile, generation, token, operation_id.clone());
        tokio::pin!(readiness);
        tokio::select! {
            biased;
            result = &mut readiness => result,
            code = &mut exited => {
                let code = code.unwrap_or(-1);
                if code != 0 {
                    let message = format!("{service_id} task exited with code {code}");
                    self.fail(
                        service_id,
                        generation,
                        token,
                        None,
                        &message,
                        operation_id,
                        None,
                    )
                    .await;
                    return Err(SupervisorError(message));
                }
                self.probe_settled_task(service_id, profile, generation, token, operation_id)
                    .await
            }
        }
    }

    /// Deadline for a one-shot external task (`hearth shared attach`) while that process is
    /// still running. A daemon-owned service does not use this: its probe loop has no deadline.
    /// The task's start is the operation, so a probe that never passes has to end the operation
    /// or `--wait` would hang for as long as the trigger process stays up.
    async fn await_task_probe(
        self: &Arc<Self>,
        service_id: &ServiceId,
        profile: &VerifiedProfile,
        generation: u64,
        token: u64,
        operation_id: Option<String>,
    ) -> Result<(), SupervisorError> {
        let timeout = profile
            .readiness_timeout_ms
            .map(|v| v as i64)
            .unwrap_or(self.options.readiness_timeout_ms);
        let deadline = self.options.clock.now_millis() + timeout;
        loop {
            if self.options.clock.now_millis() > deadline {
                break;
            }
            if !self.valid(service_id, generation, token) || (self.options.is_closing)() {
                return Ok(());
            }
            if self.probe(&profile.readiness, service_id).await {
                self.transition_if_current(
                    service_id,
                    generation,
                    token,
                    ActualServiceState::Ready,
                    ServiceReadiness::Ready,
                    Changes {
                        readiness_kind: Patch::Set(readiness_kind_of(&profile.readiness)),
                        readiness_detail: Patch::Set("readiness verified".to_string()),
                        error: Patch::Clear,
                        ..Default::default()
                    },
                )
                .await;
                return Ok(());
            }
            self.options
                .clock
                .sleep(self.options.readiness_backoff_ms)
                .await;
        }
        let message = "Readiness timed out".to_string();
        let detail = format!(
            "Readiness {} probe timed out after {}ms",
            readiness_kind_of(&profile.readiness).as_wire_str(),
            timeout
        );
        self.fail(
            service_id,
            generation,
            token,
            None,
            &message,
            operation_id,
            Some((readiness_kind_of(&profile.readiness), detail)),
        )
        .await;
        Err(SupervisorError(message))
    }

    async fn probe_settled_task(
        self: &Arc<Self>,
        service_id: &ServiceId,
        profile: &VerifiedProfile,
        generation: u64,
        token: u64,
        operation_id: Option<String>,
    ) -> Result<(), SupervisorError> {
        let budget = TASK_EXIT_SETTLE_MS.min(self.readiness_timeout_ms(service_id));
        let deadline = self.options.clock.now_millis() + budget.max(0);
        loop {
            if !self.valid(service_id, generation, token) || (self.options.is_closing)() {
                return Ok(());
            }
            if self.probe(&profile.readiness, service_id).await {
                self.transition_if_current(
                    service_id,
                    generation,
                    token,
                    ActualServiceState::Ready,
                    ServiceReadiness::Ready,
                    Changes {
                        readiness_kind: Patch::Set(readiness_kind_of(&profile.readiness)),
                        readiness_detail: Patch::Set("readiness verified".to_string()),
                        error: Patch::Clear,
                        ..Default::default()
                    },
                )
                .await;
                return Ok(());
            }
            if self.options.clock.now_millis() >= deadline {
                break;
            }
            self.options
                .clock
                .sleep(self.options.readiness_backoff_ms)
                .await;
        }
        let message = format!("{service_id} task finished but the service is not ready");
        self.fail(
            service_id,
            generation,
            token,
            None,
            &message,
            operation_id,
            None,
        )
        .await;
        Err(SupervisorError(message))
    }

    async fn readiness(
        self: &Arc<Self>,
        service_id: &ServiceId,
        profile: &VerifiedProfile,
        generation: u64,
        identity: Option<ProcessIdentity>,
        token: u64,
        operation_id: Option<String>,
    ) -> Result<(), SupervisorError> {
        self.readiness_with_adopted(
            service_id,
            profile,
            generation,
            identity,
            token,
            operation_id,
            false,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)] // the full per-start context; bundling it would only move the list
    async fn readiness_with_adopted(
        self: &Arc<Self>,
        service_id: &ServiceId,
        profile: &VerifiedProfile,
        generation: u64,
        identity: Option<ProcessIdentity>,
        token: u64,
        operation_id: Option<String>,
        adopted: bool,
    ) -> Result<(), SupervisorError> {
        if matches!(profile.readiness, ReadinessSpec::Exit) {
            let outcome = self
                .await_exit(
                    service_id,
                    profile,
                    generation,
                    identity.clone(),
                    token,
                    adopted,
                )
                .await;
            return match outcome {
                ReadinessOutcome::Ready | ReadinessOutcome::Superseded => Ok(()),
                ReadinessOutcome::Exited { message } => {
                    // `await_exit` already recorded `failed` for a command whose exit it observed.
                    // `fail` again would terminate a process that is already gone and drop the code.
                    let already_settled = self.state(service_id).is_some_and(|s| {
                        matches!(
                            s.actual_state,
                            ActualServiceState::Failed
                                | ActualServiceState::Succeeded
                                | ActualServiceState::Stopped
                        )
                    });
                    if !already_settled {
                        self.fail(
                            service_id,
                            generation,
                            token,
                            identity,
                            &message,
                            operation_id,
                            None,
                        )
                        .await;
                    }
                    Err(SupervisorError(message))
                }
            };
        }
        if matches!(profile.readiness, ReadinessSpec::Process) {
            return self
                .settle_process_liveness(service_id, generation, identity, token, operation_id)
                .await;
        }
        self.begin_continuous_probe(
            service_id,
            profile,
            generation,
            identity,
            token,
            operation_id,
            adopted,
        )
        .await
    }

    /// `readiness: process` has no probe. The start is done once the process is alive.
    async fn settle_process_liveness(
        self: &Arc<Self>,
        service_id: &ServiceId,
        generation: u64,
        identity: Option<ProcessIdentity>,
        token: u64,
        operation_id: Option<String>,
    ) -> Result<(), SupervisorError> {
        if !self.valid(service_id, generation, token) || (self.options.is_closing)() {
            return Ok(());
        }
        if let Some(identity) = &identity {
            if self.owns_identity(identity).await == Some(false) {
                if !self.valid(service_id, generation, token) {
                    return Ok(());
                }
                let message = "Process exited before liveness check".to_string();
                self.fail(
                    service_id,
                    generation,
                    token,
                    Some(identity.clone()),
                    &message,
                    operation_id,
                    None,
                )
                .await;
                return Err(SupervisorError(message));
            }
        }
        self.transition_if_current(
            service_id,
            generation,
            token,
            ActualServiceState::RunningUnready,
            ServiceReadiness::NotReady,
            Changes {
                identity: identity.map(Patch::Set).unwrap_or(Patch::Keep),
                readiness_kind: Patch::Set(ReadinessKind::Process),
                readiness_detail: Patch::Set("process-liveness-only".to_string()),
                ..Default::default()
            },
        )
        .await;
        Ok(())
    }

    /// One probe now, then a background loop at `readiness_backoff_ms`. The start returns as soon
    /// as the process is alive: waiting for the first passing probe had no deadline and would
    /// hang `--wait`. A failing probe stays `running-unready` and does not fail or kill the service.
    /// `readinessTimeoutMs` is not a service deadline here. It still bounds one `command` probe.
    #[allow(clippy::too_many_arguments)] // the full per-start context; bundling it would only move the list
    async fn begin_continuous_probe(
        self: &Arc<Self>,
        service_id: &ServiceId,
        profile: &VerifiedProfile,
        generation: u64,
        identity: Option<ProcessIdentity>,
        token: u64,
        operation_id: Option<String>,
        adopted: bool,
    ) -> Result<(), SupervisorError> {
        if !self.valid(service_id, generation, token) || (self.options.is_closing)() {
            return Ok(());
        }
        if let Some(identity) = &identity {
            if !self.still_running(service_id, identity, adopted).await {
                if !self.valid(service_id, generation, token) {
                    return Ok(());
                }
                let message = "Process exited before readiness".to_string();
                self.fail(
                    service_id,
                    generation,
                    token,
                    Some(identity.clone()),
                    &message,
                    operation_id,
                    None,
                )
                .await;
                return Err(SupervisorError(message));
            }
        }
        let passed = self.probe(&profile.readiness, service_id).await;
        if !self.valid(service_id, generation, token) || (self.options.is_closing)() {
            return Ok(());
        }
        if let Some(identity) = &identity {
            if !self.still_running(service_id, identity, adopted).await {
                if !self.valid(service_id, generation, token) {
                    return Ok(());
                }
                let message = "Process exited before readiness".to_string();
                self.fail(
                    service_id,
                    generation,
                    token,
                    Some(identity.clone()),
                    &message,
                    operation_id,
                    None,
                )
                .await;
                return Err(SupervisorError(message));
            }
        }
        let kind = readiness_kind_of(&profile.readiness);
        if passed {
            self.transition_if_current(
                service_id,
                generation,
                token,
                ActualServiceState::Ready,
                ServiceReadiness::Ready,
                Changes {
                    readiness_kind: Patch::Set(kind),
                    readiness_detail: Patch::Set(if adopted {
                        "adopted readiness verified".to_string()
                    } else {
                        "readiness verified".to_string()
                    }),
                    error: Patch::Clear,
                    ..Default::default()
                },
            )
            .await;
        } else {
            self.transition_if_current(
                service_id,
                generation,
                token,
                ActualServiceState::RunningUnready,
                ServiceReadiness::NotReady,
                Changes {
                    readiness_kind: Patch::Set(kind),
                    readiness_detail: Patch::Set(
                        "Readiness probe is currently unavailable".to_string(),
                    ),
                    ..Default::default()
                },
            )
            .await;
        }
        self.spawn_continuous_probe(service_id, profile, generation, identity, token, adopted);
        Ok(())
    }

    /// Sleeps one interval before the first tick. The caller already probed once, and this task
    /// must not take the per-service queue while that caller still holds it.
    #[allow(clippy::too_many_arguments)] // the full per-start context; bundling it would only move the list
    fn spawn_continuous_probe(
        self: &Arc<Self>,
        service_id: &ServiceId,
        profile: &VerifiedProfile,
        generation: u64,
        identity: Option<ProcessIdentity>,
        token: u64,
        adopted: bool,
    ) {
        if matches!(
            profile.readiness,
            ReadinessSpec::Process | ReadinessSpec::Exit
        ) {
            return;
        }
        {
            let mut loops = self.probe_loops.lock().unwrap();
            if loops.get(service_id) == Some(&token) {
                return;
            }
            loops.insert(service_id.clone(), token);
        }
        let sup = self.clone();
        let service_id = service_id.clone();
        let readiness = profile.readiness.clone();
        tokio::spawn(async move {
            let _guard = ProbeLoopGuard {
                supervisor: sup.clone(),
                service_id: service_id.clone(),
                token,
            };
            loop {
                sup.options
                    .clock
                    .sleep(sup.options.readiness_backoff_ms)
                    .await;
                if sup.continuous_probe_stopped(&service_id, generation, token) {
                    return;
                }
                if let Some(identity) = &identity {
                    if !sup.still_running(&service_id, identity, adopted).await {
                        return;
                    }
                }
                let passed = sup.probe(&readiness, &service_id).await;
                let sup_tick = sup.clone();
                let sid = service_id.clone();
                let identity_tick = identity.clone();
                let readiness_tick = readiness.clone();
                let stop = sup
                    .queues
                    .run(&service_id, || async move {
                        if sup_tick.continuous_probe_stopped(&sid, generation, token) {
                            return true;
                        }
                        if let Some(identity) = &identity_tick {
                            if !sup_tick.still_running(&sid, identity, adopted).await {
                                return true;
                            }
                        }
                        let Some(state) = sup_tick.state(&sid) else {
                            return true;
                        };
                        if !matches!(
                            state.actual_state,
                            ActualServiceState::Ready | ActualServiceState::RunningUnready
                        ) {
                            return true;
                        }
                        let kind = readiness_kind_of(&readiness_tick);
                        if passed && state.actual_state != ActualServiceState::Ready {
                            sup_tick
                                .transition(
                                    &sid,
                                    generation,
                                    ActualServiceState::Ready,
                                    ServiceReadiness::Ready,
                                    Changes {
                                        readiness_kind: Patch::Set(kind),
                                        readiness_detail: Patch::Set(
                                            "readiness verified".to_string(),
                                        ),
                                        error: Patch::Clear,
                                        ..Default::default()
                                    },
                                )
                                .await;
                        } else if !passed
                            && state.actual_state != ActualServiceState::RunningUnready
                        {
                            sup_tick
                                .transition(
                                    &sid,
                                    generation,
                                    ActualServiceState::RunningUnready,
                                    ServiceReadiness::NotReady,
                                    Changes {
                                        readiness_kind: Patch::Set(kind),
                                        readiness_detail: Patch::Set(
                                            "Readiness probe is currently unavailable".to_string(),
                                        ),
                                        ..Default::default()
                                    },
                                )
                                .await;
                        }
                        false
                    })
                    .await;
                if stop {
                    return;
                }
            }
        });
    }

    fn continuous_probe_stopped(
        &self,
        service_id: &ServiceId,
        generation: u64,
        token: u64,
    ) -> bool {
        !self.valid(service_id, generation, token) || (self.options.is_closing)()
    }

    /// Wait until a `readiness: exit` command's process exits. The command has no ready state and
    /// no readiness deadline: it stays `running` until the process exits or the start is cancelled.
    /// `readinessTimeoutMs` is a probe give-up and does not apply. Exit 0 is `succeeded` with
    /// desired `stopped` (reconcile must not run it again). Any other code is `failed`, also with
    /// desired `stopped`.
    ///
    /// An adopted process (daemon restarted while the command was still running) has no watcher,
    /// so its exit code is unknown: when it disappears the result is `failed`, never `succeeded`.
    async fn await_exit(
        self: &Arc<Self>,
        service_id: &ServiceId,
        _profile: &VerifiedProfile,
        generation: u64,
        identity: Option<ProcessIdentity>,
        token: u64,
        adopted: bool,
    ) -> ReadinessOutcome {
        if !self.valid(service_id, generation, token) {
            return ReadinessOutcome::Superseded;
        }
        self.transition_if_current(
            service_id,
            generation,
            token,
            ActualServiceState::Running,
            ServiceReadiness::Unknown,
            Changes {
                identity: identity.clone().map(Patch::Set).unwrap_or(Patch::Keep),
                readiness_kind: Patch::Set(ReadinessKind::Exit),
                readiness_detail: Patch::Clear,
                ..Default::default()
            },
        )
        .await;
        loop {
            if !self.valid(service_id, generation, token) {
                return ReadinessOutcome::Superseded;
            }
            if !adopted {
                if let Some(code) = self.observed_exit_code(service_id, generation) {
                    return self.finish_exit(service_id, generation, token, code).await;
                }
            } else if let Some(identity) = &identity {
                if self.owns_identity(identity).await == Some(false) {
                    let message =
                        "Command exited while the daemon was down; exit code unknown".to_string();
                    self.transition_if_current(
                        service_id,
                        generation,
                        token,
                        ActualServiceState::Failed,
                        ServiceReadiness::Failed,
                        Changes {
                            desired_state: Some(DesiredServiceState::Stopped),
                            error: Patch::Set(message.clone()),
                            identity: Patch::Clear,
                            exited_at: Patch::Set(self.options.clock.now()),
                            readiness_kind: Patch::Set(ReadinessKind::Exit),
                            readiness_detail: Patch::Set(message.clone()),
                            ..Default::default()
                        },
                    )
                    .await;
                    return ReadinessOutcome::Exited { message };
                }
            }
            self.options
                .clock
                .sleep(self.options.readiness_backoff_ms)
                .await;
        }
    }

    fn observed_exit_code(&self, service_id: &ServiceId, generation: u64) -> Option<i32> {
        let active = self.active.lock().unwrap();
        let active = active.get(service_id)?;
        if active.identity.generation() != generation || !active.exited.load(Ordering::SeqCst) {
            return None;
        }
        Some(active.exit_code.load(Ordering::SeqCst))
    }

    async fn finish_exit(
        self: &Arc<Self>,
        service_id: &ServiceId,
        generation: u64,
        token: u64,
        code: i32,
    ) -> ReadinessOutcome {
        if !self.valid(service_id, generation, token) {
            return ReadinessOutcome::Superseded;
        }
        let (actual, readiness, message) = if code == 0 {
            (
                ActualServiceState::Succeeded,
                ServiceReadiness::Unknown,
                "exited 0".to_string(),
            )
        } else {
            (
                ActualServiceState::Failed,
                ServiceReadiness::Failed,
                format!("Process exited with code {code}"),
            )
        };
        self.transition_if_current(
            service_id,
            generation,
            token,
            actual,
            readiness,
            Changes {
                desired_state: Some(DesiredServiceState::Stopped),
                exit_code: Patch::Set(code),
                exited_at: Patch::Set(self.options.clock.now()),
                error: if code == 0 {
                    Patch::Clear
                } else {
                    Patch::Set(message.clone())
                },
                identity: Patch::Clear,
                readiness_kind: Patch::Set(ReadinessKind::Exit),
                readiness_detail: Patch::Set(message.clone()),
                ..Default::default()
            },
        )
        .await;
        if self.state(service_id).map(|s| s.actual_state) != Some(actual) {
            return ReadinessOutcome::Superseded;
        }
        // Drop the active entry before releasing the queue so `on_exit` does not overwrite this
        // settlement with `failed`.
        {
            let mut active = self.active.lock().unwrap();
            if active
                .get(service_id)
                .is_some_and(|entry| entry.identity.generation() == generation)
            {
                active.remove(service_id);
            }
        }
        self.detach_output(service_id);
        if code == 0 {
            ReadinessOutcome::Ready
        } else {
            ReadinessOutcome::Exited { message }
        }
    }

    /// `true` when a `readiness: exit` command has already settled, so stop has no process to
    /// signal and must not report a failure or clear the recorded exit code.
    /// `Ok(true)` when a `readiness: exit` command has already settled and nothing of ours is
    /// still alive, so stop succeeds without signalling and without clearing the recorded exit
    /// code. `Ok(false)` falls through to a real stop. `Err` is an unverifiable probe — stop must
    /// not report success over a process that may still be running (a timed-out exit job whose
    /// terminate failed).
    async fn exit_command_is_idle(
        &self,
        service_id: &ServiceId,
        state: &ServiceLifecycleState,
    ) -> Result<bool, SupervisorError> {
        let Ok(profile) = profile_for(&self.host.catalog(), service_id) else {
            return Ok(false);
        };
        if !matches!(profile.readiness, ReadinessSpec::Exit) {
            return Ok(false);
        }
        if !matches!(
            state.actual_state,
            ActualServiceState::Succeeded | ActualServiceState::Failed
        ) {
            return Ok(false);
        }
        let Some(identity) = &state.identity else {
            return Ok(true);
        };
        match self.owns_identity(identity).await {
            Some(true) => Ok(false),
            None => Err(SupervisorError(format!(
                "{service_id} cannot be stopped: the process's liveness could not be verified"
            ))),
            Some(false) => match self.options.process.inspect(identity).await {
                // Alive, but not ours. Fall through so stop refuses instead of reporting success.
                Inspection::Observed(ObservedProcess { alive: true, .. }) => Ok(false),
                Inspection::Unknown => Err(SupervisorError(format!(
                    "{service_id} cannot be stopped: the process's liveness could not be verified"
                ))),
                Inspection::Gone | Inspection::Observed(_) => Ok(true),
            },
        }
    }

    #[allow(clippy::too_many_arguments)] // the full per-start context; bundling it would only move the list
    async fn fail(
        self: &Arc<Self>,
        service_id: &ServiceId,
        generation: u64,
        token: u64,
        identity: Option<ProcessIdentity>,
        error: &str,
        operation_id: Option<String>,
        readiness_change: Option<(ReadinessKind, String)>,
    ) {
        if !self.valid(service_id, generation, token) {
            return;
        }
        let exit_command = profile_for(&self.host.catalog(), service_id)
            .ok()
            .is_some_and(|profile| matches!(profile.readiness, ReadinessSpec::Exit));
        self.transition_if_current(
            service_id,
            generation,
            token,
            ActualServiceState::Failed,
            ServiceReadiness::Failed,
            Changes {
                desired_state: if exit_command {
                    Some(DesiredServiceState::Stopped)
                } else {
                    None
                },
                identity: identity.clone().map(Patch::Set).unwrap_or(Patch::Keep),
                error: Patch::Set(error.to_string()),
                current_operation_id: operation_id.map(Patch::Set).unwrap_or(Patch::Keep),
                readiness_kind: readiness_change
                    .as_ref()
                    .map(|(k, _)| Patch::Set(*k))
                    .unwrap_or(Patch::Keep),
                readiness_detail: readiness_change
                    .map(|(_, d)| Patch::Set(d))
                    .unwrap_or(Patch::Keep),
                ..Default::default()
            },
        )
        .await;
        self.cancel(service_id);
        let active_generation = self
            .active
            .lock()
            .unwrap()
            .get(service_id)
            .map(|a| a.identity.generation());
        if active_generation == Some(generation) {
            self.finalize(service_id, true).await;
        }
    }

    async fn on_exit(
        self: &Arc<Self>,
        service_id: &ServiceId,
        generation: u64,
        token: u64,
        code: i32,
    ) {
        let sup = self.clone();
        let sid = service_id.clone();
        self.queues
            .run(&service_id.clone(), || async move {
                if !sup.valid(&sid, generation, token) {
                    return;
                }
                let Some(state) = sup.state(&sid) else { return };
                let active_matches = {
                    sup.active
                        .lock()
                        .unwrap()
                        .get(&sid)
                        .map(|a| !a.stopped)
                        .unwrap_or(false)
                };
                if state.generation != generation || !active_matches {
                    return;
                }
                // `finish_exit` already settled a `readiness: exit` command. A late `on_exit`
                // must not replace `succeeded` with `failed`.
                // Already settled: a late watcher must not replace `succeeded` or a timeout's
                // `failed` with a second exit error. The active entry is still dropped so a later
                // start is not stuck behind it.
                if matches!(
                    state.actual_state,
                    ActualServiceState::Succeeded
                        | ActualServiceState::Stopped
                        | ActualServiceState::Stopping
                        | ActualServiceState::Failed
                ) {
                    let mut active = sup.active.lock().unwrap();
                    if active
                        .get(&sid)
                        .is_some_and(|a| a.identity.generation() == generation)
                    {
                        active.remove(&sid);
                    }
                    return;
                }
                sup.transition_if_current(
                    &sid,
                    generation,
                    token,
                    ActualServiceState::Failed,
                    ServiceReadiness::Failed,
                    Changes {
                        exit_code: Patch::Set(code),
                        exited_at: Patch::Set(sup.options.clock.now()),
                        error: Patch::Set(format!("Process exited with code {code}")),
                        ..Default::default()
                    },
                )
                .await;
                let mut active = sup.active.lock().unwrap();
                if active
                    .get(&sid)
                    .is_some_and(|a| a.identity.generation() == generation)
                {
                    active.remove(&sid);
                }
            })
            .await;
    }

    async fn finalize(self: &Arc<Self>, service_id: &ServiceId, terminate: bool) {
        let active_entry = self.active.lock().unwrap().remove(service_id);
        if let Some(active) = active_entry {
            // `unwrap_or(true)`: when liveness can't be verified `terminate` runs anyway and
            // reports its own failure, rather than skipping the stop over a wedged probe.
            if terminate && self.owns_identity(&active.identity).await.unwrap_or(true) {
                self.report_terminate_failure(
                    active.identity.service_id(),
                    self.terminate(&active.identity, &active.command).await,
                );
            }
        }
    }

    /// A container stop that FAILS must propagate: `stop_locked` transitions the service to
    /// `stopped` immediately after this returns, so swallowing the error here would record a
    /// service as stopped while its container is still running — the exact state drift this whole
    /// daemon exists to prevent. An adapter that has no `stop_container` at all is likewise an
    /// error (matching the TS source's `Docker container stop is unavailable`), not a silent no-op.
    async fn terminate(
        self: &Arc<Self>,
        identity: &ProcessIdentity,
        command: &ServiceCommand,
    ) -> Result<(), SupervisorError> {
        match self.owns_identity(identity).await {
            Some(true) => {}
            Some(false) => return Ok(()),
            // Returning `Ok` here would record `stopped` over a process that may still be
            // running — the one outcome the whole stop path exists to prevent.
            None => {
                return Err(SupervisorError(
                    "the process's liveness could not be verified; refusing to report it stopped"
                        .to_string(),
                ))
            }
        }
        match identity {
            ProcessIdentity::Docker(docker) => {
                let sink: OnOutput = Arc::new(|_| {});
                let result = match self.options.process.stop_container(command, sink).await {
                    Some(result) => result,
                    None => Err(SupervisorError(
                        "Docker container stop is unavailable".to_string(),
                    )),
                };
                // Stop the `docker logs` follower only after the stop command has run — its last
                // chunk carries the container's own shutdown output.
                self.detach_output(&docker.service_id);
                result?;
            }
            ProcessIdentity::Posix(posix) => {
                self.detach_output(&posix.service_id);
                let known = self.known_descendants(&posix.service_id, posix.pid);
                self.terminate_posix_tree(posix.pid, &posix.start_identity, posix.pgid, &known)
                    .await?;
            }
        }
        Ok(())
    }

    /// The three cleanup call sites of `terminate` have no caller to return an error to (they run
    /// while abandoning a superseded start, or while finalizing). They still must not drop it
    /// silently — this codebase's rule is that fire-and-forget work reports through
    /// `Host::record_background_error` rather than vanishing.
    fn report_terminate_failure(
        &self,
        service_id: &ServiceId,
        result: Result<(), SupervisorError>,
    ) {
        if let Err(error) = result {
            self.host.record_background_error(
                "supervisor.terminate",
                &format!("{service_id}: {}", error.0),
            );
        }
    }

    async fn process_tree(
        &self,
        leader_pid: i64,
        leader_start_identity: &str,
    ) -> ProcessTreeSnapshot {
        self.options
            .process
            .process_tree(leader_pid, leader_start_identity)
            .await
    }

    /// `None` when the live table could not be read. An empty snapshot is dead (`Some(false)`).
    async fn process_tree_alive(&self, tree: &[ProcessTreeEntry]) -> Option<bool> {
        if tree.is_empty() {
            return Some(false);
        }
        let alive = self.options.process.live_start_identities().await?;
        Some(process_tree_alive(tree, &alive))
    }

    /// Signals the leader's group, then every other process group in the snapshot — the `air`
    /// case, where the real server runs in its own group and survives a leader-only signal.
    /// Nothing is signalled when the snapshot is missing or the fresh table cannot be read, and a
    /// group is signalled only while one of its snapshotted members still shows the same start
    /// identity, so a recycled pgid is never signalled.
    async fn signal_process_tree(
        &self,
        snapshot: &ProcessTreeSnapshot,
        leader_pgid: i64,
        signal: ProcessSignal,
    ) {
        let ProcessTreeSnapshot::Present(tree) = snapshot else {
            return;
        };
        self.signal_verified_members(tree, leader_pgid, signal, true)
            .await;
    }

    async fn signal_verified_members(
        &self,
        tree: &[ProcessTreeEntry],
        leader_pgid: i64,
        signal: ProcessSignal,
        include_leader: bool,
    ) {
        let Some(alive) = self.options.process.live_start_identities().await else {
            return;
        };
        if include_leader {
            let leader_live = tree.iter().any(|member| {
                member.pgid == leader_pgid && alive.get(&member.pid) == Some(&member.start_identity)
            });
            if leader_live {
                self.options.process.signal_group(leader_pgid, signal).await;
            }
        }
        for (pgid, members) in secondary_process_groups(tree, leader_pgid) {
            if members
                .iter()
                .any(|member| alive.get(&member.pid) == Some(&member.start_identity))
            {
                self.options.process.signal_group(pgid, signal).await;
            }
        }
    }

    /// SIGTERM, wait, SIGKILL — but only from a snapshot that still names our leader. `Unknown`
    /// fails the stop. `Absent` (leader gone or pid reused) signals nothing and succeeds.
    async fn terminate_posix_tree(
        &self,
        pid: i64,
        start_identity: &str,
        pgid: i64,
        known: &[ProcessTreeEntry],
    ) -> Result<(), SupervisorError> {
        let snapshot = self.process_tree(pid, start_identity).await;
        let tree = match &snapshot {
            ProcessTreeSnapshot::Unknown => {
                return Err(SupervisorError(
                    "the process tree could not be read; refusing to report it stopped".to_string(),
                ));
            }
            ProcessTreeSnapshot::Absent => return Ok(()),
            ProcessTreeSnapshot::Present(tree) => {
                // Descendants the sampler saw that are no longer reachable from the leader (a
                // `setsid` double fork). Signalling verifies each one's start identity first.
                let mut tree = tree.clone();
                for entry in known {
                    if !tree
                        .iter()
                        .any(|t| t.pid == entry.pid && t.start_identity == entry.start_identity)
                    {
                        tree.push(entry.clone());
                    }
                }
                tree
            }
        };
        let snapshot = ProcessTreeSnapshot::Present(tree.clone());
        self.signal_process_tree(&snapshot, pgid, ProcessSignal::Sigterm)
            .await;
        let deadline = self.options.clock.now_millis() + self.options.termination_grace_ms;
        loop {
            if self.options.clock.now_millis() >= deadline {
                break;
            }
            match self.process_tree_alive(&tree).await {
                Some(false) => return Ok(()),
                Some(true) => {
                    self.options
                        .clock
                        .sleep(self.options.readiness_backoff_ms)
                        .await
                }
                None => return Err(SupervisorError(
                    "the process's liveness could not be verified; refusing to report it stopped"
                        .to_string(),
                )),
            }
        }
        match self.process_tree_alive(&tree).await {
            Some(false) => Ok(()),
            None => Err(SupervisorError(
                "the process's liveness could not be verified; refusing to report it stopped"
                    .to_string(),
            )),
            Some(true) => {
                self.signal_process_tree(
                    &ProcessTreeSnapshot::Present(tree),
                    pgid,
                    ProcessSignal::Sigkill,
                )
                .await;
                Ok(())
            }
        }
    }

    /// After the leader has exited: SIGTERM, grace, SIGKILL whatever of its last tree (sampled
    /// while it was alive) is still running — children in their own groups, and children it left
    /// behind in its own group (`sleep 1000 & exit 1`: the row is `failed` while `sleep` runs on,
    /// tracked by nobody). A group is only signalled while one of its snapshotted members still
    /// shows the same start identity, so the dead leader's pgid is never signalled once the group
    /// is empty, and never after it could have been recycled.
    async fn reap_secondary_groups(&self, tree: &[ProcessTreeEntry], leader_pgid: i64) {
        // The dead leader stays in the list: its start identity no longer shows, so it neither
        // counts as alive nor vouches for its group.
        let survivors = tree;
        if survivors.is_empty() {
            return;
        }
        if self.process_tree_alive(survivors).await != Some(true) {
            return;
        }
        self.signal_verified_members(survivors, leader_pgid, ProcessSignal::Sigterm, true)
            .await;
        let deadline = self.options.clock.now_millis() + self.options.termination_grace_ms;
        loop {
            if self.options.clock.now_millis() >= deadline {
                break;
            }
            match self.process_tree_alive(survivors).await {
                Some(true) => {
                    self.options
                        .clock
                        .sleep(self.options.readiness_backoff_ms)
                        .await
                }
                Some(false) | None => return,
            }
        }
        if self.process_tree_alive(survivors).await == Some(true) {
            self.signal_verified_members(survivors, leader_pgid, ProcessSignal::Sigkill, true)
                .await;
        }
    }

    /// Folds one sampler snapshot into the running tree — see `merge_known_descendants`. The
    /// liveness `ps` only runs when a member has left the snapshot.
    async fn record_tree_sample(
        &self,
        last_tree: &Arc<Mutex<Vec<ProcessTreeEntry>>>,
        fresh: Vec<ProcessTreeEntry>,
    ) {
        let previous = last_tree.lock().unwrap().clone();
        let merged = if has_departed_members(&previous, &fresh) {
            let alive = self.options.process.live_start_identities().await;
            merge_known_descendants(&previous, fresh, alive.as_ref())
        } else {
            fresh
        };
        *last_tree.lock().unwrap() = merged;
    }

    /// The sampled tree of the active process at `pid`, for a stop to signal alongside a fresh
    /// snapshot.
    fn known_descendants(&self, service_id: &ServiceId, pid: i64) -> Vec<ProcessTreeEntry> {
        let active = self.active.lock().unwrap();
        match active.get(service_id) {
            Some(entry) if matches!(&entry.identity, ProcessIdentity::Posix(posix) if posix.pid == pid) => {
                entry.last_tree.lock().unwrap().clone()
            }
            _ => Vec::new(),
        }
    }

    fn insert_active(
        &self,
        service_id: &ServiceId,
        command: ServiceCommand,
        identity: ProcessIdentity,
    ) -> (
        Arc<AtomicBool>,
        Arc<AtomicI32>,
        Arc<Mutex<Vec<ProcessTreeEntry>>>,
    ) {
        let exited = Arc::new(AtomicBool::new(false));
        let exit_code = Arc::new(AtomicI32::new(0));
        let last_tree = Arc::new(Mutex::new(Vec::new()));
        self.active.lock().unwrap().insert(
            service_id.clone(),
            Active {
                command,
                identity,
                stopped: false,
                exited: exited.clone(),
                exit_code: exit_code.clone(),
                last_tree: last_tree.clone(),
            },
        );
        (exited, exit_code, last_tree)
    }

    fn spawn_tree_sampler(
        self: &Arc<Self>,
        service_id: ServiceId,
        identity: ProcessIdentity,
        last_tree: Arc<Mutex<Vec<ProcessTreeEntry>>>,
        exited: Arc<AtomicBool>,
    ) {
        let ProcessIdentity::Posix(posix) = &identity else {
            return;
        };
        let pid = posix.pid;
        let start_identity = posix.start_identity.clone();
        let sup = self.clone();
        tokio::spawn(async move {
            loop {
                if exited.load(Ordering::SeqCst) {
                    return;
                }
                let still = {
                    let active = sup.active.lock().unwrap();
                    active.get(&service_id).is_some_and(|entry| {
                        !entry.stopped && Arc::ptr_eq(&entry.last_tree, &last_tree)
                    })
                };
                if !still {
                    return;
                }
                if let ProcessTreeSnapshot::Present(tree) =
                    sup.options.process.process_tree(pid, &start_identity).await
                {
                    sup.record_tree_sample(&last_tree, tree).await;
                }
                tokio::time::sleep(TREE_SAMPLE_INTERVAL).await;
            }
        });
    }

    fn track_spawned(
        self: &Arc<Self>,
        service_id: &ServiceId,
        command: ServiceCommand,
        identity: ProcessIdentity,
        generation: u64,
        token: u64,
        exited_rx: tokio::sync::oneshot::Receiver<i32>,
    ) {
        let (exited_flag, exit_code, last_tree) =
            self.insert_active(service_id, command, identity.clone());
        self.spawn_tree_sampler(
            service_id.clone(),
            identity.clone(),
            last_tree.clone(),
            exited_flag.clone(),
        );
        let sup = self.clone();
        let sid = service_id.clone();
        tokio::spawn(async move {
            let code = exited_rx.await.unwrap_or(-1);
            let tree = last_tree.lock().unwrap().clone();
            let leader_pgid = match &identity {
                ProcessIdentity::Posix(posix) => Some(posix.pgid),
                ProcessIdentity::Docker(_) => None,
            };
            exit_code.store(code, Ordering::SeqCst);
            exited_flag.store(true, Ordering::SeqCst);
            if let Some(pgid) = leader_pgid {
                sup.reap_secondary_groups(&tree, pgid).await;
            }
            sup.on_exit(&sid, generation, token, code).await;
        });
    }

    /// Adopted processes have no child handle. Poll `inspect` until the process is gone (`Unknown`
    /// keeps waiting) and sample the tree while it is alive so exit can reap secondary groups.
    fn arm_adopted_watch(
        self: &Arc<Self>,
        service_id: &ServiceId,
        identity: ProcessIdentity,
        command: ServiceCommand,
        generation: u64,
        token: u64,
    ) {
        if self.active.lock().unwrap().contains_key(service_id) {
            return;
        }
        let (exited, exit_code, last_tree) =
            self.insert_active(service_id, command, identity.clone());
        let sup = self.clone();
        let sid = service_id.clone();
        tokio::spawn(async move {
            loop {
                if exited.load(Ordering::SeqCst) {
                    return;
                }
                let still = {
                    let active = sup.active.lock().unwrap();
                    active.get(&sid).is_some_and(|entry| {
                        !entry.stopped && Arc::ptr_eq(&entry.last_tree, &last_tree)
                    })
                };
                if !still {
                    return;
                }
                match sup.options.process.inspect(&identity).await {
                    Inspection::Unknown => {}
                    Inspection::Gone
                    | Inspection::Observed(ObservedProcess { alive: false, .. }) => {
                        let tree = last_tree.lock().unwrap().clone();
                        let leader_pgid = match &identity {
                            ProcessIdentity::Posix(posix) => Some(posix.pgid),
                            ProcessIdentity::Docker(_) => None,
                        };
                        exit_code.store(-1, Ordering::SeqCst);
                        exited.store(true, Ordering::SeqCst);
                        if let Some(pgid) = leader_pgid {
                            sup.reap_secondary_groups(&tree, pgid).await;
                        }
                        sup.on_exit(&sid, generation, token, -1).await;
                        return;
                    }
                    Inspection::Observed(_) => {
                        if let ProcessIdentity::Posix(posix) = &identity {
                            if let ProcessTreeSnapshot::Present(tree) = sup
                                .options
                                .process
                                .process_tree(posix.pid, &posix.start_identity)
                                .await
                            {
                                sup.record_tree_sample(&last_tree, tree).await;
                            }
                        }
                    }
                }
                tokio::time::sleep(TREE_SAMPLE_INTERVAL).await;
            }
        });
    }

    async fn build(
        self: &Arc<Self>,
        service_id: &ServiceId,
        definition: &ServiceDefinition,
        generation: u64,
        token: u64,
        operation_id: Option<String>,
    ) -> Result<(), SupervisorError> {
        let build = definition.profiles.build.clone().unwrap();
        let cancel = CancellationToken::new();
        self.build_aborts
            .lock()
            .unwrap()
            .insert(service_id.clone(), cancel.clone());
        let timeout_ms = build.timeout_ms.unwrap_or(15 * 60_000);
        let sup = self.clone();
        let sid = service_id.clone();
        let on_output: OnOutput = Arc::new(move |data: &str| {
            if sup.valid(&sid, generation, token) {
                sup.append_output(&sid, data);
            }
        });

        let cancel_for_timeout = cancel.clone();
        let timed_out = Arc::new(AtomicBool::new(false));
        let timed_out_flag = timed_out.clone();
        let timeout_task = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(timeout_ms)).await;
            timed_out_flag.store(true, Ordering::SeqCst);
            cancel_for_timeout.cancel();
        });

        let result = if let Some(key) = &build.serialization_key {
            self.serialized_build(key, cancel.clone(), &build.command, on_output)
                .await
        } else {
            self.options
                .run_build
                .run(&build.command, on_output, cancel.clone())
                .await
        };
        timeout_task.abort();
        self.build_aborts.lock().unwrap().remove(service_id);

        match result {
            Ok(()) => Ok(()),
            Err(error) => {
                if !self.valid(service_id, generation, token) || (self.options.is_closing)() {
                    return Ok(());
                }
                // A restart or stop that aborts the build also bumps the token, so it returns above;
                // a cancellation still reaching here is reported as what it is, not as a timeout.
                let message = if timed_out.load(Ordering::SeqCst) {
                    "Build timed out"
                } else if cancel.is_cancelled() {
                    "Build cancelled"
                } else {
                    "Build failed"
                };
                self.transition_if_current(
                    service_id,
                    generation,
                    token,
                    ActualServiceState::Failed,
                    ServiceReadiness::Failed,
                    Changes {
                        error: Patch::Set(message.to_string()),
                        current_operation_id: operation_id.map(Patch::Set).unwrap_or(Patch::Keep),
                        ..Default::default()
                    },
                )
                .await;
                Err(error)
            }
        }
    }

    async fn serialized_build(
        self: &Arc<Self>,
        key: &str,
        cancel: CancellationToken,
        command: &ServiceCommand,
        on_output: OnOutput,
    ) -> Result<(), SupervisorError> {
        let run_build = self.options.run_build.clone();
        let command = command.clone();
        self.build_serials
            .run(&key.to_string(), || async move {
                if cancel.is_cancelled() {
                    return Err(SupervisorError("Build cancelled".to_string()));
                }
                // `run` owns the cancel path (SIGTERM, grace, SIGKILL). A second select here drops
                // that future during the grace and leaks the build process.
                run_build.run(&command, on_output, cancel).await
            })
            .await
    }

    /// Runs a declarative preparation command through `probes.command()`, serialized against every
    /// other in-flight preparation command sharing the same `key` — the preparation analogue of
    /// `serialized_build`, minus the cancellation plumbing (unlike a build, nothing today cancels an
    /// in-flight preparation command).
    async fn serialized_preparation_command(
        &self,
        key: &str,
        command: &CommandSpec,
        cwd: Option<&str>,
    ) -> Option<bool> {
        let probes = self.options.probes.clone();
        let command = command.clone();
        let cwd = cwd.map(str::to_string);
        self.preparation_serials
            .run(&key.to_string(), || async move {
                probes.command(&command, cwd.as_deref()).await
            })
            .await
    }

    fn abort_build(&self, service_id: &ServiceId) {
        if let Some(token) = self.build_aborts.lock().unwrap().get(service_id) {
            token.cancel();
        }
    }

    async fn probe(&self, readiness: &ReadinessSpec, service_id: &ServiceId) -> bool {
        match readiness {
            ReadinessSpec::Tcp { port } => self.options.probes.tcp(*port).await,
            ReadinessSpec::Http { url } => self.options.probes.http(url).await,
            ReadinessSpec::Tailnet => self.options.probes.tailnet().await,
            // One probe, not a service deadline. `readinessTimeoutMs` (else the supervisor
            // default) bounds this command so a hung script ends. Dropping the future kills its
            // process group (see `default_adapters::run_command`). The continuous loop runs the
            // probe outside the per-service queue, so stop is not stuck behind it.
            ReadinessSpec::Command { command, cwd } => {
                let budget =
                    Duration::from_millis(self.readiness_timeout_ms(service_id).max(1) as u64);
                tokio::time::timeout(budget, self.options.probes.command(command, cwd.as_deref()))
                    .await
                    .ok()
                    .flatten()
                    .unwrap_or(false)
            }
            ReadinessSpec::Container => {
                let Ok(profile) = profile_for(&self.host.catalog(), service_id) else {
                    return false;
                };
                match profile.command.container_name {
                    Some(name) => self.options.probes.container(&name).await,
                    None => false,
                }
            }
            ReadinessSpec::Process | ReadinessSpec::Exit => false,
        }
    }

    fn readiness_timeout_ms(&self, service_id: &ServiceId) -> i64 {
        profile_for(&self.host.catalog(), service_id)
            .ok()
            .and_then(|profile| profile.readiness_timeout_ms)
            .map(|v| v as i64)
            .unwrap_or(self.options.readiness_timeout_ms)
    }

    fn attach_output(self: &Arc<Self>, service_id: &ServiceId, source: OutputSource<'_>) {
        self.detach_output(service_id);
        // `attach_output` is optional on `ProcessAdapter` — a `None` result (no attachment
        // support) is a normal, expected outcome here.
        let on_output: OnOutput = {
            let this = self.clone();
            let sid = service_id.clone();
            Arc::new(move |data: &str| {
                this.append_output(&sid, data);
            })
        };
        if let Some(tail) = self
            .options
            .process
            .attach_output(service_id, source, on_output)
        {
            self.output_tails
                .lock()
                .unwrap()
                .insert(service_id.clone(), tail);
        }
    }

    fn detach_output(&self, service_id: &ServiceId) {
        // Released before `stop()`: a raw-file tail drains synchronously on stop, and that file
        // I/O must not run (or panic) while every other service's tail map access waits on it.
        let tail = self.output_tails.lock().unwrap().remove(service_id);
        if let Some(tail) = tail {
            tail.stop();
        }
    }

    fn has_live_output_tail(&self, service_id: &ServiceId) -> bool {
        self.output_tails
            .lock()
            .unwrap()
            .get(service_id)
            .map(|tail| !tail.is_done())
            .unwrap_or(false)
    }

    /// Log forwarding is fire-and-forget by design: a failure must never propagate into the caller.
    /// It is also strictly ordered — chunks go through this service's single forwarder task, so the
    /// order they were emitted in is the order they reach the log file.
    fn append_output(&self, service_id: &ServiceId, data: &str) {
        let existing = self.log_forwarders.lock().unwrap().get(service_id).cloned();
        let forwarder = match existing {
            Some(forwarder) => forwarder,
            None => {
                // Spawned outside the map lock, so a spawn failure can't poison it. A racing
                // caller's forwarder loses the insert below and ends once its sender is dropped.
                let created = self.spawn_log_forwarder(service_id);
                self.log_forwarders
                    .lock()
                    .unwrap()
                    .entry(service_id.clone())
                    .or_insert(created)
                    .clone()
            }
        };
        if forwarder.sender.try_send(data.to_string()).is_err() {
            forwarder.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn spawn_log_forwarder(&self, service_id: &ServiceId) -> LogForwarder {
        let (sender, mut rx) = tokio::sync::mpsc::channel::<String>(LOG_FORWARD_CAPACITY);
        let dropped = Arc::new(AtomicU64::new(0));
        let host = self.host.clone();
        let service_id = service_id.clone();
        let task_dropped = dropped.clone();
        tokio::spawn(async move {
            while let Some(chunk) = rx.recv().await {
                let missed = task_dropped.swap(0, Ordering::Relaxed);
                if missed > 0 {
                    host.append_log(&service_id, &format!("[hearth: {missed} log chunks dropped — output outpaced the log store]\n")).await;
                }
                host.append_log(&service_id, &chunk).await;
            }
        });
        LogForwarder { sender, dropped }
    }

    fn state(&self, service_id: &ServiceId) -> Option<ServiceLifecycleState> {
        self.host.service_state(service_id)
    }

    fn current_token(&self, service_id: &ServiceId) -> u64 {
        *self.tokens.lock().unwrap().get(service_id).unwrap_or(&0)
    }

    fn cancel(&self, service_id: &ServiceId) -> u64 {
        let mut tokens = self.tokens.lock().unwrap();
        let next = tokens.get(service_id).copied().unwrap_or(0) + 1;
        tokens.insert(service_id.clone(), next);
        next
    }

    fn valid(&self, service_id: &ServiceId, generation: u64, token: u64) -> bool {
        self.current_token(service_id) == token
            && self.state(service_id).map(|s| s.generation) == Some(generation)
    }

    fn identity_matches_state(&self, state: &ServiceLifecycleState) -> bool {
        state.identity.as_ref().is_some_and(|identity| {
            identity.service_id() == &state.service_id && identity.generation() == state.generation
        })
    }

    /// `Some(bool)` is a definitive answer; `None` means the probe could not run (a wedged
    /// `docker`/`ps` — `Inspection::Unknown`), and callers must not conclude either way.
    async fn owns(&self, state: &ServiceLifecycleState) -> Option<bool> {
        if !self.identity_matches_state(state) {
            return Some(false);
        }
        self.owns_identity(state.identity.as_ref().unwrap()).await
    }

    fn same_executable(&self, record: &ProcessRecord, identity: &ProcessIdentity) -> bool {
        let ProcessRecord::Posix(record) = record else {
            return false;
        };
        let ProcessIdentity::Posix(identity) = identity else {
            return false;
        };
        let Ok(profile) = profile_for(&self.host.catalog(), &identity.service_id) else {
            return false;
        };
        let Some(exe) = Self::absolute_argv0(&profile.command) else {
            return false;
        };
        executable_matches(&record.command_line, &[exe])
    }

    async fn observed_matches(&self, identity: &ProcessIdentity) -> Option<bool> {
        let observed = match self.options.process.inspect(identity).await {
            Inspection::Observed(observed) => observed,
            Inspection::Gone => return Some(false),
            Inspection::Unknown => return None,
        };
        if !observed.alive {
            return Some(false);
        }
        // Redis `setproctitle` keeps the binary and replaces the arguments
        // (`redis-server {dataDir}/redis.conf` → `redis-server 127.0.0.1:port`). The pid and
        // lstart below still have to match, so this is the same process, not a scan of the table.
        if observed.record.command_fingerprint() != identity.command_fingerprint()
            && !self.same_executable(&observed.record, identity)
        {
            return Some(false);
        }
        Some(match (identity, &observed.record) {
            (ProcessIdentity::Docker(id), ProcessRecord::Docker(rec)) => {
                rec.container_name == id.container_name
                    && rec.container_id == id.container_id
                    && rec.container_started_at == id.container_started_at
            }
            (ProcessIdentity::Posix(id), ProcessRecord::Posix(rec)) => {
                rec.pid == id.pid && rec.pgid == id.pgid && rec.start_identity == id.start_identity
            }
            _ => false,
        })
    }

    async fn owns_identity(&self, identity: &ProcessIdentity) -> Option<bool> {
        if identity.manager_instance_id() != self.host.instance_id() {
            return Some(false);
        }
        self.observed_matches(identity).await
    }

    /// The readiness loop's liveness check. A process this manager spawned has an exit watcher, so
    /// its flag answers without a shell-out; an adopted identity (or one with no live `Active`
    /// entry) has none, and falls back to a full `ps`/`docker inspect` ownership check.
    async fn still_running(
        &self,
        service_id: &ServiceId,
        identity: &ProcessIdentity,
        adopted: bool,
    ) -> bool {
        if !adopted {
            let exited = self
                .active
                .lock()
                .unwrap()
                .get(service_id)
                .filter(|active| active.identity.generation() == identity.generation())
                .map(|active| active.exited.clone());
            if let Some(exited) = exited {
                return !exited.load(Ordering::SeqCst);
            }
        }
        // `!= Some(false)`: a probe that can't answer doesn't kill the readiness wait.
        self.owns_identity(identity).await != Some(false)
    }

    async fn orphan(&self, state: &ServiceLifecycleState, operation_id: Option<String>) {
        self.transition(
            &state.service_id,
            state.generation,
            ActualServiceState::Orphaned,
            ServiceReadiness::Failed,
            Changes {
                error: Patch::Set("Process ownership identity no longer matches".to_string()),
                current_operation_id: operation_id.map(Patch::Set).unwrap_or(Patch::Keep),
                ..Default::default()
            },
        )
        .await;
    }

    async fn transition_if_current(
        &self,
        service_id: &ServiceId,
        generation: u64,
        token: u64,
        actual: ActualServiceState,
        readiness: ServiceReadiness,
        changes: Changes,
    ) {
        if self.valid(service_id, generation, token) {
            self.transition(service_id, generation, actual, readiness, changes)
                .await;
        }
    }

    async fn transition(
        &self,
        service_id: &ServiceId,
        generation: u64,
        actual_state: ActualServiceState,
        readiness: ServiceReadiness,
        changes: Changes,
    ) {
        let previous = self.state(service_id);
        let now = self.options.clock.now();
        let next = ServiceLifecycleState {
            service_id: service_id.clone(),
            desired_state: changes.desired_state.unwrap_or_else(|| {
                previous
                    .as_ref()
                    .map(|p| p.desired_state)
                    .unwrap_or(DesiredServiceState::Running)
            }),
            actual_state,
            readiness,
            generation,
            created_at: previous
                .as_ref()
                .map(|p| p.created_at.clone())
                .unwrap_or_else(|| now.clone()),
            updated_at: now.clone(),
            identity: changes
                .identity
                .apply(previous.as_ref().and_then(|p| p.identity.clone())),
            readiness_kind: changes
                .readiness_kind
                .apply(previous.as_ref().and_then(|p| p.readiness_kind)),
            readiness_detail: changes
                .readiness_detail
                .apply(previous.as_ref().and_then(|p| p.readiness_detail.clone())),
            exited_at: changes
                .exited_at
                .apply(previous.as_ref().and_then(|p| p.exited_at.clone())),
            exit_code: changes
                .exit_code
                .apply(previous.as_ref().and_then(|p| p.exit_code)),
            error: changes
                .error
                .apply(previous.as_ref().and_then(|p| p.error.clone())),
            current_operation_id: changes.current_operation_id.apply(
                previous
                    .as_ref()
                    .and_then(|p| p.current_operation_id.clone()),
            ),
        };
        // `Host::set_service_state` publishes the one `service.lifecycle` event for this change;
        // publishing here too doubled every transition's SSE frames and event-buffer use.
        let error = next.error.clone();
        let updated_at = next.updated_at.clone();
        self.host.set_service_state(next).await;
        if let Some(error) = &error {
            self.host
                .append_log(service_id, &format!("{updated_at} {error}\n"))
                .await;
        }
    }
}

fn readiness_tcp_port(readiness: &ReadinessSpec) -> Option<u16> {
    match readiness {
        ReadinessSpec::Tcp { port } => Some(*port),
        _ => None,
    }
}

/// Text after the last `exec` keyword. `exec 'build/install/name/bin/name'` keeps the path;
/// `exec node dist/main` keeps the program and its arguments.
fn exec_program(shell: &str) -> Option<String> {
    let exec_at = last_shell_word(shell, "exec")?;
    let rest = shell[exec_at + 4..].trim();
    if rest.is_empty() {
        return None;
    }
    Some(unquote_exec(rest))
}

/// Directory from the last `cd` before `exec`. `cd -- 'portal/metadata'` yields that path.
fn shell_cd_dir(shell: &str) -> Option<String> {
    let exec_at = last_shell_word(shell, "exec")?;
    let before = &shell[..exec_at];
    let cd_at = last_shell_word(before, "cd")?;
    let mut after = before[cd_at + 2..].trim_start();
    if let Some(rest) = after.strip_prefix("--") {
        after = rest.trim_start();
    }
    if after.is_empty() {
        return None;
    }
    let quote = after.as_bytes()[0];
    if quote == b'\'' || quote == b'"' {
        let body = &after[1..];
        let end = body.find(quote as char)?;
        return Some(body[..end].to_string());
    }
    Some(after.split_whitespace().next()?.to_string())
}

fn last_shell_word(text: &str, word: &str) -> Option<usize> {
    let mut found = None;
    for (index, _) in text.match_indices(word) {
        let before_ok = index == 0
            || text[..index]
                .chars()
                .next_back()
                .is_none_or(|c| !c.is_ascii_alphanumeric() && c != '_');
        let after = index + word.len();
        let after_ok = after >= text.len()
            || text[after..]
                .chars()
                .next()
                .is_none_or(|c| !c.is_ascii_alphanumeric() && c != '_');
        if before_ok && after_ok {
            found = Some(index);
        }
    }
    found
}

fn unquote_exec(rest: &str) -> String {
    let rest = rest.trim();
    let bytes = rest.as_bytes();
    if bytes.len() >= 2
        && (bytes[0] == b'\'' || bytes[0] == b'"')
        && bytes[bytes.len() - 1] == bytes[0]
    {
        let inner = &rest[1..rest.len() - 1];
        if !inner.contains(bytes[0] as char) {
            return inner.to_string();
        }
    }
    rest.to_string()
}

/// Where the exec'd program is running. A `cd` inside the shell wins over the command cwd,
/// because that is the directory `ps` reports after `cd && exec`.
fn process_cwd(command: &ServiceCommand) -> String {
    let CommandSpec::Shell { shell, .. } = &command.command else {
        return command.cwd.clone();
    };
    let Some(dir) = shell_cd_dir(shell) else {
        return command.cwd.clone();
    };
    if dir.starts_with('/') || command.cwd == "." || command.cwd.is_empty() {
        return dir;
    }
    format!("{}/{}", command.cwd.trim_end_matches('/'), dir)
}

/// Logical command, plus the `exec` target as `ps` will show it (`node dist/main`).
fn exec_target_fingerprints(command: &ServiceCommand) -> Vec<String> {
    let mut fingerprints = vec![normalize_command_fingerprint(command)];
    let CommandSpec::Shell { shell, .. } = &command.command else {
        return fingerprints;
    };
    if let Some(program) = exec_program(shell) {
        let observed = normalize_observed_command_fingerprint(&program);
        if !fingerprints.contains(&observed) {
            fingerprints.push(observed);
        }
    }
    fingerprints
}

/// `cd portal/metadata && exec build/install/portal.metadata/bin/portal.metadata` becomes
/// `portal/metadata/build/install/portal.metadata`. A Gradle script's `java` command line still
/// contains that directory. `exec node dist/main` has no install directory.
fn install_dir_marker(command: &ServiceCommand) -> Option<String> {
    let CommandSpec::Shell { shell, .. } = &command.command else {
        return None;
    };
    let program = exec_program(shell)?;
    let first = program.split_whitespace().next()?;
    let (dir, _) = first.rsplit_once("/bin/")?;
    if dir.is_empty() || dir == "." || dir == ".." {
        return None;
    }
    let marker = match shell_cd_dir(shell) {
        Some(cd) if !dir.starts_with('/') => format!("{cd}/{dir}"),
        _ => dir.to_string(),
    };
    let marker = marker.trim_start_matches("./").to_string();
    marker.contains("build/install/").then_some(marker)
}

fn with_manager_instance(identity: ProcessIdentity, instance_id: String) -> ProcessIdentity {
    match identity {
        ProcessIdentity::Posix(mut p) => {
            p.manager_instance_id = instance_id;
            ProcessIdentity::Posix(p)
        }
        ProcessIdentity::Docker(mut d) => {
            d.manager_instance_id = instance_id;
            ProcessIdentity::Docker(d)
        }
    }
}

fn output_source(identity: &ProcessIdentity, skip_backlog: bool) -> OutputSource<'_> {
    match identity {
        ProcessIdentity::Posix(_) => OutputSource::Process { skip_backlog },
        ProcessIdentity::Docker(d) => OutputSource::Container {
            container_name: &d.container_name,
            since: Some(d.container_started_at.as_str()).filter(|s| !s.is_empty()),
            tail: None,
        },
    }
}

// =============================================================================================
// Tests — every adapter is a fake. `FakeProcessAdapter` keeps a simulated process table (pid,
// pgid, start identity, parent), so the whole-tree stop path — snapshot, leader group, secondary
// groups — runs against it exactly as it runs against `ps` in production.

#[cfg(test)]
mod tests;
