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
use super::types::{Host, ManagedProcess, OnOutput, ProcessRecord, ProcessSignal, SpawnInput, SupervisorError, SupervisorOptions};

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

/// A "task" command has no long-lived managed process to track — it runs once to bring external
/// state into the desired shape (generalizes a tailnet-serve-config runner).
fn is_task_command(readiness: &ReadinessSpec) -> bool {
    matches!(readiness, ReadinessSpec::Tailnet)
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
    output_tails: Mutex<HashMap<ServiceId, Box<dyn FnOnce() + Send>>>,
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
        let sup = self.clone();
        let id = service_id.clone();
        self.queues.run(service_id, || async move { sup.start_locked(&id, operation_id).await }).await
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
                sup.start_locked(&id, op).await
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
                    sup.attach_output(&service_id, &updated_identity);
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
                    let ServiceRunProfile::Verified { readiness, .. } = &service.profiles.run else { return };
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
                    } else if !ready {
                        if let Some(state) = &state {
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
                            }
                        }
                    }
                })
                .await;
        }
    }

    // ---------------------------------------------------------------------------------------
    // Locked start/stop
    // ---------------------------------------------------------------------------------------

    async fn start_locked(self: &Arc<Self>, service_id: &ServiceId, operation_id: Option<String>) -> Result<(), SupervisorError> {
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
            self.attach_output(service_id, &identity);
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
        // express "run this before starting" in a catalog that has no closures (`.config.ts`/YAML).
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
                self.transition(
                    service_id,
                    generation,
                    ActualServiceState::ExternallyOwned,
                    ServiceReadiness::Failed,
                    Changes { error: Patch::Set(format!("Port {port} is held by an unowned process")), ..Default::default() },
                )
                .await;
                return Err(SupervisorError(format!("Port {port} is externally owned")));
            }
        }
        self.spawn_and_wait(service_id, &profile, generation, token, operation_id).await
    }

    async fn stop_locked(self: &Arc<Self>, service_id: &ServiceId, operation_id: Option<String>) -> Result<(), SupervisorError> {
        let Some(state) = self.state(service_id) else { return Ok(()) };
        if matches!(state.actual_state, ActualServiceState::Stopped | ActualServiceState::ExternallyOwned) {
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
            } else {
                self.orphan(&state, operation_id).await;
            }
            return Ok(());
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
            self.orphan(&state, operation_id).await;
            return Ok(());
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

        if is_task_command(&profile.readiness) && !app.record.is_docker() {
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
        self.attach_output(service_id, &identity);
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
            ProcessIdentity::Docker(_) => {
                let sink: OnOutput = Arc::new(|_| {});
                match self.options.process.stop_container(command, sink).await {
                    Some(result) => result?,
                    None => return Err(SupervisorError("Docker container stop is unavailable".to_string())),
                }
            }
            ProcessIdentity::Posix(posix) => {
                if let Some(stop) = self.output_tails.lock().unwrap().remove(&posix.service_id) {
                    stop();
                }
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

    fn attach_output(self: &Arc<Self>, service_id: &ServiceId, identity: &ProcessIdentity) {
        if matches!(identity, ProcessIdentity::Docker(_)) {
            return;
        }
        if let Some(stop) = self.output_tails.lock().unwrap().remove(service_id) {
            stop();
        }
        // `attach_output` is one of the "optional" ProcessAdapter methods (default adapters not yet
        // ported) — a `None` result (no attachment support) is a normal, expected outcome here.
        let on_output: OnOutput = {
            let this = self.clone();
            let sid = service_id.clone();
            Arc::new(move |data: &str| {
                this.append_output(&sid, data);
            })
        };
        if let Some(stop) = self.options.process.attach_output(service_id, on_output) {
            self.output_tails.lock().unwrap().insert(service_id.clone(), stop);
        }
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
        CommandSpec, ReadinessSpec, ServiceBuildProfile, ServiceCommand, ServiceDefinition, ServiceKind,
        ServiceProfiles, ServiceRunProfile, StartFailurePolicy,
    };
    use crate::state::{DesiredServiceState, PosixProcessIdentity, ServiceReadiness};
    use crate::supervisor::types::{
        DockerContainerRecord, Host, ManagedProcess, ObservedProcess, OnOutput, PosixProcessRecord,
        PreparationAdapter, ProbeAdapter, ProcessAdapter, ProcessRecord, RunBuild, SpawnInput, SupervisorClock,
        SupervisorError, SupervisorOptions,
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
        spawn_should_fail: HashMap<ServiceId, String>,
        container_records: HashMap<String, DockerContainerRecord>,
        exit_senders: HashMap<i64, oneshot::Sender<i32>>,
    }
    struct FakeProcessAdapter {
        state: Mutex<FakeProcessAdapterState>,
    }
    impl FakeProcessAdapter {
        fn new() -> Arc<Self> {
            Arc::new(Self { state: Mutex::new(FakeProcessAdapterState { next_pid: 900_001, ..Default::default() }) })
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

        async fn stop_container(&self, command: &ServiceCommand, _on_output: OnOutput) -> Option<Result<(), SupervisorError>> {
            let name = command.container_name.clone()?;
            self.state.lock().unwrap().container_records.remove(&name);
            Some(Ok(()))
        }
    }

    // --- Fake probe adapter ---
    #[derive(Default)]
    struct FakeProbeAdapterState {
        tcp_ready: HashMap<u16, bool>,
        port_in_use: HashMap<u16, bool>,
        container_ready: HashMap<String, bool>,
        tailnet_ready: bool,
    }
    struct FakeProbeAdapter {
        state: Mutex<FakeProbeAdapterState>,
    }
    impl FakeProbeAdapter {
        fn new() -> Arc<Self> {
            Arc::new(Self { state: Mutex::new(FakeProbeAdapterState::default()) })
        }
        fn set_tcp_ready(&self, port: u16, ready: bool) {
            self.state.lock().unwrap().tcp_ready.insert(port, ready);
        }
        fn set_port_in_use(&self, port: u16, in_use: bool) {
            self.state.lock().unwrap().port_in_use.insert(port, in_use);
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
            Some(self.state.lock().unwrap().port_in_use.get(&port).copied().unwrap_or(false))
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
        let probes = FakeProbeAdapter::new();
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
