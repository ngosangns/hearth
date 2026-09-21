import XCTest

@testable import LocalServicesApp

@MainActor
final class WorkspaceControllerTests: XCTestCase {
    private func workspace(trusted: Bool = true) -> Workspace {
        Workspace(id: UUID(), path: "/tmp/project", trusted: trusted, addedAt: Date())
    }

    private func controller(_ api: FakeManagerAPI) -> WorkspaceController {
        WorkspaceController(workspace: workspace(), watchesConfigFile: false, connector: { _ in api })
    }

    // MARK: - Connection state machine

    func testConnectReachesConnectedAndPublishesServices() async {
        let api = FakeManagerAPI()
        api.catalogHandler = { ServiceCatalogSummary(services: [], groups: [:]) }
        api.servicesHandler = { [makeService("api", actualState: "ready")] }

        let sut = controller(api)
        XCTAssertEqual(sut.phase, .idle)
        await sut.connect()

        XCTAssertEqual(sut.phase, .connected)
        XCTAssertEqual(sut.services.map(\.serviceId), ["api"])
        sut.stop()
    }

    func testConnectFailsWithTheUnderlyingReason() async {
        struct Boom: Error, LocalizedError {
            var errorDescription: String? { "daemon did not start" }
        }
        let api = FakeManagerAPI()
        let sut = WorkspaceController(workspace: workspace(), watchesConfigFile: false, connector: { _ in throw Boom() })

        await sut.connect()

        XCTAssertEqual(sut.phase, .failed("daemon did not start"))
        XCTAssertEqual(api.servicesCalls, 0)
    }

    /// The catalog fetch is deliberately best-effort — service status does not depend on it, so a
    /// catalog failure must not prevent connecting.
    func testConnectSucceedsEvenWhenTheCatalogFetchFails() async {
        let api = FakeManagerAPI()
        api.servicesHandler = { [makeService("api", actualState: "stopped")] }
        // catalogHandler left unset -> throws.

        let sut = controller(api)
        await sut.connect()

        XCTAssertEqual(sut.phase, .connected)
        XCTAssertNil(sut.catalog)
        sut.stop()
    }

    func testRefreshRecoversFromATransientFailure() async {
        let api = FakeManagerAPI()
        api.catalogHandler = { ServiceCatalogSummary(services: [], groups: [:]) }
        api.servicesHandler = { [makeService("api", actualState: "ready")] }
        let sut = controller(api)
        await sut.connect()

        struct Blip: Error, LocalizedError {
            var errorDescription: String? { "manager unavailable" }
        }
        api.servicesHandler = { throw Blip() }
        await sut.refresh()
        XCTAssertEqual(sut.phase, .failed("manager unavailable"))

        api.servicesHandler = { [makeService("api", actualState: "ready")] }
        await sut.refresh()
        XCTAssertEqual(sut.phase, .connected, "a recovered poll must clear the failed phase")
        sut.stop()
    }

    // MARK: - Action bookkeeping

    /// The regression this pins: `perform` used to return as soon as the POST came back `202`,
    /// while the daemon was still doing the work.
    func testPerformStaysBusyUntilTheOperationSettles() async {
        let api = FakeManagerAPI()
        api.catalogHandler = { ServiceCatalogSummary(services: [], groups: [:]) }
        api.servicesHandler = { [makeService("api", actualState: "stopped")] }
        let sut = controller(api)
        await sut.connect()

        api.performHandler = { _, _ in makeOperation(status: "queued") }
        // Two polls report "running" before it settles.
        let reads = Counter()
        api.operationHandler = { _ in
            reads.increment()
            return makeOperation(status: reads.value >= 3 ? "succeeded" : "running")
        }

        await sut.perform(.start, serviceId: "api")

        XCTAssertGreaterThanOrEqual(reads.value, 3, "must poll until the operation reaches a terminal status")
        XCTAssertTrue(sut.actionsInFlight.isEmpty, "busy state must clear once settled")
        XCTAssertNil(sut.lastActionError)
        sut.stop()
    }

    /// The other half: a failure only ever lands on the settled operation, so not reading it back
    /// meant the UI showed no error at all.
    func testPerformSurfacesAFailedOperationsReason() async {
        let api = FakeManagerAPI()
        api.catalogHandler = { ServiceCatalogSummary(services: [], groups: [:]) }
        api.servicesHandler = { [makeService("api", actualState: "stopped")] }
        let sut = controller(api)
        await sut.connect()

        api.performHandler = { _, _ in makeOperation(status: "queued") }
        api.operationHandler = { _ in
            makeOperation(status: "failed", error: OperationError(code: "operation_failed", message: "Dependency startup failed: db"))
        }

        await sut.perform(.start, serviceId: "api")

        XCTAssertEqual(sut.lastActionError, "Dependency startup failed: db")
        XCTAssertTrue(sut.actionsInFlight.isEmpty)
        sut.stop()
    }

    func testStartAllWaitsOnTheBulkOperationAndTargetsTheWholeCatalog() async {
        let api = FakeManagerAPI()
        api.catalogHandler = {
            ServiceCatalogSummary(
                services: [CatalogService(id: "db", label: nil, kind: nil, ownership: nil, dependencies: nil), CatalogService(id: "api", label: nil, kind: nil, ownership: nil, dependencies: nil)],
                groups: [:]
            )
        }
        api.servicesHandler = { [makeService("db", actualState: "stopped"), makeService("api", actualState: "stopped")] }
        let sut = controller(api)
        await sut.connect()

        let targets = Box<[String]>([])
        api.bulkStartHandler = { requested in
            targets.value = requested
            return makeOperation(id: "bulk-1", status: "queued")
        }
        api.operationHandler = { _ in makeOperation(id: "bulk-1", status: "succeeded") }

        await sut.startAll()

        XCTAssertEqual(targets.value, ["db", "api"])
        XCTAssertEqual(api.operationReads, ["bulk-1"], "must read the bulk operation back, not assume 202 means done")
        XCTAssertTrue(sut.actionsInFlight.isEmpty)
        sut.stop()
    }

    func testStopAllSkipsAlreadyStoppedAndQueuedServices() async {
        let api = FakeManagerAPI()
        api.catalogHandler = { ServiceCatalogSummary(services: [], groups: [:]) }
        api.servicesHandler = {
            [
                makeService("ready-one", actualState: "ready"),
                makeService("already-stopped", actualState: "stopped"),
                makeService("queued-one", actualState: "queued-start"),
            ]
        }
        let sut = controller(api)
        await sut.connect()

        api.performHandler = { _, _ in makeOperation(status: "queued") }
        api.operationHandler = { _ in makeOperation(status: "succeeded") }

        await sut.stopAll()

        XCTAssertEqual(api.performed.map(\.serviceId), ["ready-one"])
        sut.stop()
    }
}

/// Small mutable helpers for use inside `@Sendable` fake handlers.
final class Counter: @unchecked Sendable {
    private let queue = DispatchQueue(label: "Counter")
    private var _value = 0
    var value: Int { queue.sync { _value } }
    func increment() { queue.sync { _value += 1 } }
}

final class Box<T>: @unchecked Sendable {
    private let queue = DispatchQueue(label: "Box")
    private var _value: T
    init(_ value: T) { _value = value }
    var value: T {
        get { queue.sync { _value } }
        set { queue.sync { _value = newValue } }
    }
}
