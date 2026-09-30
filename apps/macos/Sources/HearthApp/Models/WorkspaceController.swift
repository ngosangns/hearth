import Foundation

/// One workspace's live connection state: ensures a daemon, holds the resulting `ManagerClient`, and
/// watches `/v1/events/stream` through `EventWatchLoop` (falling back to polling
/// `GET /v1/services`). Owned by
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
    @Published private(set) var services: [ServiceLifecycleState] = [] {
        didSet {
            servicesById = Dictionary(services.map { ($0.serviceId, $0) }, uniquingKeysWith: { first, _ in first })
            recomputeCounts()
        }
    }
    @Published private(set) var catalog: ServiceCatalogSummary? {
        didSet {
            catalogServices = Dictionary(catalog?.services.map { ($0.id, $0) } ?? [], uniquingKeysWith: { first, _ in first })
            recomputeCounts()
        }
    }
    /// Every registered service URL, placeholders resolved by the daemon. Fetched on connect and
    /// after each catalog reload — URLs only change when the catalog does (or the tailnet host does,
    /// which the daemon re-resolves on each request).
    @Published private(set) var urls: [ResolvedServiceUrl] = [] {
        didSet { urlsByService = Dictionary(grouping: urls, by: \.serviceId) }
    }
    /// Lookups the service list renders per row, rebuilt when their source changes rather than
    /// rescanned for every row on every render.
    private var catalogServices: [String: CatalogService] = [:]
    private var urlsByService: [String: [ResolvedServiceUrl]] = [:]
    private var servicesById: [String: ServiceLifecycleState] = [:]
    /// Rebuilt when `services` or the catalog changes, so a sidebar tick does not rescan every service.
    private var cachedCounts = ServiceCounts()
    /// Hoisted out of the detail view. The detail stays mounted across workspace switches, and this
    /// restores the last focused row and its cached log tail instead of refetching from byte 0.
    ///
    /// Not `@Published` here. A row click must not `objectWillChange` this controller — the service
    /// list and the sidebar row observe it, and the snapshots have not changed. The log pane
    /// observes `selection` instead. The id is stored immediately; the notification waits a turn
    /// because `List` writes the binding from inside a view update.
    var selectedServiceId: String? {
        get { selection.serviceId }
        set { selection.setServiceId(newValue) }
    }
    let selection = ServiceSelection()
    @Published private(set) var actionsInFlight: Set<String> = []
    /// Nested start-then-stop on the same row would otherwise drop the spinner when the start
    /// operation settled while the stop was still in flight.
    private var inFlightCounts: [String: Int] = [:]
    @Published var lastActionError: String?

    private var client: (any ManagerAPI)?
    private lazy var watchLoop = EventWatchLoop(pollInterval: pollInterval, handlers: .init(
        client: { [weak self] in self?.client },
        sync: { [weak self] batch in await self?.sync(batch) },
        isFailed: { [weak self] in
            if case .failed = self?.phase { return true }
            return false
        },
        reconnect: { [weak self] in await self?.reconnect() }
    ))
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
        // Stop the watch loop FIRST: while the old daemon exits its stream errors, and an
        // in-flight reconnect would otherwise race `establish` into a second, competing `ensure`.
        watchLoop.stop()
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
            watchLoop.stop()
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
            failPhase(error.localizedDescription)
        }
    }

    /// The shared body of `connect()`/`restartDaemon()`: obtain a client (find-or-start vs. replace),
    /// then bring the published state in line with it. `phase` doubles as the re-entrancy guard, so a
    /// restart clicked twice cannot stack two daemon swaps.
    private func establish(using obtain: @Sendable (String) async throws -> any ManagerAPI) async {
        guard phase != .connecting, !daemonTransitionInFlight else { return }
        daemonTransitionInFlight = true
        defer { daemonTransitionInFlight = false }
        phase = .connecting
        do {
            let client = try await obtain(workspace.path)
            dropLogControllers()
            self.client = client
            let freshCatalog = try? await client.catalog() // best-effort — service status doesn't depend on it
            if freshCatalog != catalog { catalog = freshCatalog }
            await refreshUrls()
            applyServices(try await client.services())
            phase = .connected
            watchLoop.start()
            if watchesConfigFile {
                startConfigWatcher()
            }
        } catch {
            failPhase(error.localizedDescription)
        }
    }

    /// The watch loop's recovery path: a daemon that stopped answering was usually replaced
    /// (CLI `manager restart`, a crash then `ensure`) and now listens on a new port with a new
    /// token, so the dead client is dropped and `ensure` finds — or starts — the live one.
    private func reconnect() async {
        guard phase != .connecting, !daemonTransitionInFlight else { return }
        client = nil
        await establish(using: connector)
    }

    func stop() {
        watchLoop.stop()
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
        if let fresh = try? await client.urls(), fresh != urls { urls = fresh }
    }

    /// The registered URLs for one service, in catalog order.
    func urls(for serviceId: String) -> [ResolvedServiceUrl] {
        urlsByService[serviceId] ?? []
    }

    func service(_ serviceId: String) -> ServiceLifecycleState? {
        servicesById[serviceId]
    }

    /// The catalog's label for a service, else its id.
    func displayName(for serviceId: String) -> String {
        catalogServices[serviceId]?.displayName ?? serviceId
    }

    var counts: ServiceCounts { cachedCounts }

    private func recomputeCounts() {
        var next = ServiceCounts()
        for service in services {
            next.add(service, finite: isFinite(service.serviceId))
        }
        cachedCounts = next
    }

    /// Skips the assign when the message is already showing. `@Published` notifies on every set.
    private func failPhase(_ message: String) {
        if case .failed(let existing) = phase, existing == message { return }
        phase = .failed(message)
    }

    /// What `EventWatchLoop` calls per (coalesced) batch of stream events. The catalog and URLs
    /// only move on a catalog reload — or on a resync, which may have skipped one.
    private func sync(_ batch: EventBatch) async {
        await refresh()
        guard batch.resync || batch.types.contains("manager.catalog-reloaded"), let client else { return }
        if let fresh = try? await client.catalog(), fresh != catalog { catalog = fresh }
        await refreshUrls()
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
            // `.connecting` belongs to `establish` — a late refresh failure must not overwrite it,
            // or the reconnect guard would fire a second `ensure` mid-transition.
            if phase != .connecting { failPhase(error.localizedDescription) }
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

    /// The cached tail, without creating one. Selection handoff uses this to stop the row being left.
    func cachedLogController(for serviceId: String) -> LogController? {
        logControllers[serviceId]
    }

    /// Stops every live tail poll. Cursors and buffers stay cached so focusing the row again resumes.
    func stopLogTails() {
        for controller in logControllers.values { controller.stop() }
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

    /// Whether the catalog marks this service `disabled: true` — disabled services take no
    /// lifecycle action (start/stop/restart are rejected daemon-side too).
    func isDisabled(_ serviceId: String) -> Bool {
        catalogServices[serviceId]?.isDisabled ?? false
    }

    /// `readiness: { kind: exit }` in the catalog. Lifecycle state has no `readinessKind` until
    /// the first start, so the Run button and the ready/total skip have to come from here.
    func isFinite(_ serviceId: String) -> Bool {
        catalogServices[serviceId]?.isFinite ?? false
    }

    /// No bulk-stop endpoint on the daemon (see `ManagerClient.bulkStart`) — stops every currently
    /// non-stopped service concurrently, client-side, the same way the TUI's `s` "stop all" key does.
    func stopAll() async {
        guard client != nil else { return }
        let targets = services.filter { $0.actualState != "stopped" && !isDisabled($0.serviceId) }.map(\.serviceId)
        guard !targets.isEmpty else { return }
        await stopGroup(targets)
    }

    /// Group section actions. Start goes through `bulk-start` so the daemon's stop-on-first-failure
    /// ordering applies; stop mirrors `stopAll` — a client-side loop over the group's members.
    func startGroup(_ serviceIds: [String]) async {
        guard let client else { return }
        for id in serviceIds { beginFlight(id) }
        defer { for id in serviceIds { endFlight(id) } }
        do {
            let accepted = try await client.bulkStart(targets: serviceIds)
            await refresh()
            _ = try await client.waitForOperation(id: accepted.id)
            await refresh()
        } catch {
            lastActionError = error.localizedDescription
            await refresh()
        }
    }

    func stopGroup(_ serviceIds: [String]) async {
        guard client != nil else { return }
        await withTaskGroup(of: Void.self) { group in
            for serviceId in serviceIds {
                group.addTask { [weak self] in await self?.perform(.stop, serviceId: serviceId) }
            }
        }
    }

    /// No bulk-restart endpoint either — same client-side loop as `stopGroup`.
    func restartGroup(_ serviceIds: [String]) async {
        guard client != nil else { return }
        await withTaskGroup(of: Void.self) { group in
            for serviceId in serviceIds {
                group.addTask { [weak self] in await self?.perform(.restart, serviceId: serviceId) }
            }
        }
    }

    private func beginFlight(_ serviceId: String) {
        inFlightCounts[serviceId, default: 0] += 1
        // A nested start-then-stop is already showing the spinner. Inserting an id the set
        // already has still publishes, and every row rebuilds for a busy flag that did not change.
        guard !actionsInFlight.contains(serviceId) else { return }
        actionsInFlight.insert(serviceId)
    }

    private func endFlight(_ serviceId: String) {
        let next = (inFlightCounts[serviceId] ?? 1) - 1
        if next <= 0 {
            inFlightCounts.removeValue(forKey: serviceId)
            if actionsInFlight.contains(serviceId) { actionsInFlight.remove(serviceId) }
        } else {
            inFlightCounts[serviceId] = next
        }
    }
}

/// Focused service row. Its own object so a click does not publish `WorkspaceController`.
///
/// The value is stored immediately, but `objectWillChange` waits until the next turn.
/// `List(selection:)` writes the binding from inside a view update, and publishing there makes
/// SwiftUI re-enter the update.
@MainActor
final class ServiceSelection: ObservableObject {
    private(set) var serviceId: String?
    private var notifyScheduled = false

    func setServiceId(_ newValue: String?) {
        guard serviceId != newValue else { return }
        serviceId = newValue
        guard !notifyScheduled else { return }
        notifyScheduled = true
        Task { @MainActor [weak self] in
            guard let self else { return }
            self.notifyScheduled = false
            self.objectWillChange.send()
        }
    }
}
