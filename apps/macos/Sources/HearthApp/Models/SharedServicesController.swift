import Foundation

/// Live state for the Shared Services window — the app-level connection to the machine-global smp
/// daemon (`~/.hearth/shared`). One instance owned by `HearthApp` (not per workspace): shared
/// instances are singletons across all projects, so the controller mirrors that scope.
///
/// Mirrors `WorkspaceController`'s shape — connect → publish → watch — but simpler: no config file
/// watcher (the "catalog" is `registry.json` + the remote registry, reloaded server-side), and no
/// "stopped" phase (the UI never stops smp itself; instance lifecycle is per `name@version`).
@MainActor
final class SharedServicesController: ObservableObject {
    enum Phase: Equatable {
        case idle
        case connecting
        case connected
        case failed(String)
    }

    @Published private(set) var phase: Phase = .idle
    /// Registered `name@version` instances, registry order (insertion order from `registry.json`).
    @Published private(set) var instances: [SharedInstance] = []
    /// The remote registry document — what *could* be installed. `nil` until the first fetch.
    @Published private(set) var catalogDoc: SharedCatalogDocument?
    /// Why the last registry fetch failed. The UI must show this — a silent nil `catalogDoc`
    /// renders as an empty "Available" section with no hint anything went wrong.
    @Published private(set) var catalogError: String?
    /// Sidebar selection — an instance id (`postgres@16.4`) or a catalog version key
    /// (`postgres@16.4` from the registry; the two spell the same id by design).
    @Published var selectedId: String?
    @Published private(set) var actionsInFlight: Set<String> = []
    @Published var lastActionError: String?

    private var client: (any SharedAPI)?
    /// Same SSE stream every client uses — smp is a normal manager, so `manager.*` events and
    /// service transitions drive `refresh()` exactly like a project daemon's.
    private lazy var watchLoop = EventWatchLoop(pollInterval: pollInterval, handlers: .init(
        client: { [weak self] in self?.client },
        sync: { [weak self] _ in await self?.refresh() },
        isFailed: { [weak self] in
            if case .failed = self?.phase { return true }
            return false
        },
        reconnect: { [weak self] in await self?.connect() }
    ))
    private let pollInterval: Duration
    /// How `connect()` obtains the client — injectable so tests can drive the controller without
    /// spawning `hearthd shared ensure` (the default is the real sidecar path).
    private let connector: @Sendable () async throws -> any SharedAPI
    private var logControllers: [String: LogController] = [:]

    init(
        pollInterval: Duration = .seconds(2),
        connector: (@Sendable () async throws -> any SharedAPI)? = nil
    ) {
        self.pollInterval = pollInterval
        self.connector = connector ?? { ManagerClient(connection: try await DaemonConnection.ensureShared()) }
    }

    /// Find-or-start smp, then publish instances + the registry document and start watching its
    /// event stream. Idempotent while connecting and a no-op once connected — except from
    /// `.failed`, where the daemon may have been replaced (new port + token), so the stale
    /// client is dropped and `ensure` rediscovers the live one.
    func connect() async {
        guard phase != .connecting else { return }
        if client != nil, case .connected = phase { return }
        phase = .connecting
        client = nil
        for controller in logControllers.values { controller.stop() }
        logControllers.removeAll()
        // No `watchLoop.stop()` here — connect() can run *inside* the watch loop, and an early
        // cancel would propagate into `ensure`'s subprocess and fail the whole reconnect.
        // `watchLoop.start()` below cancels the old loop once the new client is live.
        do {
            let client = try await connector()
            // Not `refresh()`: it swallows its error into `.failed`, which the `.connected` below
            // would then paper over — a daemon whose `/v1/shared` fails is not connected.
            instances = try await client.sharedInstances()
            self.client = client
            dropStaleSelection()
            await refreshCatalog()
            phase = .connected
            watchLoop.start()
        } catch {
            phase = .failed(error.localizedDescription)
        }
    }

    func refresh() async {
        guard let client else { return }
        do {
            instances = try await client.sharedInstances()
            dropStaleSelection()
            if case .failed = phase { phase = .connected }
        } catch {
            phase = .failed(error.localizedDescription)
        }
    }

    private func dropStaleSelection() {
        if let selected = selectedId, !instances.contains(where: { $0.id == selected }), catalogEntry(selected) == nil {
            selectedId = nil
        }
    }

    /// Registry fetches are cheap (a cached document) but only re-fetched explicitly — it changes
    /// when upstream publishes, not while the user watches.
    func refreshCatalog() async {
        guard let client else { return }
        do {
            catalogDoc = try await client.sharedCatalog()
            catalogError = nil
        } catch {
            catalogError = error.localizedDescription
        }
    }

    /// A catalog row's selection id spells the same `name@version` as a registered instance, so
    /// lookup is just id equality either way.
    func instance(_ id: String?) -> SharedInstance? {
        instances.first { $0.id == id }
    }

    /// `(name, version)` for a selection that came from the catalog section rather than an
    /// instance row.
    func catalogEntry(_ id: String?) -> (name: String, version: String)? {
        guard let id else { return nil }
        let split = id.split(separator: "@").map(String.init)
        guard split.count == 2, let services = catalogDoc?.services, services[split[0]]?.versions[split[1]] != nil else { return nil }
        return (split[0], split[1])
    }

    // MARK: - Actions

    /// Install (download+verify+extract) a `name@version` — blocking server-side, possibly minutes.
    func install(_ id: String) {
        run(id) { client in _ = try await client.sharedInstall(service: id) }
    }

    /// Stop the instance and delete its install + data. The view confirms before calling this;
    /// `force` is required when the instance still has project attachments.
    func remove(_ id: String, force: Bool = false) {
        run(id) { client in _ = try await client.sharedRemove(service: id, force: force) }
    }

    /// Start/stop the instance's process — the ordinary operations API on `name@version`.
    func perform(_ action: ManagerAction, _ id: String) {
        run(id) { client in
            let accepted = try await client.perform(action, serviceId: id, killUnowned: false)
            _ = try await client.waitForOperation(id: accepted.id)
        }
    }

    private func run(_ id: String, _ work: @escaping @Sendable (any SharedAPI) async throws -> Void) {
        guard let client else { return }
        actionsInFlight.insert(id)
        Task {
            defer { actionsInFlight.remove(id) }
            do {
                try await work(client)
                await refresh()
            } catch {
                lastActionError = error.localizedDescription
                await refresh()
            }
        }
    }

    /// Per-instance log tails — smp writes each shared service's log under its own id, so the
    /// ordinary `LogController` works unchanged.
    func logController(for id: String) -> LogController? {
        guard let client else { return nil }
        if let existing = logControllers[id] { return existing }
        let created = LogController(client: client, serviceId: id)
        logControllers[id] = created
        return created
    }
}
