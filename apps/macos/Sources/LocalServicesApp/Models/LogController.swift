import Foundation

/// One service's live log tail: polls `GET /v1/logs/:serviceId` on a timer, following the
/// cursor/generation protocol `localctl.ts`'s own `logs()` uses (see `ManagerClient.logs`'s doc
/// comment). One instance per open log sheet — created by the view, not shared/cached.
@MainActor
final class LogController: ObservableObject {
    @Published private(set) var text: String = ""
    @Published var lastError: String?

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
        pollTask = Task { [weak self] in
            while let self, !Task.isCancelled {
                await self.fetchOnce()
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
        } catch {
            lastError = error.localizedDescription
        }
    }
}
