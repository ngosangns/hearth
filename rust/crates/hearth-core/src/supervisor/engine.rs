//! `ProcessSupervisor` — port of the class body in `src/core/supervisor.ts`. Default (production)
//! adapters (the bottom of that file — real `ps`/`docker`/`tailscale` shell-outs) are a separate,
//! not-yet-ported follow-up (`default_adapters.rs`, tracked in AGENTS.md); this module is the
//! engine itself, driven here entirely through the `ProcessAdapter`/`ProbeAdapter`/
//! `PreparationAdapter`/`Host` traits so it can be exercised with fakes.
use std::collections::HashMap;
use std::future::Future;
use std::hash::Hash;
use std::sync::{Arc, Mutex};

use tokio_util::sync::CancellationToken;

use crate::catalog::{
    is_container_command, CommandSpec, ReadinessSpec, ServiceCatalog, ServiceCommand, ServiceDefinition, ServiceId,
    ServiceOwnership, ServiceRunProfile,
};
use crate::state::{
    ActualServiceState, DesiredServiceState, ProcessIdentity, ReadinessKind, ServiceLifecycleState, ServiceReadiness,
};

use super::fingerprint::normalize_command_fingerprint;
use super::process_tree::{build_process_tree, process_tree_alive, secondary_process_groups, ProcessTreeEntry};
use super::types::{
    Host, ManagedProcess, OnOutput, OutputSource, OutputTail, ProcessRecord, ProcessSignal, SpawnInput,
    StartOptions, SupervisorError, SupervisorOptions,
};

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

/// A "task" command has no long-lived managed process to track — it runs once to bring external
/// state into the desired shape (generalizes a tailnet-serve-config runner).
fn is_task_command(readiness: &ReadinessSpec) -> bool {
    matches!(readiness, ReadinessSpec::Tailnet)
}

/// An `ownership: external` service whose readiness is a `command` probe is also a task: its run
/// command is a one-shot "bring the external thing up" trigger (e.g. the generated
/// `hearthd shared attach` for a `shared:` entry), not the service process itself. Only the
/// external+ccommand combination is treated this way — a daemon-owned service with `command`
/// readiness still runs a real long-lived process.
fn is_external_task(definition: Option<&ServiceDefinition>, profile: &VerifiedProfile) -> bool {
    matches!(definition.and_then(|d| d.ownership), Some(ServiceOwnership::External)) && matches!(profile.readiness, ReadinessSpec::Command { .. })
}

fn readiness_kind_of(readiness: &ReadinessSpec) -> ReadinessKind {
    match readiness {
        ReadinessSpec::Process => ReadinessKind::Process,
        ReadinessSpec::Tcp { .. } => ReadinessKind::Tcp,
        ReadinessSpec::Http { .. } => ReadinessKind::Http,
        ReadinessSpec::Container => ReadinessKind::Container,
        ReadinessSpec::Tailnet => ReadinessKind::Tailnet,
        ReadinessSpec::Command { .. } => ReadinessKind::Command,
    }
}

fn profile_for(catalog: &ServiceCatalog, service_id: &str) -> Result<VerifiedProfile, SupervisorError> {
    let service = catalog.services.iter().find(|s| s.id == service_id);
    match service.map(|s| &s.profiles.run) {
        Some(ServiceRunProfile::Verified { command, readiness, readiness_timeout_ms, preparation, preparation_command }) => Ok(VerifiedProfile {
            command: command.clone(),
            readiness: readiness.clone(),
            readiness_timeout_ms: *readiness_timeout_ms,
            preparation: preparation.clone(),
            preparation_command: preparation_command.clone(),
        }),
        _ => Err(SupervisorError(format!("Unsupported service {service_id}"))),
    }
}

fn definition_for<'a>(catalog: &'a ServiceCatalog, service_id: &str) -> Option<&'a ServiceDefinition> {
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
    #[allow(dead_code)] // kept 1:1 with the TS `Active.token` field; no current method reads it back
    token: u64,
    stopped: bool,
}

/// Tri-state patch for an optional field: keep the previous value, clear it, or set a new one —
/// port of `Changes`'s `clear: Array<...>` sibling-of-a-plain-partial-object pattern.
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
    pub readiness_kind: Option<ReadinessKind>,
    pub readiness_detail: Option<String>,
    pub exited_at: Patch<String>,
    pub exit_code: Patch<i32>,
    pub error: Patch<String>,
}

enum ReadinessOutcome {
    Ready,
    Superseded,
    Exited { message: String },
    Timeout { message: String, detail: String },
}

