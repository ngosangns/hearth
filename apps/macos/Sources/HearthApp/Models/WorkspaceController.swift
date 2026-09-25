import Foundation

/// One workspace's live connection state: ensures a daemon, holds the resulting `ManagerClient`, and
/// watches `/v1/events/stream` (falling back to polling `GET /v1/services`). Owned by
/// `WorkspaceControllerRegistry` at the app
/// level — one per workspace, created and torn down only by its `sync(_:)`, so a connection outlives
/// the detail view and the menu bar can read live status with no window open.
@MainActor
final class WorkspaceController: ObservableObject {
    enum Phase: Equatable {
        case idle
        case connecting
        case connected
        case failed(String)
        /// The daemon was deliberately stopped via `stopDaemon()` — everything is down and the
        /// view offers "Start Daemon" (a fresh `ensure`), not a retry of a failure.
        case stopped

        /// `.connected` and `.failed` are the phases where a daemon may still be running (a failed
        /// connection does not mean the daemon is dead) — `stopDaemon` is only offered then.
        var mayHaveLiveDaemon: Bool {
            switch self {
            case .connected, .failed: return true
            case .idle, .connecting, .stopped: return false
            }
        }
    }

    let workspace: Workspace
    @Published private(set) var phase: Phase = .idle
    @Published private(set) var services: [ServiceLifecycleState] = []
    @Published private(set) var catalog: ServiceCatalogSummary?
    /// Every registered service URL, placeholders resolved by the daemon. Fetched on connect and
    /// after each catalog reload — URLs only change when the catalog does (or the tailnet host does,
    /// which the daemon re-resolves on each request).
    @Published private(set) var urls: [ResolvedServiceUrl] = []
    /// Hoisted out of the detail view so switching workspaces (which recreates `ServiceListView`)
    /// restores the last focused row and its cached log tail instead of refetching from byte 0.
    @Published var selectedServiceId: String?
    @Published private(set) var actionsInFlight: Set<String> = []
    /// Nested start-then-stop on the same row would otherwise drop the spinner when the start
    /// operation settled while the stop was still in flight.
    private var inFlightCounts: [String: Int] = [:]
    @Published var lastActionError: String?

    private var client: (any ManagerAPI)?
    private var pollTask: Task<Void, Never>?
    private let pollInterval: Duration
    private var configWatcher: ConfigFileWatcher?
    /// How `connect()` obtains a client for a project root. Injectable so the connection state
    /// machine can be driven without a real daemon — the default is the real sidecar path.
    private let connector: @Sendable (String) async throws -> any ManagerAPI
    /// How `restartDaemon()` obtains a client for a project root — same shape as `connector`, but
    /// the sidecar call behind it replaces the daemon instead of finding-or-starting one.
    private let restarter: @Sendable (String) async throws -> any ManagerAPI
    /// How `stopDaemon()` stops the daemon — `hearthd manager stop`, which only returns once the
    /// daemon process has exited. Injectable for the same reason as `connector`/`restarter`.
    private let stopper: @Sendable (String) async throws -> Void
    /// A `stopDaemon` in flight is not a `connecting` phase — but every daemon-lifecycle button
    /// must still stay disabled for its duration, so views read this alongside `phase`.
    @Published private(set) var daemonTransitionInFlight = false
    /// Whether `connect()` should start the config-file watcher. Off under test: it would install a
    /// real dispatch source on a real directory.
    private let watchesConfigFile: Bool
    private var logControllers: [String: LogController] = [:]

    init(
        workspace: Workspace,
        pollInterval: Duration = .seconds(2),
        watchesConfigFile: Bool = true,
        connector: (@Sendable (String) async throws -> any ManagerAPI)? = nil,
        restarter: (@Sendable (String) async throws -> any ManagerAPI)? = nil,
        stopper: (@Sendable (String) async throws -> Void)? = nil
    ) {
        self.workspace = workspace
        self.pollInterval = pollInterval
        self.watchesConfigFile = watchesConfigFile
        self.connector = connector ?? { root in
            ManagerClient(connection: try await DaemonConnection.ensure(root: root))
        }
        self.restarter = restarter ?? { root in
            ManagerClient(connection: try await DaemonConnection.restart(root: root))
        }
        self.stopper = stopper ?? { root in try await DaemonConnection.stopManager(root: root) }
    }

    deinit {
        pollTask?.cancel()
        configWatcher?.stop()
    }

    func connect() async {
        await establish(using: connector)
    }

