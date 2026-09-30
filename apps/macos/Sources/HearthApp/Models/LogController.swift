import Combine
import Foundation

/// One change to a `LogController`'s buffer, in the order it happened — what `LogTextView` applies
/// to its `NSTextStorage` in place instead of diffing a 256KB string on every poll.
enum LogDelta: Equatable {
    /// Replace the whole buffer (log rotated, stale cursor, or a `daemon.log` tail that moved).
    case reset(String)
    /// Drop `trimmedUTF16` UTF-16 code units from the head, then append `text`.
    case append(String, trimmedUTF16: Int)
}

/// One service's live log tail: polls `GET /v1/logs/:serviceId` on a timer, following the
/// cursor/generation protocol `hearthd logs` uses (see `ManagerClient.logs`'s doc
/// comment). Cached on `WorkspaceController` per service id so focusing a row again resumes the
/// existing cursor instead of refetching the whole buffer.
@MainActor
final class LogController: ObservableObject {
    /// The whole displayed buffer — for Copy and tests. Deliberately not `@Published`: views
    /// follow `deltas`, and republishing the buffer re-rendered the panel on every poll.
    private(set) var text: String = ""
    /// Every buffer change, in order. Subscribers seed themselves from `text` first.
    let deltas = PassthroughSubject<LogDelta, Never>()
    /// Flips only when the buffer goes empty ↔ non-empty (the Copy button's enabled state).
    @Published private(set) var hasText = false
    @Published var lastError: String?
    /// Set once the daemon reports `service_not_found` (`GET /v1/logs/:id`) —
    /// the service was removed from the catalog (e.g. a config-file edit + hot-reload) while this
    /// panel was open. Polling stops for good at that point: the id will never become valid again on
    /// its own, so retrying forever would just hammer the daemon with the same 404. The view is
    /// still expected to clear the selection itself once it disappears from `controller.services` —
    /// this is the backstop for the narrow window before that poll catches up.
    @Published private(set) var isGone = false

    /// Keeps the displayed buffer bounded — this is a live tail view, not a full-log archive.
    /// Over the cap the head is cut down to `trimTargetBytes`, not to the cap itself, so the next
    /// few polls append without trimming again.
    private static let maxDisplayedBytes = 262_144
    private static let trimTargetBytes = 196_608

    private let fetch: (Int?, Int?) async throws -> LogSlice
    private var cursor: Int?
    private var generation: Int?
    private var pollTask: Task<Void, Never>?
    /// Bumped on every real start and stop so a cancelled loop cannot clear a newer task.
    private var pollGeneration = 0
    /// Interval of the live loop. A second `start` at this cadence is a no-op.
    private var activeInterval: Duration?
    /// `start()`'s default cadence.
    let pollInterval: Duration

    init(client: any ManagerAPI, serviceId: String) {
        self.fetch = { cursor, generation in
            try await client.logs(serviceId: serviceId, cursor: cursor, generation: generation)
        }
        self.pollInterval = .milliseconds(700)
    }

    /// The daemon's own log — the same panel, sourced from `GET /v1/daemon/log` instead of
    /// `/v1/logs/:id`. Used for the pinned "daemon" row; its pseudo-id never collides with a real
    /// service because `$` is outside the service-id charset.
    static let daemonServiceId = "$daemon"

    /// Polled less often than a service log: every answer is a fresh 128KB tail, not a cursor delta.
    convenience init(client: any ManagerAPI, daemonLog: Void) {
        self.init(fetch: { _, _ in try await client.daemonLog() }, pollInterval: .seconds(2))
    }

    private init(fetch: @escaping (Int?, Int?) async throws -> LogSlice, pollInterval: Duration) {
        self.fetch = fetch
        self.pollInterval = pollInterval
    }

    func start(interval: Duration? = nil) {
        let interval = interval ?? pollInterval
        // The panel's appear and the selection handoff both call `start`. Cancelling a live loop
        // would drop the cursor's in-flight fetch and paint the buffer twice.
        if pollTask != nil, activeInterval == interval, !isGone { return }
        stop()
        // `@Published` emits even when the value is unchanged. Retargeting calls `start` from
        // `updateNSView`, so a no-op write here publishes in the middle of a view update.
        if isGone { isGone = false }
        activeInterval = interval
        pollGeneration += 1
        let generation = pollGeneration
        // `self` is re-resolved weakly on every pass — though the strong binding then lasts the
        // whole pass (sleep included), so a dropped controller ends one interval later at worst.
        pollTask = Task { [weak self] in
            defer {
                if let self, self.pollGeneration == generation {
                    self.pollTask = nil
                    self.activeInterval = nil
                }
            }
            while !Task.isCancelled {
                guard let self, !self.isGone else { return }
                await self.fetchOnce()
                if self.isGone { return }
                try? await Task.sleep(for: interval)
            }
        }
    }

    func stop() {
        pollGeneration += 1
        pollTask?.cancel()
        pollTask = nil
        activeInterval = nil
    }

    deinit {
        pollTask?.cancel()
    }

    /// Keeps at most `maxBytes` UTF-8 bytes from the end of `value`, cut on a scalar boundary.
    ///
    /// Both sides have to be measured in bytes: the cap test counts `utf8.count` while `suffix(n)`
    /// counts Characters, so with multi-byte output the "trimmed" string could still be over the
    /// cap — the condition stayed true on every poll and re-copied the whole string on the main
    /// actor on every poll without ever converging. A raw byte suffix can land mid-scalar, so the
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

    private static func bounded(_ value: String) -> String {
        value.utf8.count > maxDisplayedBytes ? trimmingToByteCount(value, maxBytes: trimTargetBytes) : value
    }

    /// `internal` rather than `private` so a test can step one poll deterministically instead of
    /// waiting on the real timer.
    func fetchOnce() async {
        do {
            let slice = try await fetch(cursor, generation)
            if slice.reset {
                let fresh = Self.bounded(slice.data)
                if fresh != text {
                    text = fresh
                    deltas.send(.reset(fresh))
                }
            } else if !slice.data.isEmpty {
                text.append(slice.data)
                if text.utf8.count > Self.maxDisplayedBytes {
                    // `kept` is a suffix of `text` cut on a scalar boundary, so the UTF-16 length
                    // difference is exactly the head the text view has to delete — unless the cut
                    // reaches into the slice just appended, which only a full replace can express.
                    let kept = Self.trimmingToByteCount(text, maxBytes: Self.trimTargetBytes)
                    let dropped = text.utf16.count - kept.utf16.count
                    let cutIntoSlice = kept.utf16.count < slice.data.utf16.count
                    text = kept
                    deltas.send(cutIntoSlice ? .reset(kept) : .append(slice.data, trimmedUTF16: dropped))
                } else {
                    deltas.send(.append(slice.data, trimmedUTF16: 0))
                }
            }
            if hasText == text.isEmpty { hasText = !text.isEmpty }
            cursor = slice.nextCursor
            generation = slice.generation
            if lastError != nil { lastError = nil }
        } catch ManagerClientError.http(_, "service_not_found", _) {
            isGone = true
            setLastError("This service is no longer in the catalog.")
        } catch {
            setLastError(error.localizedDescription)
        }
    }

    /// `@Published` notifies on every assign, even when the string is unchanged.
    private func setLastError(_ message: String) {
        guard lastError != message else { return }
        lastError = message
    }
}
