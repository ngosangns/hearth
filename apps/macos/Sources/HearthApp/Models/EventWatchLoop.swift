import Foundation

/// What changed since the owner last synced — the non-log event types seen, and whether the
/// stream asked for a full resync (a `reset` replay, meaning events were missed). An empty batch
/// is a plain status refetch (poll fallback, or checking the daemon after a stream error).
struct EventBatch: Equatable {
    var resync = false
    var types: Set<String> = []
}

/// The one live-status loop both `WorkspaceController` and `SharedServicesController` run: follow
/// `/v1/events/stream` with its `after`/`epoch` cursor, fall back to polling when the client cannot
/// stream, and rediscover the daemon when it stops answering.
///
/// - Events are coalesced: a burst (startup, a group start) collected over `coalesceDelay`, or
///   arriving while a refetch is in flight, becomes one `sync` rather than one
///   `GET /v1/services` each, run back to back.
/// - A stream error followed by a failed `resync` means the daemon is gone or was replaced (a
///   restarted daemon listens on a new port with a new token), so `reconnect` runs `ensure` again
///   instead of polling a dead port forever. Reconnect attempts back off up to `maxReconnectDelay`.
/// - The loop only holds its owner through the handlers, which capture it weakly — so dropping
///   the owner really does end the loop, and `stop()` cancels it outright.
@MainActor
final class EventWatchLoop {
    struct Handlers {
        /// The owner's live client, or `nil` once it dropped a dead one.
        var client: @MainActor () -> (any ManagerAPI)?
        /// Bring the owner's published state in line with the daemon.
        var sync: @MainActor (EventBatch) async -> Void
        /// Whether the last `sync` left the owner failed.
        var isFailed: @MainActor () -> Bool
        /// Drop the dead client and find-or-start the daemon again. On success the owner restarts
        /// this loop (cancelling the running one); on failure it leaves `client()` returning nil.
        var reconnect: @MainActor () async -> Void
    }

    static let maxReconnectDelay: Duration = .seconds(30)
    static let coalesceDelay: Duration = .milliseconds(50)

    private let pollInterval: Duration
    private let handlers: Handlers
    private var task: Task<Void, Never>?

    init(pollInterval: Duration, handlers: Handlers) {
        self.pollInterval = pollInterval
        self.handlers = handlers
    }

    deinit {
        task?.cancel()
    }

    func start() {
        task?.cancel()
        let handlers = handlers
        let pollInterval = pollInterval
        task = Task { await Self.run(handlers, pollInterval: pollInterval) }
    }

    func stop() {
        task?.cancel()
        task = nil
    }

    private static func run(_ handlers: Handlers, pollInterval: Duration) async {
        let coalescer = SyncCoalescer(sync: handlers.sync)
        defer { coalescer.cancel() }
        var after: UInt64?
        var epoch: String?
        var reconnectDelay = pollInterval
        while !Task.isCancelled {
            guard let client = handlers.client() else {
                await handlers.reconnect()
                if Task.isCancelled { return } // a successful reconnect restarted the loop
                try? await Task.sleep(for: reconnectDelay)
                reconnectDelay = min(reconnectDelay * 2, maxReconnectDelay)
                continue
            }
            reconnectDelay = pollInterval
            do {
                for try await event in client.watchEvents(after: after, epoch: epoch) {
                    if Task.isCancelled { return }
                    switch event {
                    case .replay(let nextEpoch, let reset, let latest):
                        epoch = nextEpoch
                        if reset {
                            after = latest
                            coalescer.request(EventBatch(resync: true))
                        }
                    case .manager(let sequence, let type):
                        after = sequence
                        if type == "service.log" { continue }
                        coalescer.request(EventBatch(types: [type]))
                    }
                }
                if Task.isCancelled { return }
                try await Task.sleep(for: .seconds(1))
            } catch is CancellationError {
                return
            } catch ManagerClientError.watchUnsupported {
                await pollFallback(handlers, pollInterval: pollInterval)
                return
            } catch {
                await handlers.sync(EventBatch())
                if Task.isCancelled { return }
                if handlers.isFailed() {
                    await handlers.reconnect()
                    if Task.isCancelled { return }
                }
                try? await Task.sleep(for: pollInterval)
            }
        }
    }

    private static func pollFallback(_ handlers: Handlers, pollInterval: Duration) async {
        while !Task.isCancelled {
            try? await Task.sleep(for: pollInterval)
            if Task.isCancelled { return }
            await handlers.sync(EventBatch())
        }
    }
}

/// Runs at most one `sync` at a time; requests arriving meanwhile merge into a single follow-up.
@MainActor
private final class SyncCoalescer {
    private let sync: @MainActor (EventBatch) async -> Void
    private var pending: EventBatch?
    private var running: Task<Void, Never>?

    init(sync: @escaping @MainActor (EventBatch) async -> Void) {
        self.sync = sync
    }

    func request(_ batch: EventBatch) {
        var merged = pending ?? EventBatch()
        merged.resync = merged.resync || batch.resync
        merged.types.formUnion(batch.types)
        pending = merged
        guard running == nil else { return }
        running = Task { [weak self] in
            try? await Task.sleep(for: EventWatchLoop.coalesceDelay)
            while let batch = self?.takePending() {
                await self?.sync(batch)
            }
        }
    }

    private func takePending() -> EventBatch? {
        guard let batch = pending, !Task.isCancelled else {
            running = nil
            return nil
        }
        pending = nil
        return batch
    }

    func cancel() {
        running?.cancel()
        running = nil
        pending = nil
    }
}