    /// Replaces this workspace's daemon and reconnects to the new one. The daemon's own services are
    /// left running across the swap (they are detached, and the new daemon re-adopts them from their
    /// persisted identities) — so this is the recovery path for a wedged daemon or one still running
    /// an older `hearthd` binary, not a way to restart a service.
    func restartDaemon() async {
        await establish(using: restarter)
    }

    /// Stops this workspace's daemon AND every service it manages (`hearthd manager stop`), then
    /// drops the whole live state — the connection, watchers, and published services are all dead
    /// afterwards, so nothing may keep polling a daemon that no longer exists.
    func stopDaemon() async {
        guard phase != .connecting, !daemonTransitionInFlight else { return }
        daemonTransitionInFlight = true
        defer { daemonTransitionInFlight = false }
        do {
            try await stopper(workspace.path)
            pollTask?.cancel()
            pollTask = nil
            configWatcher?.stop()
            configWatcher = nil
            dropLogControllers()
            client = nil
            services = []
            urls = []
            catalog = nil
            lastActionError = nil
            phase = .stopped
        } catch {
            // The daemon may still be alive (or never answered) — keep the current connection and
            // surface the reason rather than pretending the stop landed.
            phase = .failed(error.localizedDescription)
        }
    }

    /// The shared body of `connect()`/`restartDaemon()`: obtain a client (find-or-start vs. replace),
    /// then bring the published state in line with it. `phase` doubles as the re-entrancy guard, so a
    /// restart clicked twice cannot stack two daemon swaps.
    private func establish(using obtain: @Sendable (String) async throws -> any ManagerAPI) async {
        guard phase != .connecting, !daemonTransitionInFlight else { return }
        phase = .connecting
        do {
            let client = try await obtain(workspace.path)
            dropLogControllers()
            self.client = client
            catalog = try? await client.catalog() // best-effort — service status doesn't depend on it
            await refreshUrls()
            applyServices(try await client.services())
            phase = .connected
            startWatching()
            if watchesConfigFile {
                startConfigWatcher()
            }
        } catch {
            phase = .failed(error.localizedDescription)
        }
    }

    func stop() {
        pollTask?.cancel()
        pollTask = nil
        configWatcher?.stop()
        configWatcher = nil
        dropLogControllers()
    }

    /// Idempotent: a restart re-establishes the connection, and installing a second dispatch source
    /// on the same directory would leak the first one.
    private func startConfigWatcher() {
        configWatcher?.stop()
        let root = workspace.path
        configWatcher = ConfigFileWatcher(directory: root) { [weak self] in
            Task { @MainActor in await self?.handleConfigChanged(root: root) }
        }
        configWatcher?.start()
    }

    /// A config edit never tears down the live connection on failure (a bad edit — invalid YAML, a
    /// duplicate service id — is exactly when a developer most wants the app to keep showing them
    /// the last-known-good state, with the error surfaced, not a blank/disconnected screen).
    private func handleConfigChanged(root: String) async {
        do {
            try await DaemonConnection.reload(root: root)
            await refresh()
            await refreshUrls()
        } catch {
            lastActionError = "Config reload failed: \(error.localizedDescription)"
        }
    }

    /// Best-effort: a daemon from before `/v1/urls` existed leaves the list empty rather than
    /// failing the connection.
    func refreshUrls() async {
        guard let client else { return }
        if let fresh = try? await client.urls() { urls = fresh }
    }

    /// The registered URLs for one service, in catalog order.
    func urls(for serviceId: String) -> [ResolvedServiceUrl] {
        urls.filter { $0.serviceId == serviceId }
    }

    /// Prefer the daemon's SSE stream (same path as the TUI). If the client cannot stream
    /// (`watchUnsupported` on fakes / older daemons), fall back to polling `GET /v1/services`.
    private func startWatching() {
        pollTask?.cancel()
        pollTask = Task { [weak self] in
            await self?.runWatchLoop()
        }
    }

