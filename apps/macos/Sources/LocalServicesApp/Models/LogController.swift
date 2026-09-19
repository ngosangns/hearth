import Foundation

/// One service's live log tail: polls `GET /v1/logs/:serviceId` on a timer, following the
/// cursor/generation protocol `localctl.ts`'s own `logs()` uses (see `ManagerClient.logs`'s doc
/// comment). One instance per selected service (see `ServiceLogPanel`) — created fresh whenever the
/// selection changes, not shared/cached.
@MainActor
final class LogController: ObservableObject {
    @Published private(set) var text: String = ""
    @Published var lastError: String?
    /// Set once the daemon reports `service_not_found` (`src/core/manager.ts`'s `/v1/logs/:id`) —
    /// the service was removed from the catalog (e.g. a config-file edit + hot-reload) while this
    /// panel was open. Polling stops for good at that point: the id will never become valid again on
    /// its own, so retrying forever would just hammer the daemon with the same 404. The view is
    /// still expected to clear the selection itself once it disappears from `controller.services` —
    /// this is the backstop for the narrow window before that poll catches up.
    @Published private(set) var isGone = false

    /// Keeps the displayed buffer bounded — this is a live tail view, not a full-log archive.
    private static let maxDisplayedBytes = 262_144

    private let client: ManagerClient
    private let serviceId: String
    private var cursor: Int?
    private var generation: Int?
    private var pollTask: Task<Void, Never>?

    init(client: ManagerClient, serviceId: String) {
        self.client = client
        self.serviceId = serviceId
    }

    func start(interval: Duration = .milliseconds(700)) {
        stop()
        isGone = false
        pollTask = Task { [weak self] in
            while let self, !Task.isCancelled, !self.isGone {
                await self.fetchOnce()
                if self.isGone { return }
                try? await Task.sleep(for: interval)
            }
        }
    }

    func stop() {
        pollTask?.cancel()
        pollTask = nil
    }

    deinit {
        pollTask?.cancel()
    }

    private func fetchOnce() async {
        do {
            let slice = try await client.logs(serviceId: serviceId, cursor: cursor, generation: generation)
            text = slice.reset ? slice.data : text + slice.data
            if text.utf8.count > Self.maxDisplayedBytes {
                text = String(text.suffix(Self.maxDisplayedBytes))
            }
            cursor = slice.nextCursor
            generation = slice.generation
            lastError = nil
        } catch ManagerClientError.http(_, "service_not_found", _) {
            isGone = true
            lastError = "This service is no longer in the catalog."
        } catch {
            lastError = error.localizedDescription
        }
    }
}
