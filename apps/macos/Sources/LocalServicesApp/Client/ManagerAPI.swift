import Foundation

/// The daemon operations the app's controllers depend on.
///
/// `ManagerClient` is the real implementation; this protocol exists so `WorkspaceController` and
/// `LogController` — which own the riskiest logic in the app (connection state machine, in-flight
/// action bookkeeping, the log cursor/generation/reset protocol) — can be driven by a fake instead
/// of a live daemon. Without it none of that was reachable from a test.
protocol ManagerAPI: Sendable {
    func services() async throws -> [ServiceLifecycleState]
    func catalog() async throws -> ServiceCatalogSummary
    func managerInfo() async throws -> ManagerInfo
    func logs(serviceId: String, cursor: Int?, generation: Int?, limit: Int) async throws -> LogSlice
    @discardableResult func perform(_ action: ManagerAction, serviceId: String) async throws -> ManagerOperation
    @discardableResult func bulkStart(targets: [String]) async throws -> ManagerOperation
    func operation(id: String) async throws -> ManagerOperation
    /// Every registered service URL, placeholders resolved (`GET /v1/urls`).
    func urls() async throws -> [ResolvedServiceUrl]
    /// Live event stream (`GET /v1/events/stream`). Default throws `watchUnsupported` so fakes and
    /// older daemons fall back to polling.
    func watchEvents(after: UInt64?, epoch: String?) -> AsyncThrowingStream<ManagerStreamEvent, Error>
}

extension ManagerAPI {
    /// Polls an accepted operation until it reaches a terminal status, mirroring the CLI's
    /// `waitOperation`.
    ///
    /// `POST /v1/operations` answers `202 Accepted` with a *pending* operation and runs the work
    /// asynchronously, so the outcome only ever exists on the operation read back — treating the
    /// POST's return as completion both cleared the UI's busy state early and discarded the failure
    /// reason. Lives on the protocol rather than on `ManagerClient` so it is exercised by the same
    /// tests that drive a fake, not only against a live daemon.
    func waitForOperation(id: String, pollInterval: Duration = .milliseconds(250), timeout: Duration = .seconds(180)) async throws -> ManagerOperation {
        let deadline = ContinuousClock.now.advanced(by: timeout)
        while true {
            try Task.checkCancellation()
            let current = try await operation(id: id)
            switch current.status {
            case "succeeded":
                return current
            case "failed":
                throw ManagerClientError.operationFailed(current.error?.message ?? "ManagerOperation failed")
            default:
                break
            }
            if ContinuousClock.now >= deadline {
                throw ManagerClientError.operationFailed("ManagerOperation did not finish within \(timeout)")
            }
            try await Task.sleep(for: pollInterval)
        }
    }

    /// The daemon's default log page size; kept here so callers and the protocol agree on it.
    func logs(serviceId: String, cursor: Int?, generation: Int?) async throws -> LogSlice {
        try await logs(serviceId: serviceId, cursor: cursor, generation: generation, limit: 16_384)
    }

    func watchEvents(after: UInt64?, epoch: String?) -> AsyncThrowingStream<ManagerStreamEvent, Error> {
        AsyncThrowingStream { $0.finish(throwing: ManagerClientError.watchUnsupported) }
    }
}