    private func runWatchLoop() async {
        var after: UInt64?
        var epoch: String?
        while !Task.isCancelled {
            guard let client else { return }
            do {
                for try await event in client.watchEvents(after: after, epoch: epoch) {
                    if Task.isCancelled { return }
                    switch event {
                    case .replay(let nextEpoch, let reset, let latest):
                        epoch = nextEpoch
                        if reset {
                            after = latest
                            await refresh()
                            await refreshUrls()
                        }
                    case .manager(let sequence, let type):
                        after = sequence
                        if type == "service.log" { continue }
                        await refresh()
                        if type == "manager.catalog-reloaded" {
                            catalog = try? await client.catalog()
                            await refreshUrls()
                        }
                    }
                }
                if Task.isCancelled { return }
                try await Task.sleep(for: .seconds(1))
            } catch is CancellationError {
                return
            } catch ManagerClientError.watchUnsupported {
                await pollFallback()
                return
            } catch {
                await refresh()
                try? await Task.sleep(for: pollInterval)
            }
        }
    }

    private func pollFallback() async {
        while !Task.isCancelled {
            try? await Task.sleep(for: pollInterval)
            if Task.isCancelled { return }
            await refresh()
        }
    }

    /// `internal` rather than `private` so a test can step one poll deterministically instead of
    /// waiting on the real timer.
    func refresh() async {
        guard let client else { return }
        do {
            let fresh = try await client.services()
            if case .failed = phase { phase = .connected } // recovered from a transient blip
            applyServices(fresh)
        } catch {
            phase = .failed(error.localizedDescription)
        }
    }

    /// Cached per service so focusing a row again resumes the existing cursor. `nil` before a
    /// successful `connect()`. `LogController.daemonServiceId` selects the daemon's own log —
    /// the pinned "daemon" row, backed by `GET /v1/daemon/log` rather than a service's store.
    func logController(for serviceId: String) -> LogController? {
        guard let client else { return nil }
        if let existing = logControllers[serviceId] { return existing }
        let created = serviceId == LogController.daemonServiceId
            ? LogController(client: client, daemonLog: ())
            : LogController(client: client, serviceId: serviceId)
        logControllers[serviceId] = created
        return created
    }

    private func applyServices(_ fresh: [ServiceLifecycleState]) {
        let unchanged = fresh.count == services.count
            && zip(services, fresh).allSatisfy { $0.isVisuallyEqual(to: $1) }
        if !unchanged { services = fresh }
        let live = Set(fresh.map(\.serviceId))
        if let selected = selectedServiceId, !live.contains(selected), selected != LogController.daemonServiceId {
            selectedServiceId = nil
        }
        for id in logControllers.keys where !live.contains(id) && id != LogController.daemonServiceId {
            logControllers[id]?.stop()
            logControllers.removeValue(forKey: id)
        }
    }

    private func dropLogControllers() {
        for controller in logControllers.values { controller.stop() }
        logControllers.removeAll()
    }

    /// Waits for the operation to actually settle. `POST /v1/operations` returns `202 Accepted`
    /// with a *pending* operation — the work runs asynchronously on the daemon — so returning as
    /// soon as the POST completes cleared the busy state within milliseconds while the service was
    /// still starting, re-enabled the buttons mid-flight, and discarded the failure reason
    /// entirely (it only ever lands on the settled operation's `error`).
    func perform(_ action: ManagerAction, serviceId: String, killUnowned: Bool = false) async {
        guard let client else { return }
        beginFlight(serviceId)
        defer { endFlight(serviceId) }
        do {
            let accepted = try await client.perform(action, serviceId: serviceId, killUnowned: killUnowned)
            // Refresh while it runs so the UI tracks the intermediate states, then again after.
            await refresh()
            _ = try await client.waitForOperation(id: accepted.id)
            await refresh()
        } catch {
            lastActionError = error.localizedDescription
            await refresh()
        }
    }

    /// No bulk-stop endpoint on the daemon (see AGENTS.md's sharp edges) — stops every currently
    /// non-stopped service concurrently, client-side, the same way the TUI's `s` "stop all" key does.
    func stopAll() async {
        guard client != nil else { return }
        let targets = services.filter { $0.actualState != "stopped" }.map(\.serviceId)
        guard !targets.isEmpty else { return }
        await withTaskGroup(of: Void.self) { group in
            for serviceId in targets {
                group.addTask { [weak self] in await self?.perform(.stop, serviceId: serviceId) }
            }
        }
    }

    private func beginFlight(_ serviceId: String) {
        inFlightCounts[serviceId, default: 0] += 1
        actionsInFlight.insert(serviceId)
    }

    private func endFlight(_ serviceId: String) {
        let next = (inFlightCounts[serviceId] ?? 1) - 1
        if next <= 0 {
            inFlightCounts.removeValue(forKey: serviceId)
            actionsInFlight.remove(serviceId)
        } else {
            inFlightCounts[serviceId] = next
        }
    }
}