/// A keyed set of per-key async mutexes — port of the `serial()` per-`ServiceId` FIFO queue and,
/// separately, of the build-serialization-by-`serializationKey` map. Both are "run at most one
/// operation at a time per key, queue the rest" — the same primitive.
struct KeyedLock<K: Eq + Hash + Clone> {
    locks: Mutex<HashMap<K, Arc<tokio::sync::Mutex<()>>>>,
}
impl<K: Eq + Hash + Clone> KeyedLock<K> {
    fn new() -> Self {
        Self { locks: Mutex::new(HashMap::new()) }
    }
    fn get(&self, key: &K) -> Arc<tokio::sync::Mutex<()>> {
        let mut map = self.locks.lock().unwrap();
        map.entry(key.clone()).or_insert_with(|| Arc::new(tokio::sync::Mutex::new(()))).clone()
    }
    async fn run<F, Fut, T>(&self, key: &K, work: F) -> T
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = T>,
    {
        let lock = self.get(key);
        let _guard = lock.lock().await;
        work().await
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
    log_forwarders: Mutex<HashMap<ServiceId, tokio::sync::mpsc::UnboundedSender<String>>>,
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

    pub async fn start(self: &Arc<Self>, service_id: &ServiceId, operation_id: Option<String>) -> Result<(), SupervisorError> {
        self.start_with_options(service_id, operation_id, StartOptions::default()).await
    }

    /// `StartOptions::kill_unowned` is the wire-level echo of a user's explicit "yes, kill the
    /// process holding my port" — it must only ever come from a confirmed client request.
    pub async fn start_with_options(self: &Arc<Self>, service_id: &ServiceId, operation_id: Option<String>, options: StartOptions) -> Result<(), SupervisorError> {
        let sup = self.clone();
        let id = service_id.clone();
        self.queues.run(service_id, || async move { sup.start_locked(&id, operation_id, options).await }).await
    }

    pub async fn restart(self: &Arc<Self>, service_id: &ServiceId, operation_id: Option<String>) -> Result<(), SupervisorError> {
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

    pub async fn stop(self: &Arc<Self>, service_id: &ServiceId, operation_id: Option<String>) -> Result<(), SupervisorError> {
        self.cancel(service_id);
        self.abort_build(service_id);
        let sup = self.clone();
        let id = service_id.clone();
        self.queues.run(service_id, || async move { sup.stop_locked(&id, operation_id).await }).await
    }

    pub async fn start_group(self: &Arc<Self>, group: &str, operation_id: Option<String>) -> Result<(), SupervisorError> {
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
        let mut ids: std::collections::HashSet<ServiceId> = self.tokens.lock().unwrap().keys().cloned().collect();
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
        let to_reap: Vec<ServiceId> =
            self.host.service_states().into_iter().filter(|s| daemon_owned.contains(&s.service_id)).map(|s| s.service_id).collect();
        for id in to_reap {
            let sup = self.clone();
            let sid = id.clone();
            self.queues.run(&id, || async move { sup.reap_persisted_posix_identity(&sid).await }).await;
        }

        // Container log followers are real child processes (`docker logs -f`) in their own process
        // group — they would outlive the daemon as orphans streaming to a dead pipe.
        let tails: Vec<OutputTail> = self.output_tails.lock().unwrap().drain().map(|(_, tail)| tail).collect();
        for tail in tails {
            tail.stop();
        }
    }

    async fn reap_persisted_posix_identity(self: &Arc<Self>, service_id: &ServiceId) {
        let Some(state) = self.state(service_id) else { return };
        let Some(identity) = &state.identity else { return };
        if matches!(identity, ProcessIdentity::Docker(_)) || !self.identity_matches_state(&state) {
            return;
        }
        self.terminate_persisted_posix_identity(identity).await;
    }

    async fn terminate_persisted_posix_identity(self: &Arc<Self>, identity: &ProcessIdentity) {
        if !self.observed_matches(identity).await {
            return;
        }
        let ProcessIdentity::Posix(posix) = identity else { return };
        let tree = self.process_tree(posix.pid, &posix.start_identity).await;
        self.signal_process_tree(&tree, posix.pgid, ProcessSignal::Sigterm).await;
        let deadline = self.options.clock.now_millis() + self.options.termination_grace_ms;
        while self.options.clock.now_millis() < deadline {
            if !self.process_tree_alive(&tree).await {
                return;
            }
            self.options.clock.sleep(self.options.readiness_backoff_ms).await;
        }
        if self.process_tree_alive(&tree).await {
            self.signal_process_tree(&tree, posix.pgid, ProcessSignal::Sigkill).await;
        }
    }

    pub async fn status(self: &Arc<Self>, service_id: &ServiceId) -> Result<(), SupervisorError> {
        let sup = self.clone();
        let id = service_id.clone();
        self.queues
            .run(service_id, || async move {
                let Some(state) = sup.state(&id) else { return Ok(()) };
                if state.identity.is_none() || !is_active_state(state.actual_state) {
                    return Ok(());
                }
                if !sup.owns(&state).await {
                    sup.orphan(&state, None).await;
                    return Ok(());
                }
                let profile = profile_for(&sup.host.catalog(), &id)?;
                if !matches!(profile.readiness, ReadinessSpec::Process) && !sup.probe(&profile.readiness, &id).await {
                    sup.transition(
                        &id,
                        state.generation,
                        ActualServiceState::RunningUnready,
                        ServiceReadiness::NotReady,
                        Changes {
                            readiness_kind: Some(readiness_kind_of(&profile.readiness)),
                            readiness_detail: Some("Readiness probe is currently unavailable".to_string()),
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
                    let observed = sup.options.process.inspect(&identity).await;
                    if !observed.as_ref().map(|o| o.alive).unwrap_or(false) {
                        let next_state =
                            if state.desired_state == DesiredServiceState::Running { ActualServiceState::Failed } else { ActualServiceState::Stopped };
                        sup.transition(
                            &service_id,
                            state.generation,
                            next_state,
                            ServiceReadiness::Failed,
                            Changes {
                                error: Patch::Set("Managed process is no longer alive".to_string()),
                                exited_at: Patch::Set(sup.options.clock.now()),
                                ..Default::default()
                            },
                        )
                        .await;
                        return;
                    }
                    if !sup.identity_matches_state(&state) || !sup.observed_matches(&identity).await {
                        sup.orphan(&state, None).await;
                        return;
                    }
                    let updated_identity = with_manager_instance(identity, sup.host.instance_id());
                    sup.transition(
                        &service_id,
                        state.generation,
                        ActualServiceState::RunningUnready,
                        ServiceReadiness::NotReady,
                        Changes { identity: Patch::Set(updated_identity.clone()), error: Patch::Clear, ..Default::default() },
                    )
                    .await;
                    // Guard on a live tail rather than attaching unconditionally: `reconcile` runs
                    // periodically, and re-attaching a container tail replays its `--since` backlog
                    // into the log file every tick.
                    if !sup.has_live_output_tail(&service_id) {
                        sup.attach_output(&service_id, output_source(&updated_identity));
                    }
                    let Ok(profile) = profile_for(&sup.host.catalog(), &service_id) else { return };
                    let token = sup.current_token(&service_id);
                    let _ = sup.readiness_with_adopted(&service_id, &profile, state.generation, Some(updated_identity), token, None, true).await;
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
                    let ServiceRunProfile::Verified { command, readiness, .. } = &service.profiles.run else { return };
                    let ready = sup.probe(readiness, &service_id).await;
                    let actual = state.as_ref().map(|s| s.actual_state);
                    if ready && matches!(actual, Some(ActualServiceState::Stopped) | Some(ActualServiceState::Failed) | None) {
                        let generation = state.as_ref().map(|s| s.generation).unwrap_or(0) + 1;
                        sup.transition(
                            &service_id,
                            generation,
                            ActualServiceState::Ready,
                            ServiceReadiness::Ready,
                            Changes {
                                readiness_kind: Some(readiness_kind_of(readiness)),
                                readiness_detail: Some("adopted from external state".to_string()),
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
                                    OutputSource::Container { container_name, since: None, tail: Some(EXTERNAL_LOG_BACKLOG_LINES) },
                                );
                            }
                        }
                    } else if let Some(state) = &state {
                        if matches!(state.actual_state, ActualServiceState::Ready | ActualServiceState::RunningUnready) {
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

    async fn start_locked(self: &Arc<Self>, service_id: &ServiceId, operation_id: Option<String>, options: StartOptions) -> Result<(), SupervisorError> {
        if (self.options.is_closing)() {
            return Ok(());
        }
        let profile = profile_for(&self.host.catalog(), service_id)?;
        let current = self.state(service_id);
        let existing_stopped = { let active = self.active.lock().unwrap(); active.get(service_id).map(|a| a.stopped) };
        if let Some(stopped) = existing_stopped {
            if !stopped && current.as_ref().map(|s| s.actual_state) != Some(ActualServiceState::Failed) {
                return Ok(());
            }
        }
        if existing_stopped.is_some() {
            self.finalize(service_id, true).await;
        }
        if let Some(current) = &current {
            if current.identity.is_some() && is_active_state(current.actual_state) && self.owns(current).await {
                return Ok(());
            }
        }
        let retained_identity = match &current {
            Some(state) if self.identity_matches_state(state) => match &state.identity {
                Some(ProcessIdentity::Posix(_)) => {
                    if self.observed_matches(state.identity.as_ref().unwrap()).await {
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
                    current_operation_id: operation_id.clone().map(Patch::Set).unwrap_or(Patch::Keep),
                    identity: Patch::Set(identity.clone()),
                    error: Patch::Clear,
                    exit_code: Patch::Clear,
                    exited_at: Patch::Clear,
                    ..Default::default()
                },
            )
            .await;
            self.attach_output(service_id, output_source(&identity));
            // An adopted identity is only worth keeping while it still answers its readiness probe.
            let outcome = self.await_readiness(service_id, &profile, generation, Some(identity.clone()), token, true).await;
            match outcome {
                ReadinessOutcome::Ready | ReadinessOutcome::Superseded => return Ok(()),
                _ => {}
            }
            token = self.cancel(service_id);
            self.report_terminate_failure(service_id, self.terminate(&identity, &profile.command).await);
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
                ..Default::default()
            },
        )
        .await;

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
                    Some(key) => self.serialized_preparation_command(key, &prep_command.command, prep_command.cwd.as_deref()).await,
                    None => self.options.probes.command(&prep_command.command, prep_command.cwd.as_deref()).await,
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
                Changes { error: Patch::Set("Preparation failed".to_string()), current_operation_id: operation_id.clone().map(Patch::Set).unwrap_or(Patch::Keep), ..Default::default() },
            )
            .await;
            return Err(SupervisorError(format!("Preparation failed for {service_id}")));
        }
        if !self.valid(service_id, generation, token) || (self.options.is_closing)() {
            return Ok(());
        }
        let definition = definition_for(&self.host.catalog(), service_id).cloned();
        if let Some(def) = &definition {
            if def.profiles.build.is_some() {
                self.build(service_id, def, generation, token, operation_id.clone()).await?;
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
                            Changes { error: Patch::Set(format!("Port {port} is held by {held_by}")), ..Default::default() },
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
                            Changes { error: Patch::Set(error.0.clone()), ..Default::default() },
                        )
                        .await;
                        return Err(error);
                    }
                }
            }
        }
        self.spawn_and_wait(service_id, &profile, generation, token, operation_id).await
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
                    self.options.process.signal_pid(holder.pid, &holder.start_identity, signal).await;
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
                self.options.clock.sleep(self.options.readiness_backoff_ms).await;
            }
        }
        Err(SupervisorError(format!("Port {port} is still held by {} after SIGKILL", self.describe_port_holders(port).await)))
    }

    async fn stop_locked(self: &Arc<Self>, service_id: &ServiceId, operation_id: Option<String>) -> Result<(), SupervisorError> {
        let Some(state) = self.state(service_id) else { return Ok(()) };
        if state.actual_state == ActualServiceState::Stopped {
            return Ok(());
        }
        let has_active = self.active.lock().unwrap().contains_key(service_id);
        if state.identity.is_none() {
            if matches!(state.actual_state, ActualServiceState::QueuedStart | ActualServiceState::Preparing | ActualServiceState::Starting)
                || (self.options.is_closing)()
            {
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
        if !self.owns(&state).await {
            let observed = self.options.process.inspect(state.identity.as_ref().unwrap()).await;
            if !observed.map(|o| o.alive).unwrap_or(false) {
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
                return Ok(());
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
            Changes { desired_state: Some(DesiredServiceState::Stopped), current_operation_id: operation_id.clone().map(Patch::Set).unwrap_or(Patch::Keep), ..Default::default() },
        )
        .await;
        if has_active {
            if let Some(active) = self.active.lock().unwrap().get_mut(service_id) {
                active.stopped = true;
            }
        }
        let profile = profile_for(&self.host.catalog(), service_id)?;
        self.terminate(state.identity.as_ref().unwrap(), &profile.command).await?;
        if has_active {
            self.active.lock().unwrap().remove(service_id);
        }
        self.transition(
            service_id,
            state.generation,
            ActualServiceState::Stopped,
            ServiceReadiness::Unknown,
            Changes { desired_state: Some(DesiredServiceState::Stopped), exited_at: Patch::Set(self.options.clock.now()), error: Patch::Clear, exit_code: Patch::Clear, ..Default::default() },
        )
        .await;
        Ok(())
    }

    /// Stops a service this manager holds no process identity for — an adopted `ownership: external`
    /// unit (a docker/tailnet task it only observes), or a service whose port is held by a process it
    /// does not own. The catalog's `stop:` command is the only lever that exists for those, so it is
    /// what runs; with no such command, refusing loudly is the honest answer. Reporting success
    /// without stopping anything is what made Stop look broken in the app.
    async fn stop_unowned(self: &Arc<Self>, service_id: &ServiceId, state: &ServiceLifecycleState, operation_id: Option<String>) -> Result<(), SupervisorError> {
        let profile = profile_for(&self.host.catalog(), service_id)?;
        if profile.command.docker_stop_command.is_none() {
            let reason = match state.actual_state {
                ActualServiceState::ExternallyOwned => state.error.clone().unwrap_or_else(|| "its port is held by a process this manager does not own".to_string()),
                _ => "it is owned externally".to_string(),
            };
            return Err(SupervisorError(format!("{service_id} cannot be stopped: {reason}, and its catalog declares no `stop` command")));
        }
        let sink: OnOutput = Arc::new(|_| {});
        match self.options.process.stop_container(&profile.command, sink).await {
            Some(result) => result?,
            None => return Err(SupervisorError(format!("{service_id} cannot be stopped: this process adapter has no stop command support"))),
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
            Changes { desired_state: Some(DesiredServiceState::Running), current_operation_id: operation_id.clone().map(Patch::Set).unwrap_or(Patch::Keep), identity: Patch::Clear, ..Default::default() },
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
        let input = SpawnInput { command: profile.command.clone(), command_fingerprint: fingerprint, service_id: service_id.clone() };
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
                    Changes { error: Patch::Set(error.0.clone()), current_operation_id: operation_id.map(Patch::Set).unwrap_or(Patch::Keep), identity: Patch::Clear, ..Default::default() },
                )
                .await;
                return Err(error);
            }
        };

        let catalog = self.host.catalog();
        let definition = definition_for(&catalog, service_id);
        if (is_task_command(&profile.readiness) || is_external_task(definition, profile)) && !app.record.is_docker() {
            self.transition(
                service_id,
                generation,
                ActualServiceState::RunningUnready,
                ServiceReadiness::NotReady,
                Changes { identity: Patch::Clear, ..Default::default() },
            )
            .await;
            let _ = self.readiness(service_id, profile, generation, None, token, operation_id).await;
            return Ok(());
        }

        let identity = match &app.record {
            ProcessRecord::Docker(record) => ProcessIdentity::Docker(crate::state::DockerContainerIdentity {
                manager_instance_id: self.host.instance_id(),
                service_id: service_id.clone(),
                generation,
                started_at: self.options.clock.now(),
                container_name: record.container_name.clone(),
                container_id: record.container_id.clone(),
                container_started_at: record.container_started_at.clone(),
                command_fingerprint: record.command_fingerprint.clone(),
            }),
            ProcessRecord::Posix(record) => ProcessIdentity::Posix(crate::state::PosixProcessIdentity {
                manager_instance_id: self.host.instance_id(),
                service_id: service_id.clone(),
                generation,
                started_at: self.options.clock.now(),
                pid: record.pid,
                pgid: record.pgid,
                start_identity: record.start_identity.clone(),
                command_fingerprint: record.command_fingerprint.clone(),
            }),
        };
        if !self.valid(service_id, generation, token) {
            self.report_terminate_failure(service_id, self.terminate(&identity, &profile.command).await);
            return Ok(());
        }
        self.active
            .lock()
            .unwrap()
            .insert(service_id.clone(), Active { command: profile.command.clone(), identity: identity.clone(), token, stopped: false });
        self.attach_output(service_id, output_source(&identity));
        let sup = self.clone();
        let sid = service_id.clone();
        let exited = app.exited;
        tokio::spawn(async move {
            let code = exited.await.unwrap_or(-1);
            sup.on_exit(&sid, generation, token, code).await;
        });
        self.transition(service_id, generation, ActualServiceState::RunningUnready, ServiceReadiness::NotReady, Changes { identity: Patch::Set(identity), ..Default::default() }).await;
        self.readiness(service_id, profile, generation, self.state(service_id).and_then(|s| s.identity), token, operation_id).await
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
        self.readiness_with_adopted(service_id, profile, generation, identity, token, operation_id, false).await
    }

    #[allow(clippy::too_many_arguments)] // mirrors the TS method's own parameter list 1:1
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
        let outcome = self.await_readiness(service_id, profile, generation, identity.clone(), token, adopted).await;
        match outcome {
            ReadinessOutcome::Ready | ReadinessOutcome::Superseded => Ok(()),
            ReadinessOutcome::Exited { message } => {
                self.fail(service_id, generation, token, identity, &message, operation_id, None).await;
                Err(SupervisorError(message))
            }
            ReadinessOutcome::Timeout { message, detail } => {
                self.fail(service_id, generation, token, identity, &message, operation_id, Some((readiness_kind_of(&profile.readiness), detail))).await;
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
        if matches!(profile.readiness, ReadinessSpec::Process) {
            if !self.valid(service_id, generation, token) {
                return ReadinessOutcome::Superseded;
            }
            if let Some(identity) = &identity {
                if !self.owns_identity(identity).await {
                    return if self.valid(service_id, generation, token) {
                        ReadinessOutcome::Exited { message: "Process exited before liveness check".to_string() }
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
                Changes { identity: identity.map(Patch::Set).unwrap_or(Patch::Keep), readiness_kind: Some(ReadinessKind::Process), readiness_detail: Some("process-liveness-only".to_string()), ..Default::default() },
            )
            .await;
            return ReadinessOutcome::Ready;
        }
        let timeout = profile.readiness_timeout_ms.map(|v| v as i64).unwrap_or(self.options.readiness_timeout_ms);
        let deadline = self.options.clock.now_millis() + timeout;
        loop {
            if self.options.clock.now_millis() > deadline {
                break;
            }
            if !self.valid(service_id, generation, token) {
                return ReadinessOutcome::Superseded;
            }
            if let Some(identity) = &identity {
                if !self.owns_identity(identity).await {
                    return if self.valid(service_id, generation, token) {
                        ReadinessOutcome::Exited { message: "Process exited before readiness".to_string() }
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
                        readiness_kind: Some(readiness_kind_of(&profile.readiness)),
                        readiness_detail: Some(if adopted { "adopted readiness verified".to_string() } else { "readiness verified".to_string() }),
                        error: Patch::Clear,
                        ..Default::default()
                    },
                )
                .await;
                return ReadinessOutcome::Ready;
            }
            self.options.clock.sleep(self.options.readiness_backoff_ms).await;
        }
        ReadinessOutcome::Timeout {
            message: "Readiness timed out".to_string(),
            detail: format!("Readiness {} probe timed out after {}ms", readiness_kind_of(&profile.readiness).as_wire_str(), timeout),
        }
    }

    #[allow(clippy::too_many_arguments)] // mirrors the TS method's own parameter list 1:1
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
        self.transition_if_current(
            service_id,
            generation,
            token,
            ActualServiceState::Failed,
            ServiceReadiness::Failed,
            Changes {
                identity: identity.clone().map(Patch::Set).unwrap_or(Patch::Keep),
                error: Patch::Set(error.to_string()),
                current_operation_id: operation_id.map(Patch::Set).unwrap_or(Patch::Keep),
                readiness_kind: readiness_change.as_ref().map(|(k, _)| *k),
                readiness_detail: readiness_change.map(|(_, d)| d),
                ..Default::default()
            },
        )
        .await;
        self.cancel(service_id);
        let active_generation = { self.active.lock().unwrap().get(service_id).map(|a| match &a.identity {
            ProcessIdentity::Posix(p) => p.generation,
            ProcessIdentity::Docker(d) => d.generation,
        }) };
        if active_generation == Some(generation) {
            self.finalize(service_id, true).await;
        }
    }

    async fn on_exit(self: &Arc<Self>, service_id: &ServiceId, generation: u64, token: u64, code: i32) {
        let sup = self.clone();
        let sid = service_id.clone();
        self.queues
            .run(&service_id.clone(), || async move {
                if !sup.valid(&sid, generation, token) {
                    return;
                }
                let Some(state) = sup.state(&sid) else { return };
                let active_matches = { sup.active.lock().unwrap().get(&sid).map(|a| !a.stopped).unwrap_or(false) };
                if state.generation != generation || !active_matches {
                    return;
                }
                sup.transition_if_current(
                    &sid,
                    generation,
                    token,
                    ActualServiceState::Failed,
                    ServiceReadiness::Failed,
                    Changes { exit_code: Patch::Set(code), exited_at: Patch::Set(sup.options.clock.now()), error: Patch::Set(format!("Process exited with code {code}")), ..Default::default() },
                )
                .await;
                let still_this_generation = {
                    let mut active = sup.active.lock().unwrap();
                    let matches = active.get(&sid).map(|a| match &a.identity {
                        ProcessIdentity::Posix(p) => p.generation == generation,
                        ProcessIdentity::Docker(d) => d.generation == generation,
                    }).unwrap_or(false);
                    if matches {
                        active.remove(&sid);
                    }
                    matches
                };
                let _ = still_this_generation;
            })
            .await;
    }

    async fn finalize(self: &Arc<Self>, service_id: &ServiceId, terminate: bool) {
        let active_entry = { self.active.lock().unwrap().remove(service_id) };
        if let Some(mut active) = active_entry {
            active.stopped = true;
            if terminate && self.owns_identity(&active.identity).await {
                let service_id = match &active.identity {
                    ProcessIdentity::Posix(p) => p.service_id.clone(),
                    ProcessIdentity::Docker(d) => d.service_id.clone(),
                };
                self.report_terminate_failure(&service_id, self.terminate(&active.identity, &active.command).await);
            }
        }
    }

    /// A container stop that FAILS must propagate: `stop_locked` transitions the service to
    /// `stopped` immediately after this returns, so swallowing the error here would record a
    /// service as stopped while its container is still running — the exact state drift this whole
    /// daemon exists to prevent. An adapter that has no `stop_container` at all is likewise an
    /// error (matching the TS source's `Docker container stop is unavailable`), not a silent no-op.
    async fn terminate(self: &Arc<Self>, identity: &ProcessIdentity, command: &ServiceCommand) -> Result<(), SupervisorError> {
        if !self.owns_identity(identity).await {
            return Ok(());
        }
        match identity {
            ProcessIdentity::Docker(docker) => {
                let sink: OnOutput = Arc::new(|_| {});
                let result = match self.options.process.stop_container(command, sink).await {
                    Some(result) => result,
                    None => Err(SupervisorError("Docker container stop is unavailable".to_string())),
                };
                // Stop the `docker logs` follower only after the stop command has run — its last
                // chunk carries the container's own shutdown output.
                self.detach_output(&docker.service_id);
                result?;
            }
            ProcessIdentity::Posix(posix) => {
                self.detach_output(&posix.service_id);
                let tree = self.process_tree(posix.pid, &posix.start_identity).await;
                self.signal_process_tree(&tree, posix.pgid, ProcessSignal::Sigterm).await;
                let deadline = self.options.clock.now_millis() + self.options.termination_grace_ms;
                loop {
                    if self.options.clock.now_millis() >= deadline {
                        break;
                    }
                    if !self.process_tree_alive(&tree).await {
                        return Ok(());
                    }
                    self.options.clock.sleep(self.options.readiness_backoff_ms).await;
                }
                if self.process_tree_alive(&tree).await {
                    self.signal_process_tree(&tree, posix.pgid, ProcessSignal::Sigkill).await;
                }
            }
        }
        Ok(())
    }

    /// The three cleanup call sites of `terminate` have no caller to return an error to (they run
    /// while abandoning a superseded start, or while finalizing). They still must not drop it
    /// silently — this codebase's rule is that fire-and-forget work reports through
    /// `Host::record_background_error` rather than vanishing.
    fn report_terminate_failure(&self, service_id: &ServiceId, result: Result<(), SupervisorError>) {
        if let Err(error) = result {
            self.host.record_background_error("supervisor.terminate", &format!("{service_id}: {}", error.0));
        }
    }

    async fn process_tree(&self, leader_pid: i64, leader_start_identity: &str) -> Vec<ProcessTreeEntry> {
        let Ok(stdout) = run_ps(&["-Ao", "pid=,ppid=,pgid=,lstart="]).await else { return Vec::new() };
        let rows = super::process_tree::parse_ps_tree_rows(&stdout);
        build_process_tree(&rows, leader_pid, leader_start_identity)
    }

    async fn process_tree_alive(&self, tree: &[ProcessTreeEntry]) -> bool {
        if tree.is_empty() {
            return false;
        }
        let Ok(stdout) = run_ps(&["-Ao", "pid=,lstart="]).await else { return false };
        let alive = super::process_tree::parse_ps_alive_rows(&stdout);
        process_tree_alive(tree, &alive)
    }

    async fn signal_process_tree(&self, tree: &[ProcessTreeEntry], leader_pgid: i64, signal: ProcessSignal) {
        self.options.process.signal_group(leader_pgid, signal).await;
        let groups = secondary_process_groups(tree, leader_pgid);
        for (pgid, members) in groups {
            let mut still_ours = false;
            for member in &members {
                if let Ok(stdout) = run_ps(&["-o", "pid=", "-o", "pgid=", "-o", "lstart=", "-p", &member.pid.to_string()]).await {
                    let rows = super::process_tree::parse_ps_tree_rows(&format!("{stdout}\n"));
                    if let Some(row) = rows.first() {
                        if row.start_identity == member.start_identity {
                            still_ours = true;
                            break;
                        }
                    }
                }
            }
            if still_ours {
                self.options.process.signal_group(pgid, signal).await;
            }
        }
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
        self.build_aborts.lock().unwrap().insert(service_id.clone(), cancel.clone());
        let timeout_ms = build.timeout_ms.unwrap_or(15 * 60_000);
        let sup = self.clone();
        let sid = service_id.clone();
        let on_output: OnOutput = Arc::new(move |data: &str| {
            if sup.valid(&sid, generation, token) {
                sup.append_output(&sid, data);
            }
        });

        let cancel_for_timeout = cancel.clone();
        let timed_out = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let timed_out_flag = timed_out.clone();
        let timeout_task = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(timeout_ms)).await;
            timed_out_flag.store(true, std::sync::atomic::Ordering::SeqCst);
            cancel_for_timeout.cancel();
        });

        let result = if let Some(key) = &build.serialization_key {
            self.serialized_build(key, cancel.clone(), &build.command, on_output).await
        } else {
            self.options.run_build.run(&build.command, on_output, cancel.clone()).await
        };
        timeout_task.abort();
        self.build_aborts.lock().unwrap().remove(service_id);

        match result {
            Ok(()) => Ok(()),
            Err(error) => {
                if !self.valid(service_id, generation, token) || (self.options.is_closing)() {
                    return Ok(());
                }
                // An externally cancelled build (a restart or stop calling `abort_build`) reports
                // "Build timed out" too: the TS source keys this off the abort signal having fired
                // at all, not off a timeout specifically, and this string is surfaced to the user.
                let aborted = timed_out.load(std::sync::atomic::Ordering::SeqCst) || cancel.is_cancelled();
                let message = if aborted { "Build timed out" } else { "Build failed" };
                self.transition_if_current(
                    service_id,
                    generation,
                    token,
                    ActualServiceState::Failed,
                    ServiceReadiness::Failed,
                    Changes { error: Patch::Set(message.to_string()), current_operation_id: operation_id.map(Patch::Set).unwrap_or(Patch::Keep), ..Default::default() },
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
                tokio::select! {
                    result = run_build.run(&command, on_output, cancel.clone()) => result,
                    _ = cancel.cancelled() => Err(SupervisorError("Build cancelled".to_string())),
                }
            })
            .await
    }

    /// Runs a declarative preparation command through `probes.command()`, serialized against every
    /// other in-flight preparation command sharing the same `key` — the preparation analogue of
    /// `serialized_build`, minus the cancellation plumbing (unlike a build, nothing today cancels an
    /// in-flight preparation command).
    async fn serialized_preparation_command(&self, key: &str, command: &CommandSpec, cwd: Option<&str>) -> Option<bool> {
        let probes = self.options.probes.clone();
        let command = command.clone();
        let cwd = cwd.map(str::to_string);
        self.preparation_serials.run(&key.to_string(), || async move { probes.command(&command, cwd.as_deref()).await }).await
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
            ReadinessSpec::Command { command, cwd } => self.options.probes.command(command, cwd.as_deref()).await.unwrap_or(false),
            ReadinessSpec::Container => {
                let Ok(profile) = profile_for(&self.host.catalog(), service_id) else { return false };
                match profile.command.container_name {
                    Some(name) => self.options.probes.container(&name).await,
                    None => false,
                }
            }
            ReadinessSpec::Process => false,
        }
    }

    fn attach_output(self: &Arc<Self>, service_id: &ServiceId, source: OutputSource<'_>) {
        self.detach_output(service_id);
        // `attach_output` is one of the "optional" ProcessAdapter methods (default adapters not yet
        // ported) — a `None` result (no attachment support) is a normal, expected outcome here.
        let on_output: OnOutput = {
            let this = self.clone();
            let sid = service_id.clone();
            Arc::new(move |data: &str| {
                this.append_output(&sid, data);
            })
        };
        if let Some(tail) = self.options.process.attach_output(service_id, source, on_output) {
            self.output_tails.lock().unwrap().insert(service_id.clone(), tail);
        }
    }

    fn detach_output(&self, service_id: &ServiceId) {
        if let Some(tail) = self.output_tails.lock().unwrap().remove(service_id) {
            tail.stop();
        }
    }

    fn has_live_output_tail(&self, service_id: &ServiceId) -> bool {
        self.output_tails.lock().unwrap().get(service_id).map(|tail| !tail.is_done()).unwrap_or(false)
    }

    /// Log forwarding is fire-and-forget by design: a failure must never propagate into the caller.
    /// It is also strictly ordered — chunks go through this service's single forwarder task, so the
    /// order they were emitted in is the order they reach the log file.
    fn append_output(&self, service_id: &ServiceId, data: &str) {
        let sender = {
            let mut forwarders = self.log_forwarders.lock().unwrap();
            forwarders
                .entry(service_id.clone())
                .or_insert_with(|| {
                    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
                    let host = self.host.clone();
                    let service_id = service_id.clone();
                    tokio::spawn(async move {
                        while let Some(chunk) = rx.recv().await {
                            host.append_log(&service_id, &chunk).await;
                        }
                    });
                    tx
                })
                .clone()
        };
        let _ = sender.send(data.to_string());
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
        self.current_token(service_id) == token && self.state(service_id).map(|s| s.generation) == Some(generation)
    }

    fn identity_matches_state(&self, state: &ServiceLifecycleState) -> bool {
        match &state.identity {
            Some(ProcessIdentity::Posix(p)) => p.service_id == state.service_id && p.generation == state.generation,
            Some(ProcessIdentity::Docker(d)) => d.service_id == state.service_id && d.generation == state.generation,
            None => false,
        }
    }

    async fn owns(&self, state: &ServiceLifecycleState) -> bool {
        self.identity_matches_state(state) && self.owns_identity(state.identity.as_ref().unwrap()).await
    }

    async fn observed_matches(&self, identity: &ProcessIdentity) -> bool {
        let Some(observed) = self.options.process.inspect(identity).await else { return false };
        if !observed.alive || observed.record.command_fingerprint() != identity_fingerprint(identity) {
            return false;
        }
        match (identity, &observed.record) {
            (ProcessIdentity::Docker(id), ProcessRecord::Docker(rec)) => {
                rec.container_name == id.container_name && rec.container_id == id.container_id && rec.container_started_at == id.container_started_at
            }
            (ProcessIdentity::Posix(id), ProcessRecord::Posix(rec)) => rec.pid == id.pid && rec.pgid == id.pgid && rec.start_identity == id.start_identity,
            _ => false,
        }
    }

    async fn owns_identity(&self, identity: &ProcessIdentity) -> bool {
        identity_manager_instance(identity) == self.host.instance_id() && self.observed_matches(identity).await
    }

    async fn orphan(&self, state: &ServiceLifecycleState, operation_id: Option<String>) {
        self.transition(
            &state.service_id,
            state.generation,
            ActualServiceState::Orphaned,
            ServiceReadiness::Failed,
            Changes { error: Patch::Set("Process ownership identity no longer matches".to_string()), current_operation_id: operation_id.map(Patch::Set).unwrap_or(Patch::Keep), ..Default::default() },
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
            self.transition(service_id, generation, actual, readiness, changes).await;
        }
    }

    async fn transition(&self, service_id: &ServiceId, generation: u64, actual_state: ActualServiceState, readiness: ServiceReadiness, changes: Changes) {
        let previous = self.state(service_id);
        let now = self.options.clock.now();
        let next = ServiceLifecycleState {
            service_id: service_id.clone(),
            desired_state: changes.desired_state.unwrap_or_else(|| previous.as_ref().map(|p| p.desired_state).unwrap_or(DesiredServiceState::Running)),
            actual_state,
            readiness,
            generation,
            created_at: previous.as_ref().map(|p| p.created_at.clone()).unwrap_or_else(|| now.clone()),
            updated_at: now.clone(),
            identity: changes.identity.apply(previous.as_ref().and_then(|p| p.identity.clone())),
            readiness_kind: changes.readiness_kind.or_else(|| previous.as_ref().and_then(|p| p.readiness_kind)),
            readiness_detail: changes.readiness_detail.or_else(|| previous.as_ref().and_then(|p| p.readiness_detail.clone())),
            exited_at: changes.exited_at.apply(previous.as_ref().and_then(|p| p.exited_at.clone())),
            exit_code: changes.exit_code.apply(previous.as_ref().and_then(|p| p.exit_code)),
            error: changes.error.apply(previous.as_ref().and_then(|p| p.error.clone())),
            current_operation_id: changes.current_operation_id.apply(previous.as_ref().and_then(|p| p.current_operation_id.clone())),
        };
        self.host.set_service_state(next.clone()).await;
        self.host.publish(
            "service.lifecycle",
            serde_json::json!({
                "serviceId": next.service_id,
                "actualState": next.actual_state.as_wire_str(),
                "readiness": next.readiness,
                "generation": next.generation,
                "operationId": next.current_operation_id,
            }),
        );
        if let Some(error) = &next.error {
            self.host.append_log(service_id, &format!("{} {}\n", next.updated_at, error)).await;
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

fn output_source(identity: &ProcessIdentity) -> OutputSource<'_> {
    match identity {
        ProcessIdentity::Posix(_) => OutputSource::Process,
        ProcessIdentity::Docker(d) => OutputSource::Container {
            container_name: &d.container_name,
            since: Some(d.container_started_at.as_str()).filter(|s| !s.is_empty()),
            tail: None,
        },
    }
}

fn identity_manager_instance(identity: &ProcessIdentity) -> &str {
    match identity {
        ProcessIdentity::Posix(p) => &p.manager_instance_id,
        ProcessIdentity::Docker(d) => &d.manager_instance_id,
    }
}

fn identity_fingerprint(identity: &ProcessIdentity) -> &str {
    match identity {
        ProcessIdentity::Posix(p) => &p.command_fingerprint,
        ProcessIdentity::Docker(d) => &d.command_fingerprint,
    }
}

async fn run_ps(args: &[&str]) -> Result<String, std::io::Error> {
    let output = tokio::process::Command::new("ps").args(args).output().await?;
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

// =============================================================================================
// Tests — a representative (not exhaustive) port of `test/core/supervisor.test.ts`'s ~40
// scenarios, using fakes for every adapter. Fake pids are large synthetic numbers unlikely to
// collide with any real process on the machine running the test — `terminate()`'s process-tree
// walk still shells out to the REAL `ps` (see `process_tree`/`process_tree_alive` above, which are
// not behind `ProcessAdapter` in either this port or the original TS — that mirroring is
// intentional), so a fake pid correctly produces an empty tree there; `FakeProcessAdapter`'s own
// `signal_group` is what actually "kills" the simulated process for these unit tests.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::{
        CommandSpec, ReadinessSpec, ServiceBuildProfile, ServiceCommand, ServiceDefinition, ServiceKind, ServiceOwnership,
        ServiceProfiles, ServiceRunProfile, StartFailurePolicy,
    };
    use crate::state::{DesiredServiceState, PosixProcessIdentity, ServiceReadiness};
    use crate::supervisor::types::{
        DockerContainerRecord, Host, ManagedProcess, ObservedProcess, OnOutput, OutputSource, OutputTail,
        PortHolder, PosixProcessRecord, PreparationAdapter, ProbeAdapter, ProcessAdapter, ProcessRecord, RunBuild,
        SpawnInput, SupervisorClock, SupervisorError, SupervisorOptions,
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
            Arc::new(Self { millis: AtomicI64::new(1_700_000_000_000) })
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
    }
    struct FakeProcessAdapter {
        state: Arc<Mutex<FakeProcessAdapterState>>,
    }
    impl FakeProcessAdapter {
        fn new() -> Arc<Self> {
            Arc::new(Self { state: Arc::new(Mutex::new(FakeProcessAdapterState { next_pid: 900_001, ..Default::default() })) })
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
            self.state.lock().unwrap().tails_stopped.iter().any(|id| id == service_id)
        }
        fn stop_container_calls(&self) -> Vec<String> {
            self.state.lock().unwrap().stop_container_calls.clone()
        }
        fn fail_spawn(&self, service_id: &str, message: &str) {
            self.state.lock().unwrap().spawn_should_fail.insert(service_id.to_string(), message.to_string());
        }
        fn register_container(&self, name: &str, record: DockerContainerRecord) {
            self.state.lock().unwrap().container_records.insert(name.to_string(), record);
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
        fn mutate_fingerprint(&self, pid: i64, new_fingerprint: &str) {
            if let Some(record) = self.state.lock().unwrap().alive.get_mut(&pid) {
                record.command_fingerprint = new_fingerprint.to_string();
            }
        }
    }
    #[async_trait]
    impl ProcessAdapter for FakeProcessAdapter {
        async fn spawn(&self, input: SpawnInput, _on_output: OnOutput) -> Result<ManagedProcess, SupervisorError> {
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
                return Ok(ManagedProcess { record: ProcessRecord::Docker(record), exited: rx });
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
            Ok(ManagedProcess { record: ProcessRecord::Posix(record), exited: rx })
        }

        async fn inspect(&self, identity: &ProcessIdentity) -> Option<ObservedProcess> {
            let state = self.state.lock().unwrap();
            match identity {
                ProcessIdentity::Posix(id) => {
                    state.alive.get(&id.pid).cloned().map(|record| ObservedProcess { record: ProcessRecord::Posix(record), alive: true })
                }
                ProcessIdentity::Docker(id) => state
                    .container_records
                    .get(&id.container_name)
                    .cloned()
                    .map(|record| ObservedProcess { record: ProcessRecord::Docker(record), alive: true }),
            }
        }

        async fn signal_group(&self, pgid: i64, signal: ProcessSignal) {
            let mut state = self.state.lock().unwrap();
            state.signals.push((pgid, signal));
            // Our fake processes are always their own group leader (pid == pgid), so the "whole
            // process tree" for these unit tests is just the one process.
            if let Some(_record) = state.alive.remove(&pgid) {
                if let Some(tx) = state.exit_senders.remove(&pgid) {
                    let _ = tx.send(if signal == ProcessSignal::Sigterm { 143 } else { 137 });
                }
            }
        }

        async fn signal_pid(&self, pid: i64, expected_start_identity: &str, signal: ProcessSignal) {
            let mut state = self.state.lock().unwrap();
            state.pid_signals.push((pid, signal));
            // Same contract as the real adapter: a pid that no longer presents the resolved lstart
            // is not ours to signal — reclaim can never kill a recycled process in the fake either.
            let survives = state.unkillable.contains(&pid) || (state.term_immune.contains(&pid) && signal == ProcessSignal::Sigterm);
            if state.alive.get(&pid).map(|r| r.start_identity.as_str()) == Some(expected_start_identity) && !survives {
                state.alive.remove(&pid);
            }
        }

        async fn stop_container(&self, command: &ServiceCommand, _on_output: OnOutput) -> Option<Result<(), SupervisorError>> {
            let name = command.container_name.clone()?;
            let mut state = self.state.lock().unwrap();
            state.stop_container_calls.push(name.clone());
            state.container_records.remove(&name);
            Some(Ok(()))
        }

        fn attach_output(&self, service_id: &ServiceId, source: OutputSource<'_>, _on_output: OnOutput) -> Option<OutputTail> {
            let kind = match source {
                OutputSource::Process => "process",
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
            Arc::new(Self { state: Mutex::new(FakeProbeAdapterState::default()), process_state: Arc::new(Mutex::new(FakeProcessAdapterState::default())) })
        }
        fn with_process(process_state: Arc<Mutex<FakeProcessAdapterState>>) -> Arc<Self> {
            Arc::new(Self { state: Mutex::new(FakeProbeAdapterState::default()), process_state })
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
                PosixProcessRecord { pid, pgid: pid, start_identity: start_identity.to_string(), command_fingerprint: command.to_string() },
            );
            self.state
                .lock()
                .unwrap()
                .port_holders
                .entry(port)
                .or_default()
                .push(PortHolder { pid, pgid: pid, start_identity: start_identity.to_string(), command: command.to_string() });
        }
        fn set_container_ready(&self, name: &str, ready: bool) {
            self.state.lock().unwrap().container_ready.insert(name.to_string(), ready);
        }
    }
    #[async_trait]
    impl ProbeAdapter for FakeProbeAdapter {
        async fn tcp(&self, port: u16) -> bool {
            self.state.lock().unwrap().tcp_ready.get(&port).copied().unwrap_or(false)
        }
        async fn http(&self, _url: &str) -> bool {
            false
        }
        async fn container(&self, container_name: &str) -> bool {
            self.state.lock().unwrap().container_ready.get(container_name).copied().unwrap_or(false)
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
            self.timeline.lock().unwrap().push(std::time::Instant::now());
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
            Arc::new(Self { should_fail: Mutex::new(Default::default()), started_at: Mutex::new(Vec::new()), hang_until_cancelled: Mutex::new(false) })
        }
        fn fail_for(&self, shell_text: &str) {
            self.should_fail.lock().unwrap().insert(shell_text.to_string());
        }
        fn timeline(&self) -> Vec<(String, std::time::Instant)> {
            self.started_at.lock().unwrap().clone()
        }
    }
    #[async_trait]
    impl RunBuild for FakeRunBuild {
        async fn run(&self, command: &ServiceCommand, _on_output: OnOutput, cancel: CancellationToken) -> Result<(), SupervisorError> {
            let text = match &command.command {
                CommandSpec::Shell { shell, .. } => shell.clone(),
                CommandSpec::Argv { argv } => argv.join(" "),
            };
            self.started_at.lock().unwrap().push((text.clone(), std::time::Instant::now()));
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
        async fn prepare(&self, _service_id: &ServiceId, _steps: &[String]) -> Result<(), SupervisorError> {
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
            self.states.lock().unwrap().insert(state.service_id.clone(), state);
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
                    states.get(&s.id).cloned().unwrap_or_else(|| default_stopped_state(&s.id))
                })
                .collect()
        }
        async fn set_service_state(&self, next: ServiceLifecycleState) {
            self.states.lock().unwrap().insert(next.service_id.clone(), next);
        }
        async fn append_log(&self, service_id: &str, data: &str) {
            self.logs.lock().unwrap().push((service_id.to_string(), data.to_string()));
        }
        fn publish(&self, event_type: &str, data: serde_json::Value) {
            self.events.lock().unwrap().push((event_type.to_string(), data));
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
            profiles: ServiceProfiles {
                run: ServiceRunProfile::Verified {
                    command: ServiceCommand {
                        command: CommandSpec::Argv { argv: vec![id.to_string()] },
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
        }
    }

    fn with_preparation_command(mut service: ServiceDefinition, prep: crate::catalog::PreparationCommand) -> ServiceDefinition {
        if let ServiceRunProfile::Verified { preparation_command, .. } = &mut service.profiles.run {
            *preparation_command = Some(prep);
        }
        service
    }

    fn one_service_catalog(service: ServiceDefinition) -> ServiceCatalog {
        ServiceCatalog {
            services: vec![service],
            groups: HashMap::new(),
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
        #[allow(dead_code)] // available for tests that need to fast-forward the virtual clock directly
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
            clock: clock.clone(),
            readiness_timeout_ms,
            readiness_backoff_ms: 5,
            termination_grace_ms: 50,
            is_closing: Arc::new(|| false),
        };
        let supervisor = ProcessSupervisor::new(host.clone(), options);
        Harness { supervisor, host, process, probes, run_build, clock }
    }

    #[tokio::test]
    async fn starts_a_stopped_service_to_ready_with_tcp_readiness() {
        let h = build_harness(one_service_catalog(argv_verified("api", ReadinessSpec::Tcp { port: 8080 })));
        h.probes.set_tcp_ready(8080, true);
        h.supervisor.start(&"api".to_string(), None).await.unwrap();
        let state = h.host.state_of("api").unwrap();
        assert_eq!(state.actual_state, ActualServiceState::Ready);
        assert_eq!(state.readiness, ServiceReadiness::Ready);
        assert!(state.identity.is_some());
    }

    #[tokio::test]
    async fn process_readiness_reports_running_unready_as_the_terminal_state() {
        let h = build_harness(one_service_catalog(argv_verified("api", ReadinessSpec::Process)));
        h.supervisor.start(&"api".to_string(), None).await.unwrap();
        let state = h.host.state_of("api").unwrap();
        assert_eq!(state.actual_state, ActualServiceState::RunningUnready);
        assert_eq!(state.readiness_detail.as_deref(), Some("process-liveness-only"));
    }

    #[tokio::test]
    async fn restart_stops_then_starts() {
        let h = build_harness(one_service_catalog(argv_verified("api", ReadinessSpec::Tcp { port: 8080 })));
        h.probes.set_tcp_ready(8080, true);
        h.supervisor.start(&"api".to_string(), None).await.unwrap();
        let first_pid = match h.host.state_of("api").unwrap().identity.unwrap() {
            ProcessIdentity::Posix(p) => p.pid,
            _ => panic!("expected posix identity"),
        };
        h.supervisor.restart(&"api".to_string(), None).await.unwrap();
        let second_pid = match h.host.state_of("api").unwrap().identity.unwrap() {
            ProcessIdentity::Posix(p) => p.pid,
            _ => panic!("expected posix identity"),
        };
        assert_ne!(first_pid, second_pid, "restart should spawn a fresh process");
        assert!(h.process.signals().iter().any(|(pgid, sig)| *pgid == first_pid && *sig == ProcessSignal::Sigterm));
    }

    #[tokio::test]
    async fn stop_on_an_already_stopped_service_is_a_noop() {
        let h = build_harness(one_service_catalog(argv_verified("api", ReadinessSpec::Tcp { port: 8080 })));
        h.supervisor.stop(&"api".to_string(), None).await.unwrap();
        // A true no-op: `stop_locked` returns immediately for an already-stopped service without
        // ever calling `set_service_state`, so nothing is written to the host's state map at all —
        // check the synthesized default via `service_states()` instead of the raw map.
        let state = h.host.service_states().into_iter().find(|s| s.service_id == "api").unwrap();
        assert_eq!(state.actual_state, ActualServiceState::Stopped);
        assert!(h.host.state_of("api").is_none(), "a true no-op must never write to persisted state");
        assert!(h.process.signals().is_empty());
    }

    /// An adopted `ownership: external` unit carries no process identity, so the catalog's `stop:`
    /// command is the only thing that can stop it. Stop used to fall through to `orphan()` —
    /// recording "Process ownership identity no longer matches" for a service that never had an
    /// identity — while the container kept running: exactly what "I press Stop and nothing happens"
    /// looked like in the app.
    #[tokio::test]
    async fn stop_on_an_adopted_external_service_runs_its_stop_command() {
        let h = build_harness(one_service_catalog(external_container_service("cache", "proj-cache", Some("docker compose stop cache"))));
        h.host.seed(adopted_external_state("cache"));

        h.supervisor.stop(&"cache".to_string(), None).await.unwrap();

        assert_eq!(h.process.stop_container_calls(), vec!["proj-cache".to_string()], "the catalog's stop command must actually run");
        let state = h.host.state_of("cache").unwrap();
        assert_eq!(state.actual_state, ActualServiceState::Stopped);
        assert_eq!(state.desired_state, DesiredServiceState::Stopped);
        assert!(state.error.is_none(), "a successful stop must not leave the old error behind");
    }

    /// The other half of the same bug: a service whose port is held by a process this manager does
    /// not own has nothing to stop and no `stop:` command to run. Stop must say so — it used to
    /// report success and change nothing.
    #[tokio::test]
    async fn stop_on_an_externally_owned_service_without_a_stop_command_fails_loudly() {
        let h = build_harness(one_service_catalog(argv_verified("api", ReadinessSpec::Tcp { port: 8080 })));
        h.probes.set_port_in_use(8080, true);
        let start_error = h.supervisor.start(&"api".to_string(), None).await.unwrap_err();
        assert!(start_error.0.contains("externally owned"), "{start_error:?}");

        let error = h.supervisor.stop(&"api".to_string(), None).await.unwrap_err();

        assert!(error.0.contains("cannot be stopped"), "{error:?}");
        assert!(error.0.contains("Port 8080 is held by an unowned process"), "{error:?}");
        assert!(error.0.contains("no `stop` command"), "{error:?}");
        let state = h.host.state_of("api").unwrap();
        assert_eq!(state.actual_state, ActualServiceState::ExternallyOwned, "a refused stop must not pretend the service is stopped");
    }

    /// An external unit with no `stop:` command cannot be stopped by this manager either — the same
    /// loud refusal, not a silent success that leaves the container running.
    #[tokio::test]
    async fn stop_on_an_external_service_without_a_stop_command_fails_loudly() {
        let h = build_harness(one_service_catalog(external_container_service("cache", "proj-cache", None)));
        h.host.seed(adopted_external_state("cache"));

        let error = h.supervisor.stop(&"cache".to_string(), None).await.unwrap_err();

        assert!(error.0.contains("cannot be stopped"), "{error:?}");
        assert!(error.0.contains("owned externally"), "{error:?}");
        assert!(h.process.stop_container_calls().is_empty(), "nothing may be run when the catalog declares no stop command");
        assert_eq!(h.host.state_of("cache").unwrap().actual_state, ActualServiceState::Ready);
    }

    /// A stop must never report success for a process it cannot touch: an identity that no longer
    /// matches (a reused pid, or a program that replaced it) is alive, but killing it would be
    /// killing someone else's process.
    #[tokio::test]
    async fn stop_on_an_orphaned_service_refuses_instead_of_reporting_success() {
        let h = build_harness(one_service_catalog(argv_verified("api", ReadinessSpec::Tcp { port: 8080 })));
        h.probes.set_tcp_ready(8080, true);
        h.supervisor.start(&"api".to_string(), None).await.unwrap();
        let pid = match h.host.state_of("api").unwrap().identity.unwrap() {
            ProcessIdentity::Posix(posix) => posix.pid,
            _ => panic!("expected a posix identity"),
        };
        // The pid is still alive, but `ps` now reports a different program at it.
        h.process.mutate_fingerprint(pid, "not-the-same-program-anymore");

        let error = h.supervisor.stop(&"api".to_string(), None).await.unwrap_err();

        assert!(error.0.contains("cannot be stopped"), "{error:?}");
        assert!(error.0.contains(&format!("pid {pid}")), "{error:?}");
        assert!(h.process.signals().is_empty(), "a process this manager does not own must never be signalled");
        assert_eq!(h.host.state_of("api").unwrap().actual_state, ActualServiceState::Orphaned);
    }

    #[tokio::test]
    async fn readiness_timeout_transitions_to_failed_with_detail() {
        let h = build_harness_with_timeout(one_service_catalog(argv_verified("api", ReadinessSpec::Tcp { port: 8080 })), 30);
        // tcp never reports ready — probes.set_tcp_ready is never called for 8080.
        let err = h.supervisor.start(&"api".to_string(), None).await.unwrap_err();
        assert_eq!(err.0, "Readiness timed out");
        let state = h.host.state_of("api").unwrap();
        assert_eq!(state.actual_state, ActualServiceState::Failed);
        assert!(state.readiness_detail.as_deref().unwrap().contains("timed out"));
    }

    #[tokio::test]
    async fn tcp_port_conflict_refuses_to_start() {
        let h = build_harness(one_service_catalog(argv_verified("api", ReadinessSpec::Tcp { port: 8080 })));
        h.probes.set_port_in_use(8080, true);
        let err = h.supervisor.start(&"api".to_string(), None).await.unwrap_err();
        assert!(err.0.contains("externally owned"));
        let state = h.host.state_of("api").unwrap();
        assert_eq!(state.actual_state, ActualServiceState::ExternallyOwned);
        assert!(state.error.as_deref().unwrap().contains("Port 8080 is held"));
        assert!(h.process.signals().is_empty(), "must never have spawned anything");
    }

    /// The refusal error must name the squatter — `pid (command)` — so the client can show the
    /// user exactly what would be killed before they confirm.
    #[tokio::test]
    async fn tcp_port_conflict_names_the_holder_in_the_error() {
        let h = build_harness(one_service_catalog(argv_verified("api", ReadinessSpec::Tcp { port: 8080 })));
        h.probes.set_port_holder(8080, 41_234, "Fake Jan  1 00:00:01 2024", "node dist/main");
        let err = h.supervisor.start(&"api".to_string(), None).await.unwrap_err();
        assert!(err.0.contains("externally owned"));
        let state = h.host.state_of("api").unwrap();
        assert_eq!(state.actual_state, ActualServiceState::ExternallyOwned);
        assert_eq!(state.error.as_deref().unwrap(), "Port 8080 is held by pid 41234 (node dist/main)");
        assert!(h.process.pid_signals().is_empty(), "without kill_unowned nothing may be signalled");
    }

    /// `killUnowned` is the client's echo of the user's "yes": the squatter gets SIGTERM, the port
    /// frees, and the start continues into the normal spawn/readiness path.
    #[tokio::test]
    async fn start_with_kill_unowned_terminates_the_holder_and_continues() {
        let h = build_harness(one_service_catalog(argv_verified("api", ReadinessSpec::Tcp { port: 8080 })));
        h.probes.set_port_holder(8080, 41_234, "Fake Jan  1 00:00:01 2024", "node dist/main");
        h.probes.set_tcp_ready(8080, true);
        h.supervisor
            .start_with_options(&"api".to_string(), None, StartOptions { kill_unowned: true })
            .await
            .unwrap();
        assert_eq!(h.process.pid_signals(), vec![(41_234, ProcessSignal::Sigterm)]);
        assert!(!h.process.state.lock().unwrap().alive.contains_key(&41_234), "the squatter must be dead");
        let state = h.host.state_of("api").unwrap();
        assert_eq!(state.actual_state, ActualServiceState::Ready);
        assert!(state.identity.is_some(), "the service itself must have spawned after the port freed");
    }

    /// A squatter that ignores SIGTERM is escalated to SIGKILL on a second pass — holders are
    /// re-resolved, so the kill goes to the still-alive pid, not the stale first-pass record.
    #[tokio::test]
    async fn reclaim_escalates_to_sigkill_for_a_sigterm_immune_holder() {
        let h = build_harness(one_service_catalog(argv_verified("api", ReadinessSpec::Tcp { port: 8080 })));
        h.probes.set_port_holder(8080, 41_234, "Fake Jan  1 00:00:01 2024", "node dist/main");
        h.probes.set_tcp_ready(8080, true);
        h.process.mark_term_immune(41_234);
        h.supervisor
            .start_with_options(&"api".to_string(), None, StartOptions { kill_unowned: true })
            .await
            .unwrap();
        assert_eq!(
            h.process.pid_signals(),
            vec![(41_234, ProcessSignal::Sigterm), (41_234, ProcessSignal::Sigkill)]
        );
        assert_eq!(h.host.state_of("api").unwrap().actual_state, ActualServiceState::Ready);
    }

    /// If the holder still owns the port after SIGKILL the start must fail loudly — the service
    /// stays `externally-owned` instead of silently dropping the user's confirmed kill.
    #[tokio::test]
    async fn reclaim_fails_loudly_when_the_holder_survives_sigkill() {
        let h = build_harness(one_service_catalog(argv_verified("api", ReadinessSpec::Tcp { port: 8080 })));
        h.probes.set_port_holder(8080, 41_234, "Fake Jan  1 00:00:01 2024", "node dist/main");
        h.process.mark_unkillable(41_234);
        let err = h
            .supervisor
            .start_with_options(&"api".to_string(), None, StartOptions { kill_unowned: true })
            .await
            .unwrap_err();
        assert!(err.0.contains("still held"), "{err:?}");
        let state = h.host.state_of("api").unwrap();
        assert_eq!(state.actual_state, ActualServiceState::ExternallyOwned);
        assert!(state.identity.is_none(), "the service must never have spawned");
    }

    /// A pid that no longer presents the resolved lstart is not ours to kill — `signal_pid`
    /// re-verifies the identity before signalling, so a recycled pid survives reclaim untouched
    /// and the start fails closed.
    #[tokio::test]
    async fn reclaim_never_kills_a_holder_whose_start_identity_no_longer_matches() {
        let h = build_harness(one_service_catalog(argv_verified("api", ReadinessSpec::Tcp { port: 8080 })));
        h.probes.set_port_holder(8080, 41_234, "original lstart", "node dist/main");
        // The squatter died and its pid was recycled by something else before the signal landed.
        h.probes.process_state.lock().unwrap().alive.get_mut(&41_234).unwrap().start_identity = "recycled lstart".to_string();
        let err = h
            .supervisor
            .start_with_options(&"api".to_string(), None, StartOptions { kill_unowned: true })
            .await
            .unwrap_err();
        assert!(err.0.contains("still held"), "{err:?}");
        assert!(h.process.pid_signals().iter().all(|(pid, _)| *pid == 41_234));
        assert!(h.probes.process_state.lock().unwrap().alive.contains_key(&41_234), "a recycled pid must survive reclaim");
    }

    #[tokio::test]
    async fn reclaim_terminates_every_holder_of_the_port() {
        let h = build_harness(one_service_catalog(argv_verified("api", ReadinessSpec::Tcp { port: 8080 })));
        h.probes.set_port_holder(8080, 41_234, "Fake Jan  1 00:00:01 2024", "node a");
        h.probes.set_port_holder(8080, 41_235, "Fake Jan  1 00:00:02 2024", "node b");
        h.probes.set_tcp_ready(8080, true);
        h.supervisor
            .start_with_options(&"api".to_string(), None, StartOptions { kill_unowned: true })
            .await
            .unwrap();
        assert_eq!(
            h.process.pid_signals(),
            vec![(41_234, ProcessSignal::Sigterm), (41_235, ProcessSignal::Sigterm)]
        );
        assert_eq!(h.host.state_of("api").unwrap().actual_state, ActualServiceState::Ready);
    }

    /// `port_holders` returning `None` means the adapter cannot resolve who holds the port —
    /// reclaim must fail closed rather than signalling a pid it guessed.
    #[tokio::test]
    async fn reclaim_fails_closed_when_the_probe_cannot_resolve_holders() {
        let h = build_harness(one_service_catalog(argv_verified("api", ReadinessSpec::Tcp { port: 8080 })));
        h.probes.set_port_in_use(8080, true);
        h.probes.set_port_holders_unsupported();
        let err = h
            .supervisor
            .start_with_options(&"api".to_string(), None, StartOptions { kill_unowned: true })
            .await
            .unwrap_err();
        assert!(err.0.contains("still held by an unowned process"), "{err:?}");
        assert!(h.process.pid_signals().is_empty(), "an unresolvable holder must never be signalled");
        assert_eq!(h.host.state_of("api").unwrap().actual_state, ActualServiceState::ExternallyOwned);
    }

    /// Restart always uses default options — it can end up `externally-owned`, but it must never
    /// signal a squatter. Reclaim only ever comes from an explicit `killUnowned` start.
    #[tokio::test]
    async fn restart_never_reclaims_an_occupied_port() {
        let h = build_harness(one_service_catalog(argv_verified("api", ReadinessSpec::Tcp { port: 8080 })));
        h.probes.set_port_holder(8080, 41_234, "Fake Jan  1 00:00:01 2024", "node dist/main");
        let err = h.supervisor.restart(&"api".to_string(), None).await.unwrap_err();
        assert!(err.0.contains("externally owned"), "{err:?}");
        assert!(h.process.pid_signals().is_empty());
        assert_eq!(h.host.state_of("api").unwrap().actual_state, ActualServiceState::ExternallyOwned);
    }

    #[tokio::test]
    async fn shutdown_reaps_a_persisted_identity_in_a_non_active_state() {
        let h = build_harness(one_service_catalog(argv_verified("api", ReadinessSpec::Tcp { port: 8080 })));
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
                PosixProcessRecord { pid: 900_001, pgid: 900_001, start_identity: "Fake Jan  1 00:00:01 2024".to_string(), command_fingerprint: normalize_command_fingerprint(&argv_command_for_test("api")) },
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
            h.process.signals().iter().any(|(pgid, sig)| *pgid == 900_001 && *sig == ProcessSignal::Sigterm),
            "shutdown's second pass must reap a persisted identity even in a non-active state"
        );
    }

    fn argv_command_for_test(id: &str) -> ServiceCommand {
        ServiceCommand { command: CommandSpec::Argv { argv: vec![id.to_string()] }, cwd: ".".to_string(), environment: None, container_name: None, docker_stop_command: None }
    }

    /// An `ownership: external` container unit, shaped like a real one (`viclass`'s mongo/redis/…):
    /// a `docker compose up -d` run command, a container readiness probe, and — optionally — the
    /// `stop:` command that is the only way this manager can stop it.
    fn external_container_service(id: &str, container: &str, stop: Option<&str>) -> ServiceDefinition {
        let mut service = argv_verified(id, ReadinessSpec::Container);
        service.ownership = Some(ServiceOwnership::External);
        service.profiles.run = ServiceRunProfile::Verified {
            command: ServiceCommand {
                command: CommandSpec::Shell { shell: format!("docker compose up -d {container}"), exec: None },
                cwd: ".".to_string(),
                environment: None,
                container_name: Some(container.to_string()),
                docker_stop_command: stop.map(|shell| CommandSpec::Shell { shell: shell.to_string(), exec: None }),
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
        let h = build_harness(one_service_catalog(argv_verified("api", ReadinessSpec::Tcp { port: 8080 })));
        h.probes.set_tcp_ready(8080, true);
        h.supervisor.start(&"api".to_string(), None).await.unwrap();
        let pid = match h.host.state_of("api").unwrap().identity.unwrap() {
            ProcessIdentity::Posix(p) => p.pid,
            _ => panic!(),
        };
        // Simulate a pid-reuse-like scenario: the OS-observed process at this pid now has a
        // different command fingerprint (a different program entirely), so `observed_matches` must
        // fail and the service must be marked orphaned rather than treated as still-ours.
        h.process.mutate_fingerprint(pid, "not-the-same-program-anymore");
        h.supervisor.status(&"api".to_string()).await.unwrap();
        let state = h.host.state_of("api").unwrap();
        assert_eq!(state.actual_state, ActualServiceState::Orphaned);
        assert_eq!(state.error.as_deref(), Some("Process ownership identity no longer matches"));
    }

    #[tokio::test]
    async fn build_failure_short_circuits_spawn() {
        let mut service = argv_verified("api", ReadinessSpec::Tcp { port: 8080 });
        service.profiles.build = Some(ServiceBuildProfile {
            command: ServiceCommand { command: CommandSpec::Shell { shell: "build-that-fails".to_string(), exec: None }, cwd: ".".to_string(), environment: None, container_name: None, docker_stop_command: None },
            timeout_ms: None,
            serialization_key: None,
        });
        let h = build_harness(one_service_catalog(service));
        h.run_build.fail_for("build-that-fails");
        h.probes.set_tcp_ready(8080, true);

        let err = h.supervisor.start(&"api".to_string(), None).await.unwrap_err();
        assert_eq!(err.0, "Build failed");
        let state = h.host.state_of("api").unwrap();
        assert_eq!(state.actual_state, ActualServiceState::Failed);
        assert!(h.process.signals().is_empty(), "spawn must never be attempted after a build failure");
    }

    #[tokio::test]
    async fn build_serialization_by_key_runs_one_at_a_time() {
        fn service_with_build(id: &str, key: &str) -> ServiceDefinition {
            let mut service = argv_verified(id, ReadinessSpec::Tcp { port: 8080 });
            service.profiles.build = Some(ServiceBuildProfile {
                command: ServiceCommand { command: CommandSpec::Shell { shell: format!("build-{id}"), exec: None }, cwd: ".".to_string(), environment: None, container_name: None, docker_stop_command: None },
                timeout_ms: None,
                serialization_key: Some(key.to_string()),
            });
            service
        }
        let catalog = ServiceCatalog {
            services: vec![service_with_build("a", "shared"), service_with_build("b", "shared")],
            groups: HashMap::new(),
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
        assert!(gap >= std::time::Duration::from_millis(4), "builds sharing a serializationKey must not overlap, gap was {gap:?}");
    }

    #[tokio::test]
    async fn command_readiness_with_no_adapter_never_throws_times_out_normally() {
        let readiness = ReadinessSpec::Command { command: CommandSpec::Argv { argv: vec!["check".to_string()] }, cwd: None };
        let h = build_harness_with_timeout(one_service_catalog(argv_verified("api", readiness)), 20);
        let err = h.supervisor.start(&"api".to_string(), None).await.unwrap_err();
        assert_eq!(err.0, "Readiness timed out");
    }

    #[tokio::test]
    async fn command_readiness_succeeds_when_the_adapter_reports_ready() {
        let host = FakeHost::new(one_service_catalog(argv_verified(
            "api",
            ReadinessSpec::Command { command: CommandSpec::Argv { argv: vec!["check".to_string()] }, cwd: None },
        )));
        let process = FakeProcessAdapter::new();
        let inner_probes = FakeProbeAdapter::new();
        let probes = Arc::new(CommandCapableProbeAdapter { inner: Some(inner_probes), result: std::sync::Mutex::new(Some(true)), ..Default::default() });
        let run_build = FakeRunBuild::new();
        let clock = FakeClock::new();
        let supervisor = ProcessSupervisor::new(
            host.clone(),
            SupervisorOptions {
                process,
                run_build,
                probes,
                preparation: Some(Arc::new(NoPreparation)),
                clock,
                readiness_timeout_ms: 200,
                readiness_backoff_ms: 5,
                termination_grace_ms: 50,
                is_closing: Arc::new(|| false),
            },
        );
        supervisor.start(&"api".to_string(), None).await.unwrap();
        assert_eq!(host.state_of("api").unwrap().actual_state, ActualServiceState::Ready);
    }

    fn preparation_catalog() -> ServiceCatalog {
        let service = with_preparation_command(
            argv_verified("api", ReadinessSpec::Tcp { port: 8080 }),
            crate::catalog::PreparationCommand { command: CommandSpec::Argv { argv: vec!["prepare".to_string()] }, cwd: Some("infra".to_string()), serialization_key: None },
        );
        one_service_catalog(service)
    }

    #[tokio::test]
    async fn preparation_command_runs_before_start_and_succeeds_when_the_probe_reports_ready() {
        let host = FakeHost::new(preparation_catalog());
        let process = FakeProcessAdapter::new();
        let inner_probes = FakeProbeAdapter::new();
        inner_probes.set_tcp_ready(8080, true);
        let probes = Arc::new(CommandCapableProbeAdapter { inner: Some(inner_probes), result: std::sync::Mutex::new(Some(true)), ..Default::default() });
        let run_build = FakeRunBuild::new();
        let clock = FakeClock::new();
        let supervisor = ProcessSupervisor::new(
            host.clone(),
            SupervisorOptions { process, run_build, probes, preparation: Some(Arc::new(NoPreparation)), clock, readiness_timeout_ms: 200, readiness_backoff_ms: 5, termination_grace_ms: 50, is_closing: Arc::new(|| false) },
        );
        supervisor.start(&"api".to_string(), None).await.unwrap();
        assert_eq!(host.state_of("api").unwrap().actual_state, ActualServiceState::Ready);
    }

    #[tokio::test]
    async fn preparation_command_failure_fails_the_start_not_just_readiness() {
        let host = FakeHost::new(preparation_catalog());
        let process = FakeProcessAdapter::new();
        let inner_probes = FakeProbeAdapter::new();
        let probes = Arc::new(CommandCapableProbeAdapter { inner: Some(inner_probes), result: std::sync::Mutex::new(Some(false)), ..Default::default() });
        let run_build = FakeRunBuild::new();
        let clock = FakeClock::new();
        let supervisor = ProcessSupervisor::new(
            host.clone(),
            SupervisorOptions { process, run_build, probes, preparation: Some(Arc::new(NoPreparation)), clock, readiness_timeout_ms: 200, readiness_backoff_ms: 5, termination_grace_ms: 50, is_closing: Arc::new(|| false) },
        );
        let err = supervisor.start(&"api".to_string(), None).await.unwrap_err();
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
        let err = h.supervisor.start(&"api".to_string(), None).await.unwrap_err();
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
                crate::catalog::PreparationCommand { command: CommandSpec::Argv { argv: vec!["prepare".to_string()] }, cwd: None, serialization_key: Some(key.to_string()) },
            )
        }
        let catalog = ServiceCatalog {
            services: vec![service_with_prep("a", "shared"), service_with_prep("b", "shared")],
            groups: HashMap::new(),
            compose_file: None,
            runtime_directory: None,
            start_failure_policy: StartFailurePolicy::StopOnFirstFailureKeepStarted,
            private_file_guard: None,
        };
        let host = FakeHost::new(catalog);
        let process = FakeProcessAdapter::new();
        let probes = Arc::new(CommandCapableProbeAdapter { result: std::sync::Mutex::new(Some(true)), delay_ms: 5, ..Default::default() });
        let run_build = FakeRunBuild::new();
        let clock = FakeClock::new();
        let supervisor = ProcessSupervisor::new(
            host.clone(),
            SupervisorOptions { process, run_build, probes: probes.clone(), preparation: Some(Arc::new(NoPreparation)), clock, readiness_timeout_ms: 200, readiness_backoff_ms: 5, termination_grace_ms: 50, is_closing: Arc::new(|| false) },
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
        assert!(gap >= std::time::Duration::from_millis(4), "preparation commands sharing a serializationKey must not overlap, gap was {gap:?}");
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
        assert_eq!(state.readiness_detail.as_deref(), Some("adopted from external state"));
        assert!(state.identity.is_none(), "external services never get a ProcessIdentity");

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
                command: CommandSpec::Shell { shell: "docker compose up db".to_string(), exec: None },
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
                    command: CommandSpec::Shell { shell: "docker compose up db".to_string(), exec: None },
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
            h.process.attach_calls().contains(&("db".to_string(), "container")),
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
                command: CommandSpec::Argv { argv: vec!["docker".to_string(), "compose".to_string(), "up".to_string()] },
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
        assert_eq!(h.host.state_of("cache").unwrap().actual_state, ActualServiceState::Ready);
        assert_eq!(h.process.attach_calls(), vec![("cache".to_string(), "container")]);

        // `sync_external_services` polls on an interval — an already-attached live tail must not be
        // replaced every tick (a container tail replays its backlog on attach, so a naive re-attach
        // would duplicate log lines each poll).
        h.supervisor.sync_external_services().await;
        assert_eq!(h.process.attach_calls().len(), 1, "a live tail must not be re-attached");

        // A follower that ended on its own (container restarted) is re-attached on the next tick.
        h.process.finish_tail("cache");
        h.supervisor.sync_external_services().await;
        assert_eq!(h.process.attach_calls().len(), 2, "a dead tail must be re-attached");

        h.probes.set_container_ready("proj-cache", false);
        h.supervisor.sync_external_services().await;
        assert_eq!(h.host.state_of("cache").unwrap().actual_state, ActualServiceState::Stopped);
        assert!(h.process.tail_was_stopped("cache"), "release must stop the log tail");
    }

    #[tokio::test]
    async fn spawn_failure_transitions_to_failed_with_the_adapter_error() {
        let h = build_harness(one_service_catalog(argv_verified("api", ReadinessSpec::Tcp { port: 8080 })));
        h.process.fail_spawn("api", "no such file or directory");
        let err = h.supervisor.start(&"api".to_string(), None).await.unwrap_err();
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
        let mut service = argv_verified("api", ReadinessSpec::Command { command: CommandSpec::Argv { argv: vec!["attach".to_string()] }, cwd: None });
        service.ownership = Some(ServiceOwnership::External);
        let host = FakeHost::new(one_service_catalog(service));
        let process = FakeProcessAdapter::new();
        let probes = Arc::new(CommandCapableProbeAdapter { result: std::sync::Mutex::new(Some(true)), ..Default::default() });
        let supervisor = ProcessSupervisor::new(
            host.clone(),
            SupervisorOptions {
                process: process.clone(),
                run_build: FakeRunBuild::new(),
                probes,
                preparation: Some(Arc::new(NoPreparation)),
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
        assert!(state.identity.is_none(), "a task run must not track a process identity");
        assert!(process.state.lock().unwrap().alive.len() == 1, "the attach task must still be spawned once");
    }

    #[tokio::test]
    async fn reconcile_marks_an_externally_killed_process_as_failed() {
        let h = build_harness(one_service_catalog(argv_verified("api", ReadinessSpec::Tcp { port: 8080 })));
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
        assert_eq!(state.error.as_deref(), Some("Managed process is no longer alive"));
    }
}
