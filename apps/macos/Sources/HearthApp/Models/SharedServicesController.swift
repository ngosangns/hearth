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
    private var watchTask: Task<Void, Never>?
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

    deinit {
        watchTask?.cancel()
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
        logControllers.removeAll()
        // No `watchTask?.cancel()` here — connect() can run *inside* the watch task, and an
        // early cancel would propagate into `ensure`'s subprocess timeout and fail the whole
        // reconnect. `startWatching()` below cancels the old task once the new client is live.
        do {
            let client = try await connector()
            self.client = client
            await refresh()
            await refreshCatalog()
            phase = .connected
            startWatching()
        } catch {
            phase = .failed(error.localizedDescription)
        }
    }

    func refresh() async {
        guard let client else { return }
        do {
            instances = try await client.sharedInstances()
            if let selected = selectedId, !instances.contains(where: { $0.id == selected }), catalogEntry(selected) == nil {
                selectedId = nil
            }
            if case .failed = phase { phase = .connected }
        } catch {
            phase = .failed(error.localizedDescription)
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

    /// Stop the instance and delete its install + data. The view confirms before calling this.
    func remove(_ id: String) {
        run(id) { client in _ = try await client.sharedRemove(service: id) }
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

    // MARK: - Watching

    /// Same SSE stream every client uses — smp is a normal manager, so `manager.*` events and
    /// service transitions drive `refresh()` exactly like a project daemon's.
    private func startWatching() {
        watchTask?.cancel()
        watchTask = Task { [weak self] in await self?.runWatchLoop() }
    }

    private func runWatchLoop() async {
        var after: UInt64?
        var epoch: String?
        while !Task.isCancelled {
            guard let client else {
                // Dropped after a hard failure — a restarted smp listens on a new port with a
                // new token, so reconnect through `ensure` instead of polling a dead port.
                // Retries every pollInterval until the daemon answers.
                await connect()
                try? await Task.sleep(for: pollInterval)
                continue
            }
            do {
                for try await event in client.watchEvents(after: after, epoch: epoch) {
                    if Task.isCancelled { return }
                    switch event {
                    case .replay(let nextEpoch, let reset, let latest):
                        epoch = nextEpoch
                        if reset {
                            after = latest
                            await refresh()
                        }
                    case .manager(let sequence, let type):
                        after = sequence
                        if type == "service.log" { continue }
                        await refresh()
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
                if case .failed = phase {
                    self.client = nil
                }
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
}
