//! `ProcessSupervisor` — the start/stop/restart/readiness/adoption state machine. It touches the
//! OS only through the `ProcessAdapter`/`ProbeAdapter`/`PreparationAdapter`/`Host` traits, so it
//! runs against fakes in its unit tests; `default_adapters.rs` is the production implementation.
use std::collections::HashMap;
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

use super::fingerprint::normalize_command_fingerprint;
use super::process_tree::{
    process_tree_alive, secondary_process_groups, ProcessTreeEntry, ProcessTreeSnapshot,
};
use super::types::{
    Host, Inspection, ManagedProcess, ObservedProcess, OnOutput, OutputSource, OutputTail,
    ProcessRecord, ProcessSignal, SpawnInput, StartOptions, SupervisorError, SupervisorOptions,
};

/// `Running` is never produced (see its doc), but a persisted `state.json` may still carry it — it
/// stays "active" so such a row is reconciled rather than ignored.
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

/// A "task" command has no long-lived managed process to track — it runs once to bring external
/// state into the desired shape (generalizes a tailnet-serve-config runner).
fn is_task_command(readiness: &ReadinessSpec) -> bool {
    matches!(readiness, ReadinessSpec::Tailnet)
}

/// An `ownership: external` service whose readiness is a `command` probe is also a task: its run
/// command is a one-shot "bring the external thing up" trigger (e.g. the generated
/// `hearthd shared attach` for a `shared:` entry), not the service process itself. Only the
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
    /// Set by the spawned process's exit watcher the moment it exits. `on_exit` itself waits for the
    /// per-service lock — which an in-flight start holds through readiness — so the readiness loop
    /// reads this instead of spawning `ps`/`docker inspect` on every iteration to notice an exit.
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
    Timeout { message: String, detail: String },
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
                sup.stop_locked(&id, op.clone()).await?;
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

        // Container log followers are real child processes (`docker logs -f`) in their own process
        // group — they would outlive the daemon as orphans streaming to a dead pipe.
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
        if let Err(error) = self
            .terminate_posix_tree(posix.pid, &posix.start_identity, posix.pgid)
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
                if sup.owns(&state).await == Some(false) {
                    sup.orphan(&state, None).await;
                    return Ok(());
                }
                let profile = profile_for(&sup.host.catalog(), &id)?;
                if matches!(
                    profile.readiness,
                    ReadinessSpec::Process | ReadinessSpec::Exit
                ) {
                    return Ok(());
                }
                if !sup.probe(&profile.readiness, &id).await {
                    sup.transition(
                        &id,
                        state.generation,
                        ActualServiceState::RunningUnready,
                        ServiceReadiness::NotReady,
                        Changes {
                            readiness_kind: Patch::Set(readiness_kind_of(&profile.readiness)),
                            readiness_detail: Patch::Set(
                                "Readiness probe is currently unavailable".to_string(),
                            ),
                            ..Default::default()
                        },
                    )
                    .await;
                } else if state.actual_state == ActualServiceState::RunningUnready {
                    // The way back: a probe that failed earlier and passes now. Without it a service
                    // stayed `running-unready` after recovering, until something restarted it.
                    sup.transition(
                        &id,
                        state.generation,
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
                }
                Ok(())
            })
            .await
    }

    pub async fn reconcile(self: &Arc<Self>) {
        for state in self.host.service_states() {
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
                    sup.transition(
                        &service_id,
                        state.generation,
                        ActualServiceState::RunningUnready,
                        ServiceReadiness::NotReady,
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
                    let Ok(profile) = profile_for(&sup.host.catalog(), &service_id) else {
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
        let retained_identity = match &current {
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
        let generation = match &retained_identity {
            Some(ProcessIdentity::Posix(p)) => p.generation,
            _ => current.as_ref().map(|s| s.generation).unwrap_or(0) + 1,
        };
        let mut token = self.cancel(service_id);

        if let Some(retained) = retained_identity {
            let identity = with_manager_instance(retained, self.host.instance_id());
            self.transition(
                service_id,
                generation,
                ActualServiceState::RunningUnready,
                ServiceReadiness::NotReady,
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
            // An adopted identity is only worth keeping while it still answers its readiness probe.
            let outcome = self
                .await_readiness(
                    service_id,
                    &profile,
                    generation,
                    Some(identity.clone()),
                    token,
                    true,
                )
                .await;
            match outcome {
                ReadinessOutcome::Ready => {
                    self.arm_adopted_watch(
                        service_id,
                        identity,
                        profile.command.clone(),
                        generation,
                        token,
                    );
                    return Ok(());
                }
                ReadinessOutcome::Superseded => return Ok(()),
                _ => {}
            }
            token = self.cancel(service_id);
            // Replacement is only safe once terminate has actually stopped the old process. An
            // unverified liveness check used to be swallowed, and the next spawn ran beside it.
            match self.terminate(&identity, &profile.command).await {
                Err(error) => {
                    self.transition(
                        service_id,
                        generation,
                        ActualServiceState::Failed,
                        ServiceReadiness::Failed,
                        Changes {
                            error: Patch::Set(error.0.clone()),
                            identity: Patch::Set(identity),
                            ..Default::default()
                        },
                    )
                    .await;
                    return Err(error);
                }
                Ok(()) => {
                    if self.owns_identity(&identity).await != Some(false) {
                        let error = SupervisorError(format!(
                            "{service_id} was not replaced: the adopted process is still alive"
                        ));
                        self.transition(
                            service_id,
                            generation,
                            ActualServiceState::Failed,
                            ServiceReadiness::Failed,
                            Changes {
                                error: Patch::Set(error.0.clone()),
                                identity: Patch::Set(identity.clone()),
                                ..Default::default()
                            },
                        )
                        .await;
                        return Err(error);
                    }
                }
            }
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
                // `killUnowned` is the client's echo of "yes, kill whatever holds my port" — the
                // ONLY path in this engine that signals a process it does not own.
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
            None => return Err(SupervisorError(format!(
                "{service_id} cannot be stopped: this process adapter has no stop command support"
            ))),
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
                .readiness(service_id, profile, generation, None, token, operation_id)
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
        self.transition(
            service_id,
            generation,
            ActualServiceState::RunningUnready,
            ServiceReadiness::NotReady,
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
        let outcome = self
            .await_readiness(
                service_id,
                profile,
                generation,
                identity.clone(),
                token,
                adopted,
            )
            .await;
        match outcome {
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
            ReadinessOutcome::Timeout { message, detail } => {
                self.fail(
                    service_id,
                    generation,
                    token,
                    identity,
                    &message,
                    operation_id,
                    Some((readiness_kind_of(&profile.readiness), detail)),
                )
                .await;
                Err(SupervisorError(message))
            }
        }
    }

    /// Non-throwing readiness wait. `Superseded` means a newer operation (or a closing manager) took
    /// over this service's token, so the caller must stop quietly instead of reporting a failure.
    async fn await_readiness(
        self: &Arc<Self>,
        service_id: &ServiceId,
        profile: &VerifiedProfile,
        generation: u64,
        identity: Option<ProcessIdentity>,
        token: u64,
        adopted: bool,
    ) -> ReadinessOutcome {
        if matches!(profile.readiness, ReadinessSpec::Exit) {
            return self
                .await_exit(service_id, profile, generation, identity, token, adopted)
                .await;
        }
        if matches!(profile.readiness, ReadinessSpec::Process) {
            if !self.valid(service_id, generation, token) {
                return ReadinessOutcome::Superseded;
            }
            if let Some(identity) = &identity {
                if self.owns_identity(identity).await == Some(false) {
                    return if self.valid(service_id, generation, token) {
                        ReadinessOutcome::Exited {
                            message: "Process exited before liveness check".to_string(),
                        }
                    } else {
                        ReadinessOutcome::Superseded
                    };
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
            return ReadinessOutcome::Ready;
        }
        let timeout = profile
            .readiness_timeout_ms
            .map(|v| v as i64)
            .unwrap_or(self.options.readiness_timeout_ms);
        let deadline = self.options.clock.now_millis() + timeout;
        loop {
            if self.options.clock.now_millis() > deadline {
                break;
            }
            if !self.valid(service_id, generation, token) {
                return ReadinessOutcome::Superseded;
            }
            if let Some(identity) = &identity {
                if !self.still_running(service_id, identity, adopted).await {
                    return if self.valid(service_id, generation, token) {
                        ReadinessOutcome::Exited {
                            message: "Process exited before readiness".to_string(),
                        }
                    } else {
                        ReadinessOutcome::Superseded
                    };
                }
            }
            if self.probe(&profile.readiness, service_id).await {
                self.transition_if_current(
                    service_id,
                    generation,
                    token,
                    ActualServiceState::Ready,
                    ServiceReadiness::Ready,
                    Changes {
                        identity: identity.clone().map(Patch::Set).unwrap_or(Patch::Keep),
                        readiness_kind: Patch::Set(readiness_kind_of(&profile.readiness)),
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
                return ReadinessOutcome::Ready;
            }
            self.options
                .clock
                .sleep(self.options.readiness_backoff_ms)
                .await;
        }
        ReadinessOutcome::Timeout {
            message: "Readiness timed out".to_string(),
            detail: format!(
                "Readiness {} probe timed out after {}ms",
                readiness_kind_of(&profile.readiness).as_wire_str(),
                timeout
            ),
        }
    }

    /// Wait until a `readiness: exit` command's process exits. Exit 0 is `succeeded` with desired
    /// `stopped` (reconcile must not run it again). Any other code is `failed`, also with desired
    /// `stopped`. `readiness_timeout_ms` is the only deadline — the supervisor's default timeout
    /// is for probes, and a build can run for minutes.
    ///
    /// An adopted process (daemon restarted while the command was still running) has no watcher,
    /// so its exit code is unknown: when it disappears the result is `failed`, never `succeeded`.
    async fn await_exit(
        self: &Arc<Self>,
        service_id: &ServiceId,
        profile: &VerifiedProfile,
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
            ActualServiceState::RunningUnready,
            ServiceReadiness::NotReady,
            Changes {
                identity: identity.clone().map(Patch::Set).unwrap_or(Patch::Keep),
                readiness_kind: Patch::Set(ReadinessKind::Exit),
                readiness_detail: Patch::Set("waiting for the command to exit".to_string()),
                ..Default::default()
            },
        )
        .await;
        let deadline = profile
            .readiness_timeout_ms
            .map(|ms| self.options.clock.now_millis() + ms as i64);
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
            if let Some(deadline) = deadline {
                if self.options.clock.now_millis() > deadline {
                    let timeout = profile.readiness_timeout_ms.unwrap_or(0);
                    return ReadinessOutcome::Timeout {
                        message: "Readiness timed out".to_string(),
                        detail: format!("Command did not exit within {timeout}ms"),
                    };
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
                ServiceReadiness::Ready,
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
                self.terminate_posix_tree(posix.pid, &posix.start_identity, posix.pgid)
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

    async fn secondary_groups_alive(
        &self,
        tree: &[ProcessTreeEntry],
        leader_pgid: i64,
    ) -> Option<bool> {
        let groups = secondary_process_groups(tree, leader_pgid);
        if groups.is_empty() {
            return Some(false);
        }
        let alive = self.options.process.live_start_identities().await?;
        Some(
            groups
                .values()
                .flatten()
                .any(|member| alive.get(&member.pid) == Some(&member.start_identity)),
        )
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

    async fn signal_secondary_groups(
        &self,
        tree: &[ProcessTreeEntry],
        leader_pgid: i64,
        signal: ProcessSignal,
    ) {
        self.signal_verified_members(tree, leader_pgid, signal, false)
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
    ) -> Result<(), SupervisorError> {
        let snapshot = self.process_tree(pid, start_identity).await;
        let tree = match &snapshot {
            ProcessTreeSnapshot::Unknown => {
                return Err(SupervisorError(
                    "the process tree could not be read; refusing to report it stopped".to_string(),
                ));
            }
            ProcessTreeSnapshot::Absent => return Ok(()),
            ProcessTreeSnapshot::Present(tree) => tree.clone(),
        };
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

    /// After the leader has exited: signal only secondary groups from the last snapshot taken
    /// while it was alive. Never `killpg` the dead leader's pgid.
    async fn reap_secondary_groups(&self, tree: &[ProcessTreeEntry], leader_pgid: i64) {
        if secondary_process_groups(tree, leader_pgid).is_empty() {
            return;
        }
        self.signal_secondary_groups(tree, leader_pgid, ProcessSignal::Sigterm)
            .await;
        let deadline = self.options.clock.now_millis() + self.options.termination_grace_ms;
        loop {
            if self.options.clock.now_millis() >= deadline {
                break;
            }
            match self.secondary_groups_alive(tree, leader_pgid).await {
                Some(true) => {
                    self.options
                        .clock
                        .sleep(self.options.readiness_backoff_ms)
                        .await
                }
                Some(false) | None => return,
            }
        }
        if self.secondary_groups_alive(tree, leader_pgid).await == Some(true) {
            self.signal_secondary_groups(tree, leader_pgid, ProcessSignal::Sigkill)
                .await;
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
                    *last_tree.lock().unwrap() = tree;
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
                                *last_tree.lock().unwrap() = tree;
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
            // Bounded by the service's readiness timeout: a hung probe script runs under the
            // per-service lock and would otherwise make the service unstoppable. Dropping the
            // probe future kills its process group (see `default_adapters::run_command`).
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

    async fn observed_matches(&self, identity: &ProcessIdentity) -> Option<bool> {
        let observed = match self.options.process.inspect(identity).await {
            Inspection::Observed(observed) => observed,
            Inspection::Gone => return Some(false),
            Inspection::Unknown => return None,
        };
        if !observed.alive
            || observed.record.command_fingerprint() != identity.command_fingerprint()
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
mod tests {
    use super::*;
    use crate::catalog::{
        CommandSpec, ReadinessSpec, ServiceBuildProfile, ServiceCommand, ServiceDefinition,
        ServiceKind, ServiceOwnership, ServiceProfiles, ServiceRunProfile, StartFailurePolicy,
    };
    use crate::state::{DesiredServiceState, PosixProcessIdentity, ServiceReadiness};
    use crate::supervisor::process_tree::ProcessTreeSnapshot;
    use crate::supervisor::types::{
        DockerContainerRecord, Host, ManagedProcess, ObservedProcess, OnOutput, OutputSource,
        OutputTail, PortHolder, PosixProcessRecord, PreparationAdapter, ProbeAdapter,
        ProcessAdapter, ProcessRecord, RunBuild, SpawnInput, SupervisorClock, SupervisorError,
        SupervisorOptions,
    };
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicI64, Ordering};
    use tokio::sync::oneshot;

    // --- Fake clock: sleeps advance a virtual clock instantly, so readiness-timeout tests run in
    // milliseconds instead of really waiting. ---
    struct FakeClock {
        millis: AtomicI64,
    }
    impl FakeClock {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                millis: AtomicI64::new(1_700_000_000_000),
            })
        }
    }
    #[async_trait]
    impl SupervisorClock for FakeClock {
        fn now_millis(&self) -> i64 {
            self.millis.load(Ordering::SeqCst)
        }
        async fn sleep(&self, millis: i64) {
            self.millis.fetch_add(millis.max(1), Ordering::SeqCst);
            tokio::task::yield_now().await;
        }
    }

    // --- Fake process adapter ---
    #[derive(Default)]
    struct FakeProcessAdapterState {
        next_pid: i64,
        alive: HashMap<i64, PosixProcessRecord>,
        /// child pid -> parent pid, for the simulated process tree.
        parents: HashMap<i64, i64>,
        signals: Vec<(i64, ProcessSignal)>,
        pid_signals: Vec<(i64, ProcessSignal)>,
        /// Squatter processes that ignore SIGTERM (still die to SIGKILL) or never die at all —
        /// the reclaim tests' "holder survives" knobs.
        term_immune: std::collections::HashSet<i64>,
        unkillable: std::collections::HashSet<i64>,
        spawn_should_fail: HashMap<ServiceId, String>,
        container_records: HashMap<String, DockerContainerRecord>,
        exit_senders: HashMap<i64, oneshot::Sender<i32>>,
        attach_output_calls: Vec<(ServiceId, &'static str)>,
        tail_dones: HashMap<ServiceId, Arc<std::sync::atomic::AtomicBool>>,
        tails_stopped: Vec<ServiceId>,
        stop_container_calls: Vec<String>,
        /// `ps` could not be read. Distinct from an empty tree (leader gone).
        tree_unknown: bool,
    }
    struct FakeProcessAdapter {
        state: Arc<Mutex<FakeProcessAdapterState>>,
    }
    impl FakeProcessAdapter {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                state: Arc::new(Mutex::new(FakeProcessAdapterState {
                    next_pid: 900_001,
                    ..Default::default()
                })),
            })
        }
        fn attach_calls(&self) -> Vec<(ServiceId, &'static str)> {
            self.state.lock().unwrap().attach_output_calls.clone()
        }
        /// Simulates a follower ending on its own — `docker logs --follow` exits when its
        /// container does, leaving a stale stop handle in the supervisor's map.
        fn finish_tail(&self, service_id: &str) {
            if let Some(done) = self.state.lock().unwrap().tail_dones.get(service_id) {
                done.store(true, Ordering::SeqCst);
            }
        }
        fn tail_was_stopped(&self, service_id: &str) -> bool {
            self.state
                .lock()
                .unwrap()
                .tails_stopped
                .iter()
                .any(|id| id == service_id)
        }
        fn stop_container_calls(&self) -> Vec<String> {
            self.state.lock().unwrap().stop_container_calls.clone()
        }
        fn fail_spawn(&self, service_id: &str, message: &str) {
            self.state
                .lock()
                .unwrap()
                .spawn_should_fail
                .insert(service_id.to_string(), message.to_string());
        }
        fn register_container(&self, name: &str, record: DockerContainerRecord) {
            self.state
                .lock()
                .unwrap()
                .container_records
                .insert(name.to_string(), record);
        }
        fn signals(&self) -> Vec<(i64, ProcessSignal)> {
            self.state.lock().unwrap().signals.clone()
        }
        fn pid_signals(&self) -> Vec<(i64, ProcessSignal)> {
            self.state.lock().unwrap().pid_signals.clone()
        }
        fn mark_term_immune(&self, pid: i64) {
            self.state.lock().unwrap().term_immune.insert(pid);
        }
        fn mark_unkillable(&self, pid: i64) {
            self.state.lock().unwrap().unkillable.insert(pid);
        }
        /// Simulates a process dying on its own (not via a signal from us) — fires its exit code and
        /// removes it from the alive set, exactly like a real process disappearing underneath us.
        fn kill_externally(&self, pid: i64, code: i32) {
            let mut state = self.state.lock().unwrap();
            state.alive.remove(&pid);
            if let Some(tx) = state.exit_senders.remove(&pid) {
                let _ = tx.send(code);
            }
        }
        /// Simulates `air` handing its built server its own process group: a live child of `parent`
        /// whose pgid is its own pid, so signalling only the parent's group leaves it running.
        fn fork_into_own_group(&self, parent: i64) -> i64 {
            let mut state = self.state.lock().unwrap();
            let pid = state.next_pid;
            state.next_pid += 1;
            state.alive.insert(
                pid,
                PosixProcessRecord {
                    pid,
                    pgid: pid,
                    start_identity: format!("Child Jan  1 00:00:{:02} 2024", pid % 60),
                    command_fingerprint: "child".to_string(),
                },
            );
            state.parents.insert(pid, parent);
            pid
        }
        fn is_alive(&self, pid: i64) -> bool {
            self.state.lock().unwrap().alive.contains_key(&pid)
        }
        fn insert_alive(&self, pid: i64, fingerprint: &str, start_identity: &str) {
            self.state.lock().unwrap().alive.insert(
                pid,
                PosixProcessRecord {
                    pid,
                    pgid: pid,
                    start_identity: start_identity.to_string(),
                    command_fingerprint: fingerprint.to_string(),
                },
            );
        }
        fn mutate_fingerprint(&self, pid: i64, new_fingerprint: &str) {
            if let Some(record) = self.state.lock().unwrap().alive.get_mut(&pid) {
                record.command_fingerprint = new_fingerprint.to_string();
            }
        }
    }
    #[async_trait]
    impl ProcessAdapter for FakeProcessAdapter {
        async fn spawn(
            &self,
            input: SpawnInput,
            _on_output: OnOutput,
        ) -> Result<ManagedProcess, SupervisorError> {
            let mut state = self.state.lock().unwrap();
            if let Some(message) = state.spawn_should_fail.get(&input.service_id).cloned() {
                return Err(SupervisorError(message));
            }
            if is_container_command(&input.command) {
                let name = input.command.container_name.clone().unwrap();
                let record = state
                    .container_records
                    .get(&name)
                    .cloned()
                    .unwrap_or_else(|| DockerContainerRecord {
                        container_name: name.clone(),
                        container_id: format!("container-{name}"),
                        container_started_at: "2024-01-01T00:00:00.000Z".to_string(),
                        command_fingerprint: input.command_fingerprint.clone(),
                    });
                let (_tx, rx) = oneshot::channel();
                return Ok(ManagedProcess {
                    record: ProcessRecord::Docker(record),
                    exited: rx,
                });
            }
            let pid = state.next_pid;
            state.next_pid += 1;
            let record = PosixProcessRecord {
                pid,
                pgid: pid,
                start_identity: format!("Fake Jan  1 00:00:{:02} 2024", pid % 60),
                command_fingerprint: input.command_fingerprint.clone(),
            };
            state.alive.insert(pid, record.clone());
            let (tx, rx) = oneshot::channel();
            state.exit_senders.insert(pid, tx);
            Ok(ManagedProcess {
                record: ProcessRecord::Posix(record),
                exited: rx,
            })
        }

        async fn inspect(&self, identity: &ProcessIdentity) -> Inspection {
            let state = self.state.lock().unwrap();
            match identity {
                ProcessIdentity::Posix(id) => state
                    .alive
                    .get(&id.pid)
                    .cloned()
                    .map(|record| {
                        Inspection::Observed(ObservedProcess {
                            record: ProcessRecord::Posix(record),
                            alive: true,
                        })
                    })
                    .unwrap_or(Inspection::Gone),
                ProcessIdentity::Docker(id) => state
                    .container_records
                    .get(&id.container_name)
                    .cloned()
                    .map(|record| {
                        Inspection::Observed(ObservedProcess {
                            record: ProcessRecord::Docker(record),
                            alive: true,
                        })
                    })
                    .unwrap_or(Inspection::Gone),
            }
        }

        async fn signal_group(&self, pgid: i64, signal: ProcessSignal) {
            let mut state = self.state.lock().unwrap();
            state.signals.push((pgid, signal));
            // Every member of the group dies — and only the group: a tree member that moved into a
            // group of its own survives, exactly like the real `air` case.
            let members: Vec<i64> = state
                .alive
                .values()
                .filter(|r| r.pgid == pgid)
                .map(|r| r.pid)
                .collect();
            for pid in members {
                state.alive.remove(&pid);
                if let Some(tx) = state.exit_senders.remove(&pid) {
                    let _ = tx.send(if signal == ProcessSignal::Sigterm {
                        143
                    } else {
                        137
                    });
                }
            }
        }

        async fn process_tree(
            &self,
            leader_pid: i64,
            leader_start_identity: &str,
        ) -> ProcessTreeSnapshot {
            let state = self.state.lock().unwrap();
            if state.tree_unknown {
                return ProcessTreeSnapshot::Unknown;
            }
            let rows: Vec<crate::supervisor::process_tree::PsTreeRow> = state
                .alive
                .values()
                .map(|r| crate::supervisor::process_tree::PsTreeRow {
                    pid: r.pid,
                    ppid: state.parents.get(&r.pid).copied().unwrap_or(1),
                    pgid: r.pgid,
                    start_identity: r.start_identity.clone(),
                })
                .collect();
            let tree = crate::supervisor::process_tree::build_process_tree(
                &rows,
                leader_pid,
                leader_start_identity,
            );
            if tree.is_empty() {
                ProcessTreeSnapshot::Absent
            } else {
                ProcessTreeSnapshot::Present(tree)
            }
        }

        async fn live_start_identities(&self) -> Option<HashMap<i64, String>> {
            Some(
                self.state
                    .lock()
                    .unwrap()
                    .alive
                    .values()
                    .map(|r| (r.pid, r.start_identity.clone()))
                    .collect(),
            )
        }

        async fn signal_pid(&self, pid: i64, expected_start_identity: &str, signal: ProcessSignal) {
            let mut state = self.state.lock().unwrap();
            state.pid_signals.push((pid, signal));
            // Same contract as the real adapter: a pid that no longer presents the resolved lstart
            // is not ours to signal — reclaim can never kill a recycled process in the fake either.
            let survives = state.unkillable.contains(&pid)
                || (state.term_immune.contains(&pid) && signal == ProcessSignal::Sigterm);
            if state.alive.get(&pid).map(|r| r.start_identity.as_str())
                == Some(expected_start_identity)
                && !survives
            {
                state.alive.remove(&pid);
            }
        }

        async fn stop_container(
            &self,
            command: &ServiceCommand,
            _on_output: OnOutput,
        ) -> Option<Result<(), SupervisorError>> {
            let name = command.container_name.clone()?;
            let mut state = self.state.lock().unwrap();
            state.stop_container_calls.push(name.clone());
            state.container_records.remove(&name);
            Some(Ok(()))
        }

        fn attach_output(
            &self,
            service_id: &ServiceId,
            source: OutputSource<'_>,
            _on_output: OnOutput,
        ) -> Option<OutputTail> {
            let kind = match source {
                OutputSource::Process { .. } => "process",
                OutputSource::Container { .. } => "container",
            };
            let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let stopped = self.state.clone();
            let stopped_id = service_id.clone();
            let mut state = self.state.lock().unwrap();
            state.attach_output_calls.push((service_id.clone(), kind));
            state.tail_dones.insert(service_id.clone(), done.clone());
            drop(state);
            Some(OutputTail::new(
                Box::new(move || stopped.lock().unwrap().tails_stopped.push(stopped_id)),
                done,
            ))
        }
    }

    // --- Fake probe adapter ---
    #[derive(Default)]
    struct FakeProbeAdapterState {
        tcp_ready: HashMap<u16, bool>,
        port_in_use: HashMap<u16, bool>,
        port_holders: HashMap<u16, Vec<PortHolder>>,
        /// Simulates an adapter without holder-resolution capability — `port_holders` must return
        /// `None`, the same answer the engine treats as "fail closed, never guess a pid".
        port_holders_unsupported: bool,
        container_ready: HashMap<String, bool>,
        tailnet_ready: bool,
    }
    struct FakeProbeAdapter {
        state: Mutex<FakeProbeAdapterState>,
        /// The fake process table, shared with `FakeProcessAdapter` when built `with_process` —
        /// a holder port reads "in use" exactly while its holder pids are still in that table, so
        /// a reclaim's `signal_pid` frees the port just like a real squatter dying would.
        process_state: Arc<Mutex<FakeProcessAdapterState>>,
    }
    impl FakeProbeAdapter {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                state: Mutex::new(FakeProbeAdapterState::default()),
                process_state: Arc::new(Mutex::new(FakeProcessAdapterState::default())),
            })
        }
        fn with_process(process_state: Arc<Mutex<FakeProcessAdapterState>>) -> Arc<Self> {
            Arc::new(Self {
                state: Mutex::new(FakeProbeAdapterState::default()),
                process_state,
            })
        }
        fn set_tcp_ready(&self, port: u16, ready: bool) {
            self.state.lock().unwrap().tcp_ready.insert(port, ready);
        }
        fn set_port_in_use(&self, port: u16, in_use: bool) {
            self.state.lock().unwrap().port_in_use.insert(port, in_use);
        }
        fn set_port_holders_unsupported(&self) {
            self.state.lock().unwrap().port_holders_unsupported = true;
        }
        /// Registers a squatter holding `port`: the holder pid goes into the shared fake process
        /// table (so `signal_pid` can kill it) and into the port's holder list (so `port_holders`
        /// resolves it and `port_in_use` stays true until it dies).
        fn set_port_holder(&self, port: u16, pid: i64, start_identity: &str, command: &str) {
            self.process_state.lock().unwrap().alive.insert(
                pid,
                PosixProcessRecord {
                    pid,
                    pgid: pid,
                    start_identity: start_identity.to_string(),
                    command_fingerprint: command.to_string(),
                },
            );
            self.state
                .lock()
                .unwrap()
                .port_holders
                .entry(port)
                .or_default()
                .push(PortHolder {
                    pid,
                    pgid: pid,
                    start_identity: start_identity.to_string(),
                    command: command.to_string(),
                });
        }
        fn set_container_ready(&self, name: &str, ready: bool) {
            self.state
                .lock()
                .unwrap()
                .container_ready
                .insert(name.to_string(), ready);
        }
    }
    #[async_trait]
    impl ProbeAdapter for FakeProbeAdapter {
        async fn tcp(&self, port: u16) -> bool {
            self.state
                .lock()
                .unwrap()
                .tcp_ready
                .get(&port)
                .copied()
                .unwrap_or(false)
        }
        async fn http(&self, _url: &str) -> bool {
            false
        }
        async fn container(&self, container_name: &str) -> bool {
            self.state
                .lock()
                .unwrap()
                .container_ready
                .get(container_name)
                .copied()
                .unwrap_or(false)
        }
        async fn tailnet(&self) -> bool {
            self.state.lock().unwrap().tailnet_ready
        }
        async fn port_in_use(&self, port: u16) -> Option<bool> {
            let state = self.state.lock().unwrap();
            if let Some(holders) = state.port_holders.get(&port) {
                let alive = self.process_state.lock().unwrap();
                return Some(holders.iter().any(|h| alive.alive.contains_key(&h.pid)));
            }
            Some(state.port_in_use.get(&port).copied().unwrap_or(false))
        }
        async fn port_holders(&self, port: u16) -> Option<Vec<PortHolder>> {
            let state = self.state.lock().unwrap();
            if state.port_holders_unsupported {
                return None;
            }
            let alive = self.process_state.lock().unwrap();
            Some(
                state
                    .port_holders
                    .get(&port)
                    .cloned()
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|h| alive.alive.contains_key(&h.pid))
                    .collect(),
            )
        }
        // Deliberately NOT overriding `command` for the "no adapter configured" tests — the trait
        // default (`None`) is exactly the sharp edge under test. A dedicated adapter type below
        // (`CommandCapableProbeAdapter`) is used for tests that need a real command-probe result.
    }

    #[derive(Default)]
    struct CommandCapableProbeAdapter {
        inner: Option<Arc<FakeProbeAdapter>>,
        result: std::sync::Mutex<Option<bool>>,
        // Only used by the preparation-command serialization test below — a real delay (instead of
        // an instant return) is what makes two concurrent calls sharing a `serializationKey`
        // observably overlap if the engine's serialization ever regressed.
        delay_ms: u64,
        timeline: std::sync::Mutex<Vec<std::time::Instant>>,
    }
    #[async_trait]
    impl ProbeAdapter for CommandCapableProbeAdapter {
        async fn tcp(&self, port: u16) -> bool {
            match &self.inner {
                Some(inner) => inner.tcp(port).await,
                None => false,
            }
        }
        async fn http(&self, url: &str) -> bool {
            match &self.inner {
                Some(inner) => inner.http(url).await,
                None => false,
            }
        }
        async fn container(&self, name: &str) -> bool {
            match &self.inner {
                Some(inner) => inner.container(name).await,
                None => false,
            }
        }
        async fn tailnet(&self) -> bool {
            match &self.inner {
                Some(inner) => inner.tailnet().await,
                None => false,
            }
        }
        async fn port_in_use(&self, port: u16) -> Option<bool> {
            match &self.inner {
                Some(inner) => inner.port_in_use(port).await,
                None => None,
            }
        }
        async fn port_holders(&self, port: u16) -> Option<Vec<PortHolder>> {
            match &self.inner {
                Some(inner) => inner.port_holders(port).await,
                None => None,
            }
        }
        async fn command(&self, _command: &CommandSpec, _cwd: Option<&str>) -> Option<bool> {
            self.timeline
                .lock()
                .unwrap()
                .push(std::time::Instant::now());
            if self.delay_ms > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(self.delay_ms)).await;
            }
            *self.result.lock().unwrap()
        }
    }

    // --- Fake run_build ---
    struct FakeRunBuild {
        should_fail: Mutex<std::collections::HashSet<String>>,
        started_at: Mutex<Vec<(String, std::time::Instant)>>,
        hang_until_cancelled: Mutex<bool>,
    }
    impl FakeRunBuild {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                should_fail: Mutex::new(Default::default()),
                started_at: Mutex::new(Vec::new()),
                hang_until_cancelled: Mutex::new(false),
            })
        }
        fn fail_for(&self, shell_text: &str) {
            self.should_fail
                .lock()
                .unwrap()
                .insert(shell_text.to_string());
        }
        fn timeline(&self) -> Vec<(String, std::time::Instant)> {
            self.started_at.lock().unwrap().clone()
        }
    }
    #[async_trait]
    impl RunBuild for FakeRunBuild {
        async fn run(
            &self,
            command: &ServiceCommand,
            _on_output: OnOutput,
            cancel: CancellationToken,
        ) -> Result<(), SupervisorError> {
            let text = match &command.command {
                CommandSpec::Shell { shell, .. } => shell.clone(),
                CommandSpec::Argv { argv } => argv.join(" "),
            };
            self.started_at
                .lock()
                .unwrap()
                .push((text.clone(), std::time::Instant::now()));
            if *self.hang_until_cancelled.lock().unwrap() {
                cancel.cancelled().await;
                return Err(SupervisorError("Build cancelled".to_string()));
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            if self.should_fail.lock().unwrap().contains(&text) {
                return Err(SupervisorError("Build failed".to_string()));
            }
            Ok(())
        }
    }

    struct NoPreparation;
    #[async_trait]
    impl PreparationAdapter for NoPreparation {
        async fn prepare(
            &self,
            _service_id: &ServiceId,
            _steps: &[String],
        ) -> Result<(), SupervisorError> {
            Ok(())
        }
    }

    // --- Fake host ---
    struct FakeHost {
        instance_id: String,
        catalog: Mutex<Arc<ServiceCatalog>>,
        states: Mutex<HashMap<ServiceId, ServiceLifecycleState>>,
        logs: Mutex<Vec<(String, String)>>,
        events: Mutex<Vec<(String, serde_json::Value)>>,
    }
    impl FakeHost {
        fn new(catalog: ServiceCatalog) -> Arc<Self> {
            Arc::new(Self {
                instance_id: "instance-1".to_string(),
                catalog: Mutex::new(Arc::new(catalog)),
                states: Mutex::new(HashMap::new()),
                logs: Mutex::new(Vec::new()),
                events: Mutex::new(Vec::new()),
            })
        }
        fn state_of(&self, service_id: &str) -> Option<ServiceLifecycleState> {
            self.states.lock().unwrap().get(service_id).cloned()
        }
        /// Seeds a persisted state directly (bypassing the supervisor) — mirrors a test fixture
        /// asserting behavior against a pre-existing `state.json`-style entry.
        fn seed(&self, state: ServiceLifecycleState) {
            self.states
                .lock()
                .unwrap()
                .insert(state.service_id.clone(), state);
        }
    }
    #[async_trait]
    impl Host for FakeHost {
        fn instance_id(&self) -> String {
            self.instance_id.clone()
        }
        fn catalog(&self) -> Arc<ServiceCatalog> {
            self.catalog.lock().unwrap().clone()
        }
        fn service_states(&self) -> Vec<ServiceLifecycleState> {
            let states = self.states.lock().unwrap();
            self.catalog
                .lock()
                .unwrap()
                .services
                .iter()
                .map(|s| {
                    states
                        .get(&s.id)
                        .cloned()
                        .unwrap_or_else(|| default_stopped_state(&s.id))
                })
                .collect()
        }
        async fn set_service_state(&self, next: ServiceLifecycleState) {
            self.states
                .lock()
                .unwrap()
                .insert(next.service_id.clone(), next);
        }
        async fn append_log(&self, service_id: &str, data: &str) {
            self.logs
                .lock()
                .unwrap()
                .push((service_id.to_string(), data.to_string()));
        }
        fn publish(&self, event_type: &str, data: serde_json::Value) {
            self.events
                .lock()
                .unwrap()
                .push((event_type.to_string(), data));
        }
    }

    fn default_stopped_state(service_id: &str) -> ServiceLifecycleState {
        let now = "2024-01-01T00:00:00.000Z".to_string();
        ServiceLifecycleState {
            service_id: service_id.to_string(),
            desired_state: DesiredServiceState::Stopped,
            actual_state: ActualServiceState::Stopped,
            readiness: ServiceReadiness::Unknown,
            generation: 0,
            identity: None,
            readiness_kind: None,
            readiness_detail: None,
            created_at: now.clone(),
            updated_at: now,
            exited_at: None,
            exit_code: None,
            error: None,
            current_operation_id: None,
        }
    }

    // --- Catalog builders ---
    fn argv_verified(id: &str, readiness: ReadinessSpec) -> ServiceDefinition {
        ServiceDefinition {
            id: id.to_string(),
            label: None,
            kind: Some(ServiceKind::Application),
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
                    readiness,
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

    fn with_preparation_command(
        mut service: ServiceDefinition,
        prep: crate::catalog::PreparationCommand,
    ) -> ServiceDefinition {
        if let ServiceRunProfile::Verified {
            preparation_command,
            ..
        } = &mut service.profiles.run
        {
            *preparation_command = Some(prep);
        }
        service
    }

    fn one_service_catalog(service: ServiceDefinition) -> ServiceCatalog {
        ServiceCatalog {
            services: vec![service],
            groups: HashMap::new(),
            group_tree: Vec::new(),
            compose_file: None,
            runtime_directory: None,
            start_failure_policy: StartFailurePolicy::StopOnFirstFailureKeepStarted,
            private_file_guard: None,
        }
    }

    struct Harness {
        supervisor: Arc<ProcessSupervisor>,
        host: Arc<FakeHost>,
        process: Arc<FakeProcessAdapter>,
        probes: Arc<FakeProbeAdapter>,
        run_build: Arc<FakeRunBuild>,
        #[allow(dead_code)]
        // available for tests that need to fast-forward the virtual clock directly
        clock: Arc<FakeClock>,
    }

    fn build_harness(catalog: ServiceCatalog) -> Harness {
        build_harness_with_timeout(catalog, 200)
    }

    fn build_harness_with_timeout(catalog: ServiceCatalog, readiness_timeout_ms: i64) -> Harness {
        let host = FakeHost::new(catalog);
        let process = FakeProcessAdapter::new();
        let probes = FakeProbeAdapter::with_process(process.state.clone());
        let run_build = FakeRunBuild::new();
        let clock = FakeClock::new();
        let options = SupervisorOptions {
            process: process.clone(),
            run_build: run_build.clone(),
            probes: probes.clone(),
            preparation: Some(Arc::new(NoPreparation)),
            artifact_installer: None,
            clock: clock.clone(),
            readiness_timeout_ms,
            readiness_backoff_ms: 5,
            termination_grace_ms: 50,
            is_closing: Arc::new(|| false),
        };
        let supervisor = ProcessSupervisor::new(host.clone(), options);
        Harness {
            supervisor,
            host,
            process,
            probes,
            run_build,
            clock,
        }
    }

    #[tokio::test]
    async fn starts_a_stopped_service_to_ready_with_tcp_readiness() {
        let h = build_harness(one_service_catalog(argv_verified(
            "api",
            ReadinessSpec::Tcp { port: 8080 },
        )));
        h.probes.set_tcp_ready(8080, true);
        h.supervisor.start(&"api".to_string(), None).await.unwrap();
        let state = h.host.state_of("api").unwrap();
        assert_eq!(state.actual_state, ActualServiceState::Ready);
        assert_eq!(state.readiness, ServiceReadiness::Ready);
        assert!(state.identity.is_some());
    }

    #[tokio::test]
    async fn process_readiness_reports_running_unready_as_the_terminal_state() {
        let h = build_harness(one_service_catalog(argv_verified(
            "api",
            ReadinessSpec::Process,
        )));
        h.supervisor.start(&"api".to_string(), None).await.unwrap();
        let state = h.host.state_of("api").unwrap();
        assert_eq!(state.actual_state, ActualServiceState::RunningUnready);
        assert_eq!(
            state.readiness_detail.as_deref(),
            Some("process-liveness-only")
        );
    }

    #[tokio::test]
    async fn restart_stops_then_starts() {
        let h = build_harness(one_service_catalog(argv_verified(
            "api",
            ReadinessSpec::Tcp { port: 8080 },
        )));
        h.probes.set_tcp_ready(8080, true);
        h.supervisor.start(&"api".to_string(), None).await.unwrap();
        let first_pid = match h.host.state_of("api").unwrap().identity.unwrap() {
            ProcessIdentity::Posix(p) => p.pid,
            _ => panic!("expected posix identity"),
        };
        h.supervisor
            .restart(&"api".to_string(), None)
            .await
            .unwrap();
        let second_pid = match h.host.state_of("api").unwrap().identity.unwrap() {
            ProcessIdentity::Posix(p) => p.pid,
            _ => panic!("expected posix identity"),
        };
        assert_ne!(
            first_pid, second_pid,
            "restart should spawn a fresh process"
        );
        assert!(h
            .process
            .signals()
            .iter()
            .any(|(pgid, sig)| *pgid == first_pid && *sig == ProcessSignal::Sigterm));
    }

    /// The whole-tree stop rule: `air` runs the real server in its own process group, so a stop that
    /// signals only the tracked pgid leaves it alive holding the port. The secondary group's
    /// liveness check once parsed a 3-column `ps` row with the 4-column tree regex, never matched,
    /// and so never signalled that group at all.
    #[tokio::test]
    async fn stop_signals_a_child_that_moved_into_its_own_process_group() {
        let h = build_harness(one_service_catalog(argv_verified(
            "api",
            ReadinessSpec::Tcp { port: 8080 },
        )));
        h.probes.set_tcp_ready(8080, true);
        h.supervisor.start(&"api".to_string(), None).await.unwrap();
        let leader = match h.host.state_of("api").unwrap().identity.unwrap() {
            ProcessIdentity::Posix(p) => p.pid,
            _ => panic!("expected posix identity"),
        };
        let child = h.process.fork_into_own_group(leader);

        h.supervisor.stop(&"api".to_string(), None).await.unwrap();

        assert!(h
            .process
            .signals()
            .contains(&(leader, ProcessSignal::Sigterm)));
        assert!(
            h.process
                .signals()
                .contains(&(child, ProcessSignal::Sigterm)),
            "the secondary group must be signalled: {:?}",
            h.process.signals()
        );
        assert!(
            !h.process.is_alive(child),
            "the child in its own process group must not survive the stop"
        );
        assert_eq!(
            h.host.state_of("api").unwrap().actual_state,
            ActualServiceState::Stopped
        );
    }

    #[tokio::test]
    async fn status_upgrades_a_recovered_service_back_to_ready() {
        let h = build_harness(one_service_catalog(argv_verified(
            "api",
            ReadinessSpec::Tcp { port: 8080 },
        )));
        h.probes.set_tcp_ready(8080, true);
        h.supervisor.start(&"api".to_string(), None).await.unwrap();

        h.probes.set_tcp_ready(8080, false);
        h.supervisor.status(&"api".to_string()).await.unwrap();
        assert_eq!(
            h.host.state_of("api").unwrap().actual_state,
            ActualServiceState::RunningUnready
        );

        h.probes.set_tcp_ready(8080, true);
        h.supervisor.status(&"api".to_string()).await.unwrap();
        let state = h.host.state_of("api").unwrap();
        assert_eq!(state.actual_state, ActualServiceState::Ready);
        assert_eq!(state.readiness, ServiceReadiness::Ready);
    }

    #[tokio::test]
    async fn transitions_leave_the_single_lifecycle_publish_to_the_host() {
        let h = build_harness(one_service_catalog(argv_verified(
            "api",
            ReadinessSpec::Tcp { port: 8080 },
        )));
        h.probes.set_tcp_ready(8080, true);
        h.supervisor.start(&"api".to_string(), None).await.unwrap();
        let published = h
            .host
            .events
            .lock()
            .unwrap()
            .iter()
            .filter(|(event_type, _)| event_type == "service.lifecycle")
            .count();
        assert_eq!(
            published, 0,
            "`Host::set_service_state` owns the service.lifecycle event"
        );
    }

    #[tokio::test]
    async fn stop_on_an_already_stopped_service_is_a_noop() {
        let h = build_harness(one_service_catalog(argv_verified(
            "api",
            ReadinessSpec::Tcp { port: 8080 },
        )));
        h.supervisor.stop(&"api".to_string(), None).await.unwrap();
        // A true no-op: `stop_locked` returns immediately for an already-stopped service without
        // ever calling `set_service_state`, so nothing is written to the host's state map at all —
        // check the synthesized default via `service_states()` instead of the raw map.
        let state = h
            .host
            .service_states()
            .into_iter()
            .find(|s| s.service_id == "api")
            .unwrap();
        assert_eq!(state.actual_state, ActualServiceState::Stopped);
        assert!(
            h.host.state_of("api").is_none(),
            "a true no-op must never write to persisted state"
        );
        assert!(h.process.signals().is_empty());
    }

    /// An adopted `ownership: external` unit carries no process identity, so the catalog's `stop:`
    /// command is the only thing that can stop it. Stop used to fall through to `orphan()` —
    /// recording "Process ownership identity no longer matches" for a service that never had an
    /// identity — while the container kept running: exactly what "I press Stop and nothing happens"
    /// looked like in the GUI.
    #[tokio::test]
    async fn stop_on_an_adopted_external_service_runs_its_stop_command() {
        let h = build_harness(one_service_catalog(external_container_service(
            "cache",
            "proj-cache",
            Some("docker compose stop cache"),
        )));
        h.host.seed(adopted_external_state("cache"));

        h.supervisor.stop(&"cache".to_string(), None).await.unwrap();

        assert_eq!(
            h.process.stop_container_calls(),
            vec!["proj-cache".to_string()],
            "the catalog's stop command must actually run"
        );
        let state = h.host.state_of("cache").unwrap();
        assert_eq!(state.actual_state, ActualServiceState::Stopped);
        assert_eq!(state.desired_state, DesiredServiceState::Stopped);
        assert!(
            state.error.is_none(),
            "a successful stop must not leave the old error behind"
        );
    }

    /// The other half of the same bug: a service whose port is held by a process this manager does
    /// not own has nothing to stop and no `stop:` command to run. Stop must say so — it used to
    /// report success and change nothing.
    #[tokio::test]
    async fn stop_on_an_externally_owned_service_without_a_stop_command_fails_loudly() {
        let h = build_harness(one_service_catalog(argv_verified(
            "api",
            ReadinessSpec::Tcp { port: 8080 },
        )));
        h.probes.set_port_in_use(8080, true);
        let start_error = h
            .supervisor
            .start(&"api".to_string(), None)
            .await
            .unwrap_err();
        assert!(
            start_error.0.contains("externally owned"),
            "{start_error:?}"
        );

        let error = h
            .supervisor
            .stop(&"api".to_string(), None)
            .await
            .unwrap_err();

        assert!(error.0.contains("cannot be stopped"), "{error:?}");
        assert!(
            error.0.contains("Port 8080 is held by an unowned process"),
            "{error:?}"
        );
        assert!(error.0.contains("no `stop` command"), "{error:?}");
        let state = h.host.state_of("api").unwrap();
        assert_eq!(
            state.actual_state,
            ActualServiceState::ExternallyOwned,
            "a refused stop must not pretend the service is stopped"
        );
    }

    /// An external unit with no `stop:` command cannot be stopped by this manager either — the same
    /// loud refusal, not a silent success that leaves the container running.
    #[tokio::test]
    async fn stop_on_an_external_service_without_a_stop_command_fails_loudly() {
        let h = build_harness(one_service_catalog(external_container_service(
            "cache",
            "proj-cache",
            None,
        )));
        h.host.seed(adopted_external_state("cache"));

        let error = h
            .supervisor
            .stop(&"cache".to_string(), None)
            .await
            .unwrap_err();

        assert!(error.0.contains("cannot be stopped"), "{error:?}");
        assert!(error.0.contains("owned externally"), "{error:?}");
        assert!(
            h.process.stop_container_calls().is_empty(),
            "nothing may be run when the catalog declares no stop command"
        );
        assert_eq!(
            h.host.state_of("cache").unwrap().actual_state,
            ActualServiceState::Ready
        );
    }

    /// A stop must never report success for a process it cannot touch: an identity that no longer
    /// matches (a reused pid, or a program that replaced it) is alive, but killing it would be
    /// killing someone else's process.
    #[tokio::test]
    async fn stop_on_an_orphaned_service_refuses_instead_of_reporting_success() {
        let h = build_harness(one_service_catalog(argv_verified(
            "api",
            ReadinessSpec::Tcp { port: 8080 },
        )));
        h.probes.set_tcp_ready(8080, true);
        h.supervisor.start(&"api".to_string(), None).await.unwrap();
        let pid = match h.host.state_of("api").unwrap().identity.unwrap() {
            ProcessIdentity::Posix(posix) => posix.pid,
            _ => panic!("expected a posix identity"),
        };
        // The pid is still alive, but `ps` now reports a different program at it.
        h.process
            .mutate_fingerprint(pid, "not-the-same-program-anymore");

        let error = h
            .supervisor
            .stop(&"api".to_string(), None)
            .await
            .unwrap_err();

        assert!(error.0.contains("cannot be stopped"), "{error:?}");
        assert!(error.0.contains(&format!("pid {pid}")), "{error:?}");
        assert!(
            h.process.signals().is_empty(),
            "a process this manager does not own must never be signalled"
        );
        assert_eq!(
            h.host.state_of("api").unwrap().actual_state,
            ActualServiceState::Orphaned
        );
    }

    #[tokio::test]
    async fn readiness_timeout_transitions_to_failed_with_detail() {
        let h = build_harness_with_timeout(
            one_service_catalog(argv_verified("api", ReadinessSpec::Tcp { port: 8080 })),
            30,
        );
        // tcp never reports ready — probes.set_tcp_ready is never called for 8080.
        let err = h
            .supervisor
            .start(&"api".to_string(), None)
            .await
            .unwrap_err();
        assert_eq!(err.0, "Readiness timed out");
        let state = h.host.state_of("api").unwrap();
        assert_eq!(state.actual_state, ActualServiceState::Failed);
        assert!(state
            .readiness_detail
            .as_deref()
            .unwrap()
            .contains("timed out"));
    }

    #[tokio::test]
    async fn tcp_port_conflict_refuses_to_start() {
        let h = build_harness(one_service_catalog(argv_verified(
            "api",
            ReadinessSpec::Tcp { port: 8080 },
        )));
        h.probes.set_port_in_use(8080, true);
        let err = h
            .supervisor
            .start(&"api".to_string(), None)
            .await
            .unwrap_err();
        assert!(err.0.contains("externally owned"));
        let state = h.host.state_of("api").unwrap();
        assert_eq!(state.actual_state, ActualServiceState::ExternallyOwned);
        assert!(state
            .error
            .as_deref()
            .unwrap()
            .contains("Port 8080 is held"));
        assert!(
            h.process.signals().is_empty(),
            "must never have spawned anything"
        );
    }

    /// The refusal error must name the squatter — `pid (command)` — so the client can show the
    /// user exactly what would be killed before they confirm.
    #[tokio::test]
    async fn tcp_port_conflict_names_the_holder_in_the_error() {
        let h = build_harness(one_service_catalog(argv_verified(
            "api",
            ReadinessSpec::Tcp { port: 8080 },
        )));
        h.probes
            .set_port_holder(8080, 41_234, "Fake Jan  1 00:00:01 2024", "node dist/main");
        let err = h
            .supervisor
            .start(&"api".to_string(), None)
            .await
            .unwrap_err();
        assert!(err.0.contains("externally owned"));
        let state = h.host.state_of("api").unwrap();
        assert_eq!(state.actual_state, ActualServiceState::ExternallyOwned);
        assert_eq!(
            state.error.as_deref().unwrap(),
            "Port 8080 is held by pid 41234 (node dist/main)"
        );
        assert!(
            h.process.pid_signals().is_empty(),
            "without kill_unowned nothing may be signalled"
        );
    }

    /// `killUnowned` is the client's echo of the user's "yes": the squatter gets SIGTERM, the port
    /// frees, and the start continues into the normal spawn/readiness path.
    #[tokio::test]
    async fn start_with_kill_unowned_terminates_the_holder_and_continues() {
        let h = build_harness(one_service_catalog(argv_verified(
            "api",
            ReadinessSpec::Tcp { port: 8080 },
        )));
        h.probes
            .set_port_holder(8080, 41_234, "Fake Jan  1 00:00:01 2024", "node dist/main");
        h.probes.set_tcp_ready(8080, true);
        h.supervisor
            .start_with_options(
                &"api".to_string(),
                None,
                StartOptions { kill_unowned: true },
            )
            .await
            .unwrap();
        assert_eq!(
            h.process.pid_signals(),
            vec![(41_234, ProcessSignal::Sigterm)]
        );
        assert!(
            !h.process.state.lock().unwrap().alive.contains_key(&41_234),
            "the squatter must be dead"
        );
        let state = h.host.state_of("api").unwrap();
        assert_eq!(state.actual_state, ActualServiceState::Ready);
        assert!(
            state.identity.is_some(),
            "the service itself must have spawned after the port freed"
        );
    }

    /// A squatter that ignores SIGTERM is escalated to SIGKILL on a second pass — holders are
    /// re-resolved, so the kill goes to the still-alive pid, not the stale first-pass record.
    #[tokio::test]
    async fn reclaim_escalates_to_sigkill_for_a_sigterm_immune_holder() {
        let h = build_harness(one_service_catalog(argv_verified(
            "api",
            ReadinessSpec::Tcp { port: 8080 },
        )));
        h.probes
            .set_port_holder(8080, 41_234, "Fake Jan  1 00:00:01 2024", "node dist/main");
        h.probes.set_tcp_ready(8080, true);
        h.process.mark_term_immune(41_234);
        h.supervisor
            .start_with_options(
                &"api".to_string(),
                None,
                StartOptions { kill_unowned: true },
            )
            .await
            .unwrap();
        assert_eq!(
            h.process.pid_signals(),
            vec![
                (41_234, ProcessSignal::Sigterm),
                (41_234, ProcessSignal::Sigkill)
            ]
        );
        assert_eq!(
            h.host.state_of("api").unwrap().actual_state,
            ActualServiceState::Ready
        );
    }

    /// If the holder still owns the port after SIGKILL the start must fail loudly — the service
    /// stays `externally-owned` instead of silently dropping the user's confirmed kill.
    #[tokio::test]
    async fn reclaim_fails_loudly_when_the_holder_survives_sigkill() {
        let h = build_harness(one_service_catalog(argv_verified(
            "api",
            ReadinessSpec::Tcp { port: 8080 },
        )));
        h.probes
            .set_port_holder(8080, 41_234, "Fake Jan  1 00:00:01 2024", "node dist/main");
        h.process.mark_unkillable(41_234);
        let err = h
            .supervisor
            .start_with_options(
                &"api".to_string(),
                None,
                StartOptions { kill_unowned: true },
            )
            .await
            .unwrap_err();
        assert!(err.0.contains("still held"), "{err:?}");
        let state = h.host.state_of("api").unwrap();
        assert_eq!(state.actual_state, ActualServiceState::ExternallyOwned);
        assert!(
            state.identity.is_none(),
            "the service must never have spawned"
        );
    }

    /// A pid that no longer presents the resolved lstart is not ours to kill — `signal_pid`
    /// re-verifies the identity before signalling, so a recycled pid survives reclaim untouched
    /// and the start fails closed.
    #[tokio::test]
    async fn reclaim_never_kills_a_holder_whose_start_identity_no_longer_matches() {
        let h = build_harness(one_service_catalog(argv_verified(
            "api",
            ReadinessSpec::Tcp { port: 8080 },
        )));
        h.probes
            .set_port_holder(8080, 41_234, "original lstart", "node dist/main");
        // The squatter died and its pid was recycled by something else before the signal landed.
        h.probes
            .process_state
            .lock()
            .unwrap()
            .alive
            .get_mut(&41_234)
            .unwrap()
            .start_identity = "recycled lstart".to_string();
        let err = h
            .supervisor
            .start_with_options(
                &"api".to_string(),
                None,
                StartOptions { kill_unowned: true },
            )
            .await
            .unwrap_err();
        assert!(err.0.contains("still held"), "{err:?}");
        assert!(h
            .process
            .pid_signals()
            .iter()
            .all(|(pid, _)| *pid == 41_234));
        assert!(
            h.probes
                .process_state
                .lock()
                .unwrap()
                .alive
                .contains_key(&41_234),
            "a recycled pid must survive reclaim"
        );
    }

    #[tokio::test]
    async fn reclaim_terminates_every_holder_of_the_port() {
        let h = build_harness(one_service_catalog(argv_verified(
            "api",
            ReadinessSpec::Tcp { port: 8080 },
        )));
        h.probes
            .set_port_holder(8080, 41_234, "Fake Jan  1 00:00:01 2024", "node a");
        h.probes
            .set_port_holder(8080, 41_235, "Fake Jan  1 00:00:02 2024", "node b");
        h.probes.set_tcp_ready(8080, true);
        h.supervisor
            .start_with_options(
                &"api".to_string(),
                None,
                StartOptions { kill_unowned: true },
            )
            .await
            .unwrap();
        assert_eq!(
            h.process.pid_signals(),
            vec![
                (41_234, ProcessSignal::Sigterm),
                (41_235, ProcessSignal::Sigterm)
            ]
        );
        assert_eq!(
            h.host.state_of("api").unwrap().actual_state,
            ActualServiceState::Ready
        );
    }

    /// `port_holders` returning `None` means the adapter cannot resolve who holds the port —
    /// reclaim must fail closed rather than signalling a pid it guessed.
    #[tokio::test]
    async fn reclaim_fails_closed_when_the_probe_cannot_resolve_holders() {
        let h = build_harness(one_service_catalog(argv_verified(
            "api",
            ReadinessSpec::Tcp { port: 8080 },
        )));
        h.probes.set_port_in_use(8080, true);
        h.probes.set_port_holders_unsupported();
        let err = h
            .supervisor
            .start_with_options(
                &"api".to_string(),
                None,
                StartOptions { kill_unowned: true },
            )
            .await
            .unwrap_err();
        assert!(
            err.0.contains("still held by an unowned process"),
            "{err:?}"
        );
        assert!(
            h.process.pid_signals().is_empty(),
            "an unresolvable holder must never be signalled"
        );
        assert_eq!(
            h.host.state_of("api").unwrap().actual_state,
            ActualServiceState::ExternallyOwned
        );
    }

    /// Restart always uses default options — it can end up `externally-owned`, but it must never
    /// signal a squatter. Reclaim only ever comes from an explicit `killUnowned` start.
    #[tokio::test]
    async fn restart_never_reclaims_an_occupied_port() {
        let h = build_harness(one_service_catalog(argv_verified(
            "api",
            ReadinessSpec::Tcp { port: 8080 },
        )));
        h.probes
            .set_port_holder(8080, 41_234, "Fake Jan  1 00:00:01 2024", "node dist/main");
        let err = h
            .supervisor
            .restart(&"api".to_string(), None)
            .await
            .unwrap_err();
        assert!(err.0.contains("externally owned"), "{err:?}");
        assert!(h.process.pid_signals().is_empty());
        assert_eq!(
            h.host.state_of("api").unwrap().actual_state,
            ActualServiceState::ExternallyOwned
        );
    }

    #[tokio::test]
    async fn shutdown_reaps_a_persisted_identity_in_a_non_active_state() {
        let h = build_harness(one_service_catalog(argv_verified(
            "api",
            ReadinessSpec::Tcp { port: 8080 },
        )));
        // Simulate: a prior start spawned pid 900001 (still alive per the process adapter), but the
        // service's *current* actualState is "externally-owned" (e.g. a port conflict was detected
        // on the most recent start attempt) — not one of the "active" states shutdown's first pass
        // stops. Pass 2 must still reap it.
        let identity = ProcessIdentity::Posix(PosixProcessIdentity {
            manager_instance_id: h.host.instance_id(),
            service_id: "api".to_string(),
            generation: 1,
            pid: 900_001,
            pgid: 900_001,
            started_at: "2024-01-01T00:00:00.000Z".to_string(),
            start_identity: "Fake Jan  1 00:00:01 2024".to_string(),
            command_fingerprint: normalize_command_fingerprint(&argv_command_for_test("api")),
        });
        // Register the pid as alive in the fake process adapter, matching the identity above.
        {
            let mut state = h.process.state.lock().unwrap();
            state.alive.insert(
                900_001,
                PosixProcessRecord {
                    pid: 900_001,
                    pgid: 900_001,
                    start_identity: "Fake Jan  1 00:00:01 2024".to_string(),
                    command_fingerprint: normalize_command_fingerprint(&argv_command_for_test(
                        "api",
                    )),
                },
            );
        }
        h.host.seed(ServiceLifecycleState {
            service_id: "api".to_string(),
            desired_state: DesiredServiceState::Stopped,
            actual_state: ActualServiceState::ExternallyOwned,
            readiness: ServiceReadiness::Failed,
            generation: 1,
            identity: Some(identity),
            readiness_kind: None,
            readiness_detail: None,
            created_at: "2024-01-01T00:00:00.000Z".to_string(),
            updated_at: "2024-01-01T00:00:00.000Z".to_string(),
            exited_at: None,
            exit_code: None,
            error: Some("Port 8080 is held by an unowned process".to_string()),
            current_operation_id: None,
        });

        h.supervisor.shutdown().await;

        assert!(
            h.process
                .signals()
                .iter()
                .any(|(pgid, sig)| *pgid == 900_001 && *sig == ProcessSignal::Sigterm),
            "shutdown's second pass must reap a persisted identity even in a non-active state"
        );
    }

    fn argv_command_for_test(id: &str) -> ServiceCommand {
        ServiceCommand {
            command: CommandSpec::Argv {
                argv: vec![id.to_string()],
            },
            cwd: ".".to_string(),
            environment: None,
            container_name: None,
            docker_stop_command: None,
        }
    }

    /// An `ownership: external` container unit, shaped like a real one (`viclass`'s mongo/redis/…):
    /// a `docker compose up -d` run command, a container readiness probe, and — optionally — the
    /// `stop:` command that is the only way this manager can stop it.
    fn external_container_service(
        id: &str,
        container: &str,
        stop: Option<&str>,
    ) -> ServiceDefinition {
        let mut service = argv_verified(id, ReadinessSpec::Container);
        service.ownership = Some(ServiceOwnership::External);
        service.profiles.run = ServiceRunProfile::Verified {
            command: ServiceCommand {
                command: CommandSpec::Shell {
                    shell: format!("docker compose up -d {container}"),
                    exec: None,
                },
                cwd: ".".to_string(),
                environment: None,
                container_name: Some(container.to_string()),
                docker_stop_command: stop.map(|shell| CommandSpec::Shell {
                    shell: shell.to_string(),
                    exec: None,
                }),
            },
            readiness: ReadinessSpec::Container,
            readiness_timeout_ms: None,
            preparation: None,
            preparation_command: None,
        };
        service
    }

    /// The state `sync_external_services` leaves behind for an adopted external unit: ready, no
    /// process identity, and a readiness detail saying where that readiness came from.
    fn adopted_external_state(service_id: &str) -> ServiceLifecycleState {
        ServiceLifecycleState {
            service_id: service_id.to_string(),
            desired_state: DesiredServiceState::Running,
            actual_state: ActualServiceState::Ready,
            readiness: ServiceReadiness::Ready,
            generation: 1,
            identity: None,
            readiness_kind: None,
            readiness_detail: Some("adopted from external state".to_string()),
            created_at: "2024-01-01T00:00:00.000Z".to_string(),
            updated_at: "2024-01-01T00:00:00.000Z".to_string(),
            exited_at: None,
            exit_code: None,
            error: None,
            current_operation_id: None,
        }
    }

    #[tokio::test]
    async fn orphans_when_the_observed_fingerprint_no_longer_matches() {
        let h = build_harness(one_service_catalog(argv_verified(
            "api",
            ReadinessSpec::Tcp { port: 8080 },
        )));
        h.probes.set_tcp_ready(8080, true);
        h.supervisor.start(&"api".to_string(), None).await.unwrap();
        let pid = match h.host.state_of("api").unwrap().identity.unwrap() {
            ProcessIdentity::Posix(p) => p.pid,
            _ => panic!(),
        };
        // Simulate a pid-reuse-like scenario: the OS-observed process at this pid now has a
        // different command fingerprint (a different program entirely), so `observed_matches` must
        // fail and the service must be marked orphaned rather than treated as still-ours.
        h.process
            .mutate_fingerprint(pid, "not-the-same-program-anymore");
        h.supervisor.status(&"api".to_string()).await.unwrap();
        let state = h.host.state_of("api").unwrap();
        assert_eq!(state.actual_state, ActualServiceState::Orphaned);
        assert_eq!(
            state.error.as_deref(),
            Some("Process ownership identity no longer matches")
        );
    }

    #[tokio::test]
    async fn build_failure_short_circuits_spawn() {
        let mut service = argv_verified("api", ReadinessSpec::Tcp { port: 8080 });
        service.profiles.build = Some(ServiceBuildProfile {
            command: ServiceCommand {
                command: CommandSpec::Shell {
                    shell: "build-that-fails".to_string(),
                    exec: None,
                },
                cwd: ".".to_string(),
                environment: None,
                container_name: None,
                docker_stop_command: None,
            },
            timeout_ms: None,
            serialization_key: None,
        });
        let h = build_harness(one_service_catalog(service));
        h.run_build.fail_for("build-that-fails");
        h.probes.set_tcp_ready(8080, true);

        let err = h
            .supervisor
            .start(&"api".to_string(), None)
            .await
            .unwrap_err();
        assert_eq!(err.0, "Build failed");
        let state = h.host.state_of("api").unwrap();
        assert_eq!(state.actual_state, ActualServiceState::Failed);
        assert!(
            h.process.signals().is_empty(),
            "spawn must never be attempted after a build failure"
        );
    }

    #[tokio::test]
    async fn build_serialization_by_key_runs_one_at_a_time() {
        fn service_with_build(id: &str, key: &str) -> ServiceDefinition {
            let mut service = argv_verified(id, ReadinessSpec::Tcp { port: 8080 });
            service.profiles.build = Some(ServiceBuildProfile {
                command: ServiceCommand {
                    command: CommandSpec::Shell {
                        shell: format!("build-{id}"),
                        exec: None,
                    },
                    cwd: ".".to_string(),
                    environment: None,
                    container_name: None,
                    docker_stop_command: None,
                },
                timeout_ms: None,
                serialization_key: Some(key.to_string()),
            });
            service
        }
        let catalog = ServiceCatalog {
            services: vec![
                service_with_build("a", "shared"),
                service_with_build("b", "shared"),
            ],
            groups: HashMap::new(),
            group_tree: Vec::new(),
            compose_file: None,
            runtime_directory: None,
            start_failure_policy: StartFailurePolicy::StopOnFirstFailureKeepStarted,
            private_file_guard: None,
        };
        let h = build_harness(catalog);
        h.probes.set_tcp_ready(8080, true);

        let sup_a = h.supervisor.clone();
        let sup_b = h.supervisor.clone();
        let id_a = "a".to_string();
        let id_b = "b".to_string();
        let (ra, rb) = tokio::join!(sup_a.start(&id_a, None), sup_b.start(&id_b, None));
        ra.unwrap();
        rb.unwrap();

        let timeline = h.run_build.timeline();
        assert_eq!(timeline.len(), 2);
        // Each build sleeps 5ms; if they ran concurrently, their start times would be within a
        // couple ms of each other. Serialized, the second must start at or after the first's
        // finish, i.e. at least ~5ms after the first started.
        let gap = timeline[1].1.duration_since(timeline[0].1);
        assert!(
            gap >= std::time::Duration::from_millis(4),
            "builds sharing a serializationKey must not overlap, gap was {gap:?}"
        );
    }

    #[tokio::test]
    async fn command_readiness_with_no_adapter_never_throws_times_out_normally() {
        let readiness = ReadinessSpec::Command {
            command: CommandSpec::Argv {
                argv: vec!["check".to_string()],
            },
            cwd: None,
        };
        let h =
            build_harness_with_timeout(one_service_catalog(argv_verified("api", readiness)), 20);
        let err = h
            .supervisor
            .start(&"api".to_string(), None)
            .await
            .unwrap_err();
        assert_eq!(err.0, "Readiness timed out");
    }

    #[tokio::test]
    async fn command_readiness_succeeds_when_the_adapter_reports_ready() {
        let host = FakeHost::new(one_service_catalog(argv_verified(
            "api",
            ReadinessSpec::Command {
                command: CommandSpec::Argv {
                    argv: vec!["check".to_string()],
                },
                cwd: None,
            },
        )));
        let process = FakeProcessAdapter::new();
        let inner_probes = FakeProbeAdapter::new();
        let probes = Arc::new(CommandCapableProbeAdapter {
            inner: Some(inner_probes),
            result: std::sync::Mutex::new(Some(true)),
            ..Default::default()
        });
        let run_build = FakeRunBuild::new();
        let clock = FakeClock::new();
        let supervisor = ProcessSupervisor::new(
            host.clone(),
            SupervisorOptions {
                process,
                run_build,
                probes,
                preparation: Some(Arc::new(NoPreparation)),
                artifact_installer: None,
                clock,
                readiness_timeout_ms: 200,
                readiness_backoff_ms: 5,
                termination_grace_ms: 50,
                is_closing: Arc::new(|| false),
            },
        );
        supervisor.start(&"api".to_string(), None).await.unwrap();
        assert_eq!(
            host.state_of("api").unwrap().actual_state,
            ActualServiceState::Ready
        );
    }

    // --- artifact installs ---

    struct FakeArtifactInstaller {
        calls: std::sync::Mutex<Vec<String>>,
        result: std::sync::Mutex<Result<(), String>>,
    }

    impl FakeArtifactInstaller {
        fn ok() -> Self {
            Self {
                calls: std::sync::Mutex::new(Vec::new()),
                result: std::sync::Mutex::new(Ok(())),
            }
        }
    }

    #[async_trait]
    impl crate::supervisor::types::ArtifactInstaller for FakeArtifactInstaller {
        async fn install(
            &self,
            service: &ServiceDefinition,
            _on_output: OnOutput,
        ) -> Result<(), SupervisorError> {
            self.calls.lock().unwrap().push(service.id.clone());
            self.result.lock().unwrap().clone().map_err(SupervisorError)
        }
    }

    fn artifact_service(id: &str) -> ServiceDefinition {
        let mut service = argv_verified(id, ReadinessSpec::Process);
        service.artifact = Some(crate::catalog::ServiceArtifact {
            version: "1.0".to_string(),
            url: Some("file:///tmp/x.tgz".to_string()),
            script: None,
            script_args: vec![],
            sha256: Some("0".repeat(64)),
            install_dir: Some("/tmp/x".to_string()),
            data_dir: Some("/tmp/x-data".to_string()),
        });
        service
    }

    fn artifact_harness(
        service: ServiceDefinition,
        installer: Option<Arc<FakeArtifactInstaller>>,
    ) -> (Arc<ProcessSupervisor>, Arc<FakeHost>) {
        let host = FakeHost::new(one_service_catalog(service));
        let process = FakeProcessAdapter::new();
        let probes = FakeProbeAdapter::new();
        let run_build = FakeRunBuild::new();
        let clock = FakeClock::new();
        let supervisor = ProcessSupervisor::new(
            host.clone(),
            SupervisorOptions {
                process,
                run_build,
                probes,
                preparation: Some(Arc::new(NoPreparation)),
                artifact_installer: installer
                    .map(|i| i as Arc<dyn crate::supervisor::types::ArtifactInstaller>),
                clock,
                readiness_timeout_ms: 200,
                readiness_backoff_ms: 5,
                termination_grace_ms: 50,
                is_closing: Arc::new(|| false),
            },
        );
        (supervisor, host)
    }

    #[tokio::test]
    async fn artifact_service_installs_before_starting() {
        let installer = Arc::new(FakeArtifactInstaller::ok());
        let (supervisor, host) = artifact_harness(artifact_service("db"), Some(installer.clone()));
        supervisor.start(&"db".to_string(), None).await.unwrap();
        assert_eq!(
            installer.calls.lock().unwrap().as_slice(),
            &["db".to_string()]
        );
        // `process` readiness ends the start at running-unready — the proof we want is that the
        // install ran before spawn and nothing failed.
        assert_eq!(
            host.state_of("db").unwrap().actual_state,
            ActualServiceState::RunningUnready
        );
    }

    #[tokio::test]
    async fn artifact_install_failure_fails_the_start() {
        let installer = Arc::new(FakeArtifactInstaller::ok());
        *installer.result.lock().unwrap() = Err("sha256 mismatch".to_string());
        let (supervisor, host) = artifact_harness(artifact_service("db"), Some(installer));
        supervisor.start(&"db".to_string(), None).await.unwrap_err();
        let state = host.state_of("db").unwrap();
        assert_eq!(state.actual_state, ActualServiceState::Failed);
        assert_eq!(
            state.error.as_deref(),
            Some("Install failed: sha256 mismatch")
        );
    }

    #[tokio::test]
    async fn artifact_without_an_installer_fails_clearly() {
        let (supervisor, host) = artifact_harness(artifact_service("db"), None);
        supervisor.start(&"db".to_string(), None).await.unwrap_err();
        let state = host.state_of("db").unwrap();
        assert_eq!(state.actual_state, ActualServiceState::Failed);
        assert!(state.error.as_deref().unwrap().contains("not supported"));
    }

    fn preparation_catalog() -> ServiceCatalog {
        let service = with_preparation_command(
            argv_verified("api", ReadinessSpec::Tcp { port: 8080 }),
            crate::catalog::PreparationCommand {
                command: CommandSpec::Argv {
                    argv: vec!["prepare".to_string()],
                },
                cwd: Some("infra".to_string()),
                serialization_key: None,
            },
        );
        one_service_catalog(service)
    }

    #[tokio::test]
    async fn preparation_command_runs_before_start_and_succeeds_when_the_probe_reports_ready() {
        let host = FakeHost::new(preparation_catalog());
        let process = FakeProcessAdapter::new();
        let inner_probes = FakeProbeAdapter::new();
        inner_probes.set_tcp_ready(8080, true);
        let probes = Arc::new(CommandCapableProbeAdapter {
            inner: Some(inner_probes),
            result: std::sync::Mutex::new(Some(true)),
            ..Default::default()
        });
        let run_build = FakeRunBuild::new();
        let clock = FakeClock::new();
        let supervisor = ProcessSupervisor::new(
            host.clone(),
            SupervisorOptions {
                process,
                run_build,
                probes,
                preparation: Some(Arc::new(NoPreparation)),
                artifact_installer: None,
                clock,
                readiness_timeout_ms: 200,
                readiness_backoff_ms: 5,
                termination_grace_ms: 50,
                is_closing: Arc::new(|| false),
            },
        );
        supervisor.start(&"api".to_string(), None).await.unwrap();
        assert_eq!(
            host.state_of("api").unwrap().actual_state,
            ActualServiceState::Ready
        );
    }

    #[tokio::test]
    async fn preparation_command_failure_fails_the_start_not_just_readiness() {
        let host = FakeHost::new(preparation_catalog());
        let process = FakeProcessAdapter::new();
        let inner_probes = FakeProbeAdapter::new();
        let probes = Arc::new(CommandCapableProbeAdapter {
            inner: Some(inner_probes),
            result: std::sync::Mutex::new(Some(false)),
            ..Default::default()
        });
        let run_build = FakeRunBuild::new();
        let clock = FakeClock::new();
        let supervisor = ProcessSupervisor::new(
            host.clone(),
            SupervisorOptions {
                process,
                run_build,
                probes,
                preparation: Some(Arc::new(NoPreparation)),
                artifact_installer: None,
                clock,
                readiness_timeout_ms: 200,
                readiness_backoff_ms: 5,
                termination_grace_ms: 50,
                is_closing: Arc::new(|| false),
            },
        );
        let err = supervisor
            .start(&"api".to_string(), None)
            .await
            .unwrap_err();
        assert_eq!(err.0, "Preparation failed for api");
        let state = host.state_of("api").unwrap();
        assert_eq!(state.actual_state, ActualServiceState::Failed);
        assert_eq!(state.error.as_deref(), Some("Preparation failed"));
    }

    #[tokio::test]
    async fn preparation_command_with_no_command_probe_configured_fails_the_start_immediately() {
        // Unlike command *readiness* (which degrades to a normal timeout with no adapter), a
        // preparation command with no way to run it can never succeed by waiting longer, so it
        // fails the start right away instead.
        let h = build_harness_with_timeout(preparation_catalog(), 20);
        let err = h
            .supervisor
            .start(&"api".to_string(), None)
            .await
            .unwrap_err();
        assert_eq!(err.0, "Preparation failed for api");
    }

    #[tokio::test]
    async fn preparation_command_serialization_by_key_runs_one_at_a_time() {
        // Mirrors `build_serialization_by_key_runs_one_at_a_time` — the same real risk (a
        // check-then-generate shared cert/config file with no locking of its own, e.g. `viclass`'s
        // `ensure_local_certificates`) motivated adding `serializationKey` to `preparationCommand` at
        // all, so this proves the `KeyedLock` wiring actually prevents the overlap it exists for.
        fn service_with_prep(id: &str, key: &str) -> ServiceDefinition {
            with_preparation_command(
                argv_verified(id, ReadinessSpec::Process),
                crate::catalog::PreparationCommand {
                    command: CommandSpec::Argv {
                        argv: vec!["prepare".to_string()],
                    },
                    cwd: None,
                    serialization_key: Some(key.to_string()),
                },
            )
        }
        let catalog = ServiceCatalog {
            services: vec![
                service_with_prep("a", "shared"),
                service_with_prep("b", "shared"),
            ],
            groups: HashMap::new(),
            group_tree: Vec::new(),
            compose_file: None,
            runtime_directory: None,
            start_failure_policy: StartFailurePolicy::StopOnFirstFailureKeepStarted,
            private_file_guard: None,
        };
        let host = FakeHost::new(catalog);
        let process = FakeProcessAdapter::new();
        let probes = Arc::new(CommandCapableProbeAdapter {
            result: std::sync::Mutex::new(Some(true)),
            delay_ms: 5,
            ..Default::default()
        });
        let run_build = FakeRunBuild::new();
        let clock = FakeClock::new();
        let supervisor = ProcessSupervisor::new(
            host.clone(),
            SupervisorOptions {
                process,
                run_build,
                probes: probes.clone(),
                preparation: Some(Arc::new(NoPreparation)),
                artifact_installer: None,
                clock,
                readiness_timeout_ms: 200,
                readiness_backoff_ms: 5,
                termination_grace_ms: 50,
                is_closing: Arc::new(|| false),
            },
        );
        let sup_a = supervisor.clone();
        let sup_b = supervisor.clone();
        let id_a = "a".to_string();
        let id_b = "b".to_string();
        let (ra, rb) = tokio::join!(sup_a.start(&id_a, None), sup_b.start(&id_b, None));
        ra.unwrap();
        rb.unwrap();

        let timeline = probes.timeline.lock().unwrap().clone();
        assert_eq!(timeline.len(), 2);
        let gap = timeline[1].duration_since(timeline[0]);
        assert!(
            gap >= std::time::Duration::from_millis(4),
            "preparation commands sharing a serializationKey must not overlap, gap was {gap:?}"
        );
    }

    #[tokio::test]
    async fn sync_external_services_adopts_on_ready_and_releases_on_not_ready() {
        let mut service = argv_verified("cache", ReadinessSpec::Tcp { port: 6379 });
        service.ownership = Some(crate::catalog::ServiceOwnership::External);
        let h = build_harness(one_service_catalog(service));

        h.probes.set_tcp_ready(6379, true);
        h.supervisor.sync_external_services().await;
        let state = h.host.state_of("cache").unwrap();
        assert_eq!(state.actual_state, ActualServiceState::Ready);
        assert_eq!(
            state.readiness_detail.as_deref(),
            Some("adopted from external state")
        );
        assert!(
            state.identity.is_none(),
            "external services never get a ProcessIdentity"
        );

        h.probes.set_tcp_ready(6379, false);
        h.supervisor.sync_external_services().await;
        let state = h.host.state_of("cache").unwrap();
        assert_eq!(state.actual_state, ActualServiceState::Stopped);
    }

    #[tokio::test]
    async fn container_readiness_identity_round_trips() {
        let mut service = argv_verified("db", ReadinessSpec::Container);
        service.profiles.run = ServiceRunProfile::Verified {
            command: ServiceCommand {
                command: CommandSpec::Shell {
                    shell: "docker compose up db".to_string(),
                    exec: None,
                },
                cwd: ".".to_string(),
                environment: None,
                container_name: Some("proj-db".to_string()),
                docker_stop_command: None,
            },
            readiness: ReadinessSpec::Container,
            readiness_timeout_ms: None,
            preparation: None,
            preparation_command: None,
        };
        let h = build_harness(one_service_catalog(service));
        h.process.register_container(
            "proj-db",
            DockerContainerRecord {
                container_name: "proj-db".to_string(),
                container_id: "abc123".to_string(),
                container_started_at: "2024-01-01T00:00:00.000Z".to_string(),
                command_fingerprint: normalize_command_fingerprint(&ServiceCommand {
                    command: CommandSpec::Shell {
                        shell: "docker compose up db".to_string(),
                        exec: None,
                    },
                    cwd: ".".to_string(),
                    environment: None,
                    container_name: Some("proj-db".to_string()),
                    docker_stop_command: None,
                }),
            },
        );
        h.probes.set_container_ready("proj-db", true);
        h.supervisor.start(&"db".to_string(), None).await.unwrap();
        let state = h.host.state_of("db").unwrap();
        assert_eq!(state.actual_state, ActualServiceState::Ready);
        match state.identity.unwrap() {
            ProcessIdentity::Docker(d) => assert_eq!(d.container_name, "proj-db"),
            _ => panic!("expected a docker identity"),
        }
        assert!(
            h.process
                .attach_calls()
                .contains(&("db".to_string(), "container")),
            "a container service must get a container log tail, not skip attach_output: {:?}",
            h.process.attach_calls()
        );
    }

    #[tokio::test]
    async fn external_container_adoption_attaches_one_log_tail_and_releases_it() {
        let mut service = argv_verified("cache", ReadinessSpec::Container);
        service.ownership = Some(crate::catalog::ServiceOwnership::External);
        service.profiles.run = ServiceRunProfile::Verified {
            command: ServiceCommand {
                command: CommandSpec::Argv {
                    argv: vec![
                        "docker".to_string(),
                        "compose".to_string(),
                        "up".to_string(),
                    ],
                },
                cwd: ".".to_string(),
                environment: None,
                container_name: Some("proj-cache".to_string()),
                docker_stop_command: None,
            },
            readiness: ReadinessSpec::Container,
            readiness_timeout_ms: None,
            preparation: None,
            preparation_command: None,
        };
        let h = build_harness(one_service_catalog(service));

        h.probes.set_container_ready("proj-cache", true);
        h.supervisor.sync_external_services().await;
        assert_eq!(
            h.host.state_of("cache").unwrap().actual_state,
            ActualServiceState::Ready
        );
        assert_eq!(
            h.process.attach_calls(),
            vec![("cache".to_string(), "container")]
        );

        // `sync_external_services` polls on an interval — an already-attached live tail must not be
        // replaced every tick (a container tail replays its backlog on attach, so a naive re-attach
        // would duplicate log lines each poll).
        h.supervisor.sync_external_services().await;
        assert_eq!(
            h.process.attach_calls().len(),
            1,
            "a live tail must not be re-attached"
        );

        // A follower that ended on its own (container restarted) is re-attached on the next tick.
        h.process.finish_tail("cache");
        h.supervisor.sync_external_services().await;
        assert_eq!(
            h.process.attach_calls().len(),
            2,
            "a dead tail must be re-attached"
        );

        h.probes.set_container_ready("proj-cache", false);
        h.supervisor.sync_external_services().await;
        assert_eq!(
            h.host.state_of("cache").unwrap().actual_state,
            ActualServiceState::Stopped
        );
        assert!(
            h.process.tail_was_stopped("cache"),
            "release must stop the log tail"
        );
    }

    #[tokio::test]
    async fn spawn_failure_transitions_to_failed_with_the_adapter_error() {
        let h = build_harness(one_service_catalog(argv_verified(
            "api",
            ReadinessSpec::Tcp { port: 8080 },
        )));
        h.process.fail_spawn("api", "no such file or directory");
        let err = h
            .supervisor
            .start(&"api".to_string(), None)
            .await
            .unwrap_err();
        assert_eq!(err.0, "no such file or directory");
        let state = h.host.state_of("api").unwrap();
        assert_eq!(state.actual_state, ActualServiceState::Failed);
        assert_eq!(state.error.as_deref(), Some("no such file or directory"));
    }

    /// The `shared:` expansion relies on this: an `ownership: external` service whose readiness is a
    /// `command` probe treats its run command as a one-shot task (`hearthd shared attach`), not the
    /// service process — the service adopts when the probe reports ready and carries no identity.
    #[tokio::test]
    async fn an_external_command_readiness_service_runs_its_command_as_a_one_shot_task() {
        let mut service = argv_verified(
            "api",
            ReadinessSpec::Command {
                command: CommandSpec::Argv {
                    argv: vec!["attach".to_string()],
                },
                cwd: None,
            },
        );
        service.ownership = Some(ServiceOwnership::External);
        let host = FakeHost::new(one_service_catalog(service));
        let process = FakeProcessAdapter::new();
        let probes = Arc::new(CommandCapableProbeAdapter {
            result: std::sync::Mutex::new(Some(true)),
            ..Default::default()
        });
        let supervisor = ProcessSupervisor::new(
            host.clone(),
            SupervisorOptions {
                process: process.clone(),
                run_build: FakeRunBuild::new(),
                probes,
                preparation: Some(Arc::new(NoPreparation)),
                artifact_installer: None,
                clock: FakeClock::new(),
                readiness_timeout_ms: 200,
                readiness_backoff_ms: 5,
                termination_grace_ms: 50,
                is_closing: Arc::new(|| false),
            },
        );
        supervisor.start(&"api".to_string(), None).await.unwrap();
        let state = host.state_of("api").unwrap();
        assert_eq!(state.actual_state, ActualServiceState::Ready);
        assert!(
            state.identity.is_none(),
            "a task run must not track a process identity"
        );
        assert!(
            process.state.lock().unwrap().alive.len() == 1,
            "the attach task must still be spawned once"
        );
    }

    #[tokio::test]
    async fn reconcile_marks_an_externally_killed_process_as_failed() {
        let h = build_harness(one_service_catalog(argv_verified(
            "api",
            ReadinessSpec::Tcp { port: 8080 },
        )));
        h.probes.set_tcp_ready(8080, true);
        h.supervisor.start(&"api".to_string(), None).await.unwrap();
        let pid = match h.host.state_of("api").unwrap().identity.unwrap() {
            ProcessIdentity::Posix(p) => p.pid,
            _ => panic!("expected posix identity"),
        };

        // Simulate the process dying on its own, out from under the supervisor (e.g. an operator ran
        // `kill -9` directly) — `inspect` will now report it as gone.
        h.process.kill_externally(pid, -1);
        h.supervisor.reconcile().await;

        let state = h.host.state_of("api").unwrap();
        assert_eq!(state.actual_state, ActualServiceState::Failed);
        let error = state.error.unwrap_or_default();
        assert!(
            error.contains("no longer alive") || error.contains("exited with code"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn stop_does_not_signal_when_the_process_tree_cannot_be_read() {
        let h = build_harness(one_service_catalog(argv_verified(
            "api",
            ReadinessSpec::Tcp { port: 8080 },
        )));
        h.probes.set_tcp_ready(8080, true);
        h.supervisor.start(&"api".to_string(), None).await.unwrap();
        h.process.state.lock().unwrap().tree_unknown = true;

        let error = h
            .supervisor
            .stop(&"api".to_string(), None)
            .await
            .unwrap_err();
        assert!(
            error.0.contains("could not be read") || error.0.contains("could not be verified"),
            "{error:?}"
        );
        assert!(
            h.process.signals().is_empty(),
            "a failed ps must not signal a pgid: {:?}",
            h.process.signals()
        );
        assert_ne!(
            h.host.state_of("api").unwrap().actual_state,
            ActualServiceState::Stopped
        );
    }

    #[tokio::test]
    async fn a_leader_exit_reaps_a_child_in_its_own_process_group() {
        let h = build_harness(one_service_catalog(argv_verified(
            "api",
            ReadinessSpec::Tcp { port: 8080 },
        )));
        h.probes.set_tcp_ready(8080, true);
        h.supervisor.start(&"api".to_string(), None).await.unwrap();
        let leader = match h.host.state_of("api").unwrap().identity.unwrap() {
            ProcessIdentity::Posix(p) => p.pid,
            _ => panic!("expected posix identity"),
        };
        let child = h.process.fork_into_own_group(leader);
        // The sampler records the tree while the leader is alive, then the leader exits on its own.
        tokio::time::sleep(std::time::Duration::from_millis(350)).await;
        h.process.kill_externally(leader, -1);

        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while tokio::time::Instant::now() < deadline && h.process.is_alive(child) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(
            !h.process.is_alive(child),
            "the child in its own process group must not survive the leader"
        );
        assert!(
            h.process
                .signals()
                .contains(&(child, ProcessSignal::Sigterm)),
            "secondary group must be signalled: {:?}",
            h.process.signals()
        );
        assert!(
            !h.process.signals().iter().any(|(pgid, _)| *pgid == leader),
            "the dead leader pgid must not be signalled"
        );
        assert_eq!(
            h.host.state_of("api").unwrap().actual_state,
            ActualServiceState::Failed
        );
    }

    #[tokio::test]
    async fn a_failed_one_shot_task_is_stopped_and_the_error_is_returned() {
        let mut service = argv_verified(
            "api",
            ReadinessSpec::Command {
                command: CommandSpec::Argv {
                    argv: vec!["attach".to_string()],
                },
                cwd: None,
            },
        );
        service.ownership = Some(ServiceOwnership::External);
        let host = FakeHost::new(one_service_catalog(service));
        let process = FakeProcessAdapter::new();
        let probes = Arc::new(CommandCapableProbeAdapter {
            result: std::sync::Mutex::new(Some(false)),
            ..Default::default()
        });
        let supervisor = ProcessSupervisor::new(
            host.clone(),
            SupervisorOptions {
                process: process.clone(),
                run_build: FakeRunBuild::new(),
                probes,
                preparation: Some(Arc::new(NoPreparation)),
                artifact_installer: None,
                clock: FakeClock::new(),
                readiness_timeout_ms: 30,
                readiness_backoff_ms: 5,
                termination_grace_ms: 50,
                is_closing: Arc::new(|| false),
            },
        );
        let error = supervisor
            .start(&"api".to_string(), None)
            .await
            .unwrap_err();
        assert!(error.0.contains("Readiness timed out"), "{error:?}");
        assert!(
            process.state.lock().unwrap().alive.is_empty(),
            "the one-shot process must be stopped when readiness fails"
        );
        assert!(
            host.state_of("api").unwrap().identity.is_none(),
            "a task failure must not persist the trigger process as the service identity"
        );
    }

    fn with_exit_timeout(mut service: ServiceDefinition, timeout_ms: u64) -> ServiceDefinition {
        if let ServiceRunProfile::Verified {
            readiness_timeout_ms,
            ..
        } = &mut service.profiles.run
        {
            *readiness_timeout_ms = Some(timeout_ms);
        }
        service
    }

    async fn wait_for_posix_pid(host: &FakeHost, service_id: &str) -> i64 {
        loop {
            if let Some(ProcessIdentity::Posix(posix)) =
                host.state_of(service_id).and_then(|state| state.identity)
            {
                return posix.pid;
            }
            tokio::task::yield_now().await;
        }
    }

    #[tokio::test]
    async fn an_exit_command_that_returns_zero_is_succeeded_and_not_restarted() {
        let h = build_harness_with_timeout(
            one_service_catalog(argv_verified("fe", ReadinessSpec::Exit)),
            30,
        );
        let supervisor = h.supervisor.clone();
        let host = h.host.clone();
        let process = h.process.clone();
        let clock = h.clock.clone();
        let started = clock.now_millis();
        let run = tokio::spawn(async move { supervisor.start(&"fe".to_string(), None).await });
        let pid = wait_for_posix_pid(&host, "fe").await;
        while clock.now_millis() < started + 40 {
            tokio::task::yield_now().await;
        }
        process.kill_externally(pid, 0);
        run.await.unwrap().unwrap();

        let state = host.state_of("fe").unwrap();
        assert_eq!(state.actual_state, ActualServiceState::Succeeded);
        assert_eq!(state.desired_state, DesiredServiceState::Stopped);
        assert_eq!(state.exit_code, Some(0));
        assert!(state.identity.is_none());
        assert!(state.error.is_none());

        h.supervisor.reconcile().await;
        let after = h.host.state_of("fe").unwrap();
        assert_eq!(after.actual_state, ActualServiceState::Succeeded);
        assert!(!h.process.is_alive(pid));
    }

    #[tokio::test]
    async fn an_exit_command_that_returns_nonzero_is_failed() {
        let h = build_harness(one_service_catalog(argv_verified(
            "fe",
            ReadinessSpec::Exit,
        )));
        let supervisor = h.supervisor.clone();
        let host = h.host.clone();
        let process = h.process.clone();
        let run = tokio::spawn(async move { supervisor.start(&"fe".to_string(), None).await });
        let pid = wait_for_posix_pid(&host, "fe").await;
        process.kill_externally(pid, 1);
        let error = run.await.unwrap().unwrap_err();
        assert!(error.0.contains("code 1"), "{error:?}");
        let state = host.state_of("fe").unwrap();
        assert_eq!(state.actual_state, ActualServiceState::Failed);
        assert_eq!(state.desired_state, DesiredServiceState::Stopped);
        assert_eq!(state.exit_code, Some(1));
        assert!(state.identity.is_none());
    }

    #[tokio::test]
    async fn stopping_an_exit_command_mid_run_records_stopped() {
        let h = build_harness(one_service_catalog(argv_verified(
            "fe",
            ReadinessSpec::Exit,
        )));
        let supervisor = h.supervisor.clone();
        let host = h.host.clone();
        let run = tokio::spawn(async move { supervisor.start(&"fe".to_string(), None).await });
        let _pid = wait_for_posix_pid(&host, "fe").await;
        h.supervisor.stop(&"fe".to_string(), None).await.unwrap();
        let _ = run.await.unwrap();
        let state = h.host.state_of("fe").unwrap();
        assert_eq!(state.actual_state, ActualServiceState::Stopped);
        assert_ne!(state.actual_state, ActualServiceState::Succeeded);
        assert_ne!(state.actual_state, ActualServiceState::Failed);
    }

    #[tokio::test]
    async fn an_exit_command_times_out_only_when_a_deadline_is_set() {
        let h = build_harness(one_service_catalog(with_exit_timeout(
            argv_verified("fe", ReadinessSpec::Exit),
            40,
        )));
        let error = h
            .supervisor
            .start(&"fe".to_string(), None)
            .await
            .unwrap_err();
        assert_eq!(error.0, "Readiness timed out");
        let state = h.host.state_of("fe").unwrap();
        assert_eq!(state.actual_state, ActualServiceState::Failed);
        assert_eq!(state.desired_state, DesiredServiceState::Stopped);
        assert!(state
            .readiness_detail
            .as_deref()
            .unwrap_or("")
            .contains("did not exit"));
    }

    #[tokio::test]
    async fn stop_on_a_succeeded_exit_command_keeps_succeeded() {
        let h = build_harness(one_service_catalog(argv_verified(
            "fe",
            ReadinessSpec::Exit,
        )));
        let supervisor = h.supervisor.clone();
        let host = h.host.clone();
        let process = h.process.clone();
        let run = tokio::spawn(async move { supervisor.start(&"fe".to_string(), None).await });
        let pid = wait_for_posix_pid(&host, "fe").await;
        process.kill_externally(pid, 0);
        run.await.unwrap().unwrap();
        h.supervisor.stop(&"fe".to_string(), None).await.unwrap();
        let state = h.host.state_of("fe").unwrap();
        assert_eq!(state.actual_state, ActualServiceState::Succeeded);
        assert_eq!(state.exit_code, Some(0));
    }

    #[tokio::test]
    async fn an_adopted_exit_command_that_disappears_is_not_succeeded() {
        let h = build_harness(one_service_catalog(argv_verified(
            "fe",
            ReadinessSpec::Exit,
        )));
        let start_identity = "Fake Jan  1 00:00:00 2024".to_string();
        h.process.insert_alive(42, "fp", &start_identity);
        h.host.seed(ServiceLifecycleState {
            service_id: "fe".to_string(),
            desired_state: DesiredServiceState::Running,
            actual_state: ActualServiceState::RunningUnready,
            readiness: ServiceReadiness::NotReady,
            generation: 1,
            identity: Some(ProcessIdentity::Posix(PosixProcessIdentity {
                manager_instance_id: "instance-1".to_string(),
                service_id: "fe".to_string(),
                generation: 1,
                pid: 42,
                pgid: 42,
                started_at: "2024-01-01T00:00:00.000Z".to_string(),
                start_identity,
                command_fingerprint: "fp".to_string(),
            })),
            readiness_kind: Some(crate::state::ReadinessKind::Exit),
            readiness_detail: None,
            created_at: "2024-01-01T00:00:00.000Z".to_string(),
            updated_at: "2024-01-01T00:00:00.000Z".to_string(),
            exited_at: None,
            exit_code: None,
            error: None,
            current_operation_id: None,
        });
        let supervisor = h.supervisor.clone();
        let process = h.process.clone();
        let reconcile = tokio::spawn(async move { supervisor.reconcile().await });
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
        process.kill_externally(42, 0);
        reconcile.await.unwrap();
        let state = h.host.state_of("fe").unwrap();
        assert_eq!(state.actual_state, ActualServiceState::Failed);
        assert_ne!(state.actual_state, ActualServiceState::Succeeded);
        let error = state.error.unwrap_or_default();
        assert!(
            error.contains("exit code unknown")
                || error.contains("no longer alive")
                || error.contains("exited with code"),
            "{error}"
        );
    }
}
