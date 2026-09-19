import Foundation

/// One workspace's live connection state: ensures a daemon, holds the resulting `ManagerClient`, and
/// polls `/v1/services` on a timer while `.connected` (see `ManagerClient`'s doc comment for why
/// polling rather than SSE in this first pass). One instance per open workspace detail view — created
/// and torn down by SwiftUI's `@StateObject`, not shared/cached across the app.
@MainActor
final class WorkspaceController: ObservableObject {
    enum Phase: Equatable {
        case idle
        case connecting
        case connected
        case failed(String)
    }

    let workspace: Workspace
    @Published private(set) var phase: Phase = .idle
    @Published private(set) var services: [ServiceLifecycleState] = []
    @Published private(set) var catalog: ServiceCatalogSummary?
    @Published private(set) var actionsInFlight: Set<String> = []
    @Published var lastActionError: String?

    private var client: ManagerClient?
    private var pollTask: Task<Void, Never>?
    private let pollInterval: Duration
    private var configWatcher: ConfigFileWatcher?

    init(workspace: Workspace, pollInterval: Duration = .seconds(2)) {
        self.workspace = workspace
        self.pollInterval = pollInterval
    }

    deinit {
        pollTask?.cancel()
        configWatcher?.stop()
    }

    func connect() async {
        guard phase != .connecting else { return }
        phase = .connecting
        do {
            let connection = try await DaemonConnection.ensure(root: workspace.path)
            let client = ManagerClient(connection: connection)
            self.client = client
            catalog = try? await client.catalog() // best-effort — service status doesn't depend on it
            services = try await client.services()
            phase = .connected
            startPolling()
            startConfigWatcher()
        } catch {
            phase = .failed(error.localizedDescription)
        }
    }

    func stop() {
        pollTask?.cancel()
        pollTask = nil
        configWatcher?.stop()
        configWatcher = nil
    }

    private func startConfigWatcher() {
        let root = workspace.path
        configWatcher = ConfigFileWatcher(directory: root) { [weak self] in
            Task { @MainActor in await self?.handleConfigChanged(root: root) }
        }
        configWatcher?.start()
    }

    /// A config edit never tears down the live connection on failure (a bad edit — invalid YAML, a
    /// dependency cycle — is exactly when a developer most wants the app to keep showing them the
    /// last-known-good state, with the error surfaced, not a blank/disconnected screen).
    private func handleConfigChanged(root: String) async {
        do {
            try await DaemonConnection.reload(root: root)
            await refresh()
        } catch {
            lastActionError = "Config reload failed: \(error.localizedDescription)"
        }
    }

    private func startPolling() {
        pollTask?.cancel()
        pollTask = Task { [weak self, pollInterval] in
            while let self, !Task.isCancelled {
                try? await Task.sleep(for: pollInterval)
                if Task.isCancelled { return }
                await self.refresh()
            }
        }
    }

    private func refresh() async {
        guard let client else { return }
        do {
            services = try await client.services()
            if case .failed = phase { phase = .connected } // recovered from a transient blip
        } catch {
            phase = .failed(error.localizedDescription)
        }
    }

    /// `nil` before a successful `connect()` — a log sheet should only ever be offered once
    /// `phase == .connected`, so callers do not need to distinguish "not connected yet" from "will
    /// never have a client" here.
    func makeLogController(serviceId: String) -> LogController? {
        guard let client else { return nil }
        return LogController(client: client, serviceId: serviceId)
    }

    func perform(_ action: ManagerAction, serviceId: String) async {
        guard let client else { return }
        actionsInFlight.insert(serviceId)
        defer { actionsInFlight.remove(serviceId) }
        do {
            _ = try await client.perform(action, serviceId: serviceId)
            await refresh()
        } catch {
            lastActionError = error.localizedDescription
        }
    }

    /// Every catalog service, dependency order handled server-side (`/v1/operations/bulk-start`) —
    /// same "stop on first failure" policy `lsd start <group> --wait` uses.
    func startAll() async {
        guard let client, let catalog, !catalog.services.isEmpty else { return }
        let targets = catalog.services.map(\.id)
        actionsInFlight.formUnion(targets)
        defer { actionsInFlight.subtract(targets) }
        do {
            _ = try await client.bulkStart(targets: targets)
            await refresh()
        } catch {
            lastActionError = error.localizedDescription
        }
    }

    /// No bulk-stop endpoint on the daemon (see AGENTS.md's sharp edges) — stops every currently
    /// non-stopped service concurrently, client-side, the same way the TUI's `s` "stop all" key does.
    func stopAll() async {
        guard client != nil else { return }
        let targets = services.filter { !["stopped", "queued-start"].contains($0.actualState) }.map(\.serviceId)
        guard !targets.isEmpty else { return }
        await withTaskGroup(of: Void.self) { group in
            for serviceId in targets {
                group.addTask { [weak self] in await self?.perform(.stop, serviceId: serviceId) }
            }
        }
    }
}
