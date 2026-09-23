import Foundation

@testable import HearthApp

/// A scriptable stand-in for the daemon. Every method is backed by a closure so a test states only
/// the behaviour it cares about; anything left unset fails loudly rather than returning a plausible
/// default that would silently weaken the assertion.
final class FakeManagerAPI: ManagerAPI, @unchecked Sendable {
    struct Unimplemented: Error, LocalizedError {
        let what: String
        var errorDescription: String? { "FakeManagerAPI.\(what) was called but not stubbed" }
    }

    private let lock = DispatchQueue(label: "FakeManagerAPI")
    private var _performed: [(action: ManagerAction, serviceId: String)] = []
    private var _operationReads: [String] = []
    private var _logRequests: [(cursor: Int?, generation: Int?)] = []
    private var _servicesCalls = 0

    var servicesHandler: (@Sendable () async throws -> [ServiceLifecycleState])?
    var catalogHandler: (@Sendable () async throws -> ServiceCatalogSummary)?
    var logsHandler: (@Sendable (Int?, Int?) async throws -> LogSlice)?
    var performHandler: (@Sendable (ManagerAction, String) async throws -> ManagerOperation)?
    var bulkStartHandler: (@Sendable ([String]) async throws -> ManagerOperation)?
    var operationHandler: (@Sendable (String) async throws -> ManagerOperation)?
    var urlsHandler: (@Sendable () async throws -> [ResolvedServiceUrl])?

    var performed: [(action: ManagerAction, serviceId: String)] {
        lock.sync { _performed }
    }
    var operationReads: [String] {
        lock.sync { _operationReads }
    }
    var logRequests: [(cursor: Int?, generation: Int?)] {
        lock.sync { _logRequests }
    }
    var servicesCalls: Int {
        lock.sync { _servicesCalls }
    }

    func services() async throws -> [ServiceLifecycleState] {
        lock.sync { _servicesCalls += 1 }
        guard let servicesHandler else { throw Unimplemented(what: "services") }
        return try await servicesHandler()
    }

    func catalog() async throws -> ServiceCatalogSummary {
        guard let catalogHandler else { throw Unimplemented(what: "catalog") }
        return try await catalogHandler()
    }

    func managerInfo() async throws -> ManagerInfo {
        throw Unimplemented(what: "managerInfo")
    }

    func logs(serviceId: String, cursor: Int?, generation: Int?, limit: Int) async throws -> LogSlice {
        lock.sync { _logRequests.append((cursor, generation)) }
        guard let logsHandler else { throw Unimplemented(what: "logs") }
        return try await logsHandler(cursor, generation)
    }

    @discardableResult
    func perform(_ action: ManagerAction, serviceId: String) async throws -> ManagerOperation {
        lock.sync { _performed.append((action, serviceId)) }
        guard let performHandler else { throw Unimplemented(what: "perform") }
        return try await performHandler(action, serviceId)
    }

    @discardableResult
    func bulkStart(targets: [String]) async throws -> ManagerOperation {
        guard let bulkStartHandler else { throw Unimplemented(what: "bulkStart") }
        return try await bulkStartHandler(targets)
    }

    func urls() async throws -> [ResolvedServiceUrl] {
        guard let urlsHandler else { throw Unimplemented(what: "urls") }
        return try await urlsHandler()
    }

    func operation(id: String) async throws -> ManagerOperation {
        lock.sync { _operationReads.append(id) }
        guard let operationHandler else { throw Unimplemented(what: "operation") }
        return try await operationHandler(id)
    }
}

// MARK: - Fixtures

func makeOperation(id: String = "op-1", status: String, error: OperationError? = nil) -> ManagerOperation {
    ManagerOperation(
        id: id,
        requestId: "req-1",
        kind: "service",
        serviceId: "api",
        action: "start",
        status: status,
        createdAt: "2026-01-01T00:00:00.000Z",
        updatedAt: "2026-01-01T00:00:00.000Z",
        trace: [],
        error: error
    )
}

func makeService(_ serviceId: String, actualState: String, readiness: String = "ready") -> ServiceLifecycleState {
    ServiceLifecycleState(
        serviceId: serviceId,
        desiredState: actualState == "stopped" ? "stopped" : "running",
        actualState: actualState,
        readiness: readiness,
        generation: 1,
        identity: nil,
        readinessKind: nil,
        readinessDetail: nil,
        createdAt: "2026-01-01T00:00:00.000Z",
        updatedAt: "2026-01-01T00:00:00.000Z",
        exitedAt: nil,
        exitCode: nil,
        error: nil,
        currentOperationId: nil
    )
}

func makeLogSlice(data: String, nextCursor: Int, generation: Int, reset: Bool = false, truncated: Bool = false) -> LogSlice {
    LogSlice(serviceId: "api", generation: generation, cursor: 0, nextCursor: nextCursor, data: data, reset: reset, truncated: truncated)
}
