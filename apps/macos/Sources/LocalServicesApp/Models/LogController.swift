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

    /// Keeps at most `maxBytes` UTF-8 bytes from the end of `value`, cut on a scalar boundary.
    ///
    /// Both sides have to be measured in bytes: the cap test counts `utf8.count` while `suffix(n)`
    /// counts Characters, so with multi-byte output the "trimmed" string could still be over the
    /// cap — the condition stayed true on every poll and re-copied the whole string on the main
    /// actor every 700ms without ever converging. A raw byte suffix can land mid-scalar, so the
    /// leading continuation bytes (`0b10xxxxxx`) are dropped rather than decoded into U+FFFD.
    /// `nonisolated` because it is pure — no reason to hop to the main actor to trim a string.
    nonisolated static func trimmingToByteCount(_ value: String, maxBytes: Int) -> String {
        let bytes = Array(value.utf8)
        guard bytes.count > maxBytes else { return value }
        var start = bytes.count - maxBytes
        while start < bytes.count && (bytes[start] & 0b1100_0000) == 0b1000_0000 {
            start += 1
        }
        return String(decoding: bytes[start...], as: UTF8.self)
    }

    private func fetchOnce() async {
        do {
            let slice = try await client.logs(serviceId: serviceId, cursor: cursor, generation: generation)
            text = slice.reset ? slice.data : text + slice.data
            // Trim by BYTES on both sides of the comparison. `suffix(n)` counts Characters, so with
            // any multi-byte log output the trimmed string could still exceed the byte cap — the
            // condition then stayed true on every poll and re-copied a 256KB+ string on the main
            // actor every 700ms, forever, without ever converging.
            if text.utf8.count > Self.maxDisplayedBytes {
                text = Self.trimmingToByteCount(text, maxBytes: Self.maxDisplayedBytes)
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
