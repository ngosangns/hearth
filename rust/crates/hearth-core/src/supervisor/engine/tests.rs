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
    /// `command` probe treats its run command as a one-shot task (`hearth shared attach`), not the
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

