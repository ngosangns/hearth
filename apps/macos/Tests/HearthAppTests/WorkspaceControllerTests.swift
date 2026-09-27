import XCTest

@testable import HearthApp

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
        api.catalogHandler = { ServiceCatalogSummary(services: []) }
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
        api.catalogHandler = { ServiceCatalogSummary(services: []) }
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
        api.catalogHandler = { ServiceCatalogSummary(services: []) }
        api.servicesHandler = { [makeService("api", actualState: "stopped")] }
        let sut = controller(api)
        await sut.connect()

        api.performHandler = { _, _, _ in makeOperation(status: "queued") }
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
        api.catalogHandler = { ServiceCatalogSummary(services: []) }
        api.servicesHandler = { [makeService("api", actualState: "stopped")] }
        let sut = controller(api)
        await sut.connect()

        api.performHandler = { _, _, _ in makeOperation(status: "queued") }
        api.operationHandler = { _ in
            makeOperation(status: "failed", error: OperationError(code: "operation_failed", message: "Dependency startup failed: db"))
        }

        await sut.perform(.start, serviceId: "api")

        XCTAssertEqual(sut.lastActionError, "Dependency startup failed: db")
        XCTAssertTrue(sut.actionsInFlight.isEmpty)
        sut.stop()
    }

    // MARK: - Daemon restart

    /// The whole point of the button: after a restart the controller must be talking to the *new*
    /// daemon, not the one it just replaced — without a manual reconnect.
    func testRestartDaemonReconnectsToTheNewClient() async {
        let old = FakeManagerAPI()
        old.catalogHandler = { ServiceCatalogSummary(services: []) }
        old.servicesHandler = { [makeService("api", actualState: "ready")] }
        let new = FakeManagerAPI()
        new.catalogHandler = { ServiceCatalogSummary(services: []) }
        new.servicesHandler = { [makeService("api", actualState: "stopped")] }

        let restarts = Counter()
        let sut = WorkspaceController(
            workspace: workspace(),
            watchesConfigFile: false,
            connector: { _ in old },
            restarter: { _ in
                restarts.increment()
                return new
            }
        )
        await sut.connect()
        XCTAssertEqual(sut.services.map(\.actualState), ["ready"])

        await sut.restartDaemon()

        XCTAssertEqual(restarts.value, 1)
        XCTAssertEqual(sut.phase, .connected)
        XCTAssertEqual(sut.services.map(\.actualState), ["stopped"], "the published state must come from the new daemon")
        sut.stop()
    }

    /// A swap that fails must report why, and must leave the workspace on the daemon it still has
    /// rather than stranding it with no connection at all.
    func testRestartDaemonSurfacesAFailureAndKeepsTheOldConnection() async {
        struct Boom: Error, LocalizedError {
            var errorDescription: String? { "hearthd exited 1: manager unavailable" }
        }
        let api = FakeManagerAPI()
        api.catalogHandler = { ServiceCatalogSummary(services: []) }
        api.servicesHandler = { [makeService("api", actualState: "ready")] }
        let sut = WorkspaceController(workspace: workspace(), watchesConfigFile: false, connector: { _ in api }, restarter: { _ in throw Boom() })
        await sut.connect()

        await sut.restartDaemon()

        XCTAssertEqual(sut.phase, .failed("hearthd exited 1: manager unavailable"))
        await sut.refresh()
        XCTAssertEqual(sut.services.map(\.serviceId), ["api"], "a failed swap must leave the workspace on the daemon it still has")
        sut.stop()
    }

    // MARK: - Daemon stop

    /// A successful stop must land on `.stopped` with the whole live state dropped — nothing may
    /// keep polling a daemon that no longer exists.
    func testStopDaemonDropsAllLiveStateAndReportsStopped() async {
        let api = FakeManagerAPI()
        api.catalogHandler = { ServiceCatalogSummary(services: []) }
        api.servicesHandler = { [makeService("api", actualState: "ready")] }
        let stops = Counter()
        let sut = WorkspaceController(
            workspace: workspace(),
            watchesConfigFile: false,
            connector: { _ in api },
            stopper: { _ in stops.increment() }
        )
        await sut.connect()

        await sut.stopDaemon()

        XCTAssertEqual(stops.value, 1)
        XCTAssertEqual(sut.phase, .stopped)
        XCTAssertTrue(sut.services.isEmpty)
        XCTAssertFalse(sut.phase.mayHaveLiveDaemon)
        sut.stop()
    }

    /// A failed stop must report why and keep the workspace on the daemon that is still running.
    func testStopDaemonSurfacesAFailureAndKeepsTheConnection() async {
        struct Boom: Error, LocalizedError {
            var errorDescription: String? { "hearthd exited 1: shutdown refused" }
        }
        let api = FakeManagerAPI()
        api.catalogHandler = { ServiceCatalogSummary(services: []) }
        api.servicesHandler = { [makeService("api", actualState: "ready")] }
        let sut = WorkspaceController(
            workspace: workspace(),
            watchesConfigFile: false,
            connector: { _ in api },
            stopper: { _ in throw Boom() }
        )
        await sut.connect()

        await sut.stopDaemon()

        XCTAssertEqual(sut.phase, .failed("hearthd exited 1: shutdown refused"))
        await sut.refresh()
        XCTAssertEqual(sut.services.map(\.serviceId), ["api"], "a failed stop must leave the workspace on the daemon it still has")
        sut.stop()
    }

    func testLogControllerIsReusedForTheSameService() async {
        let api = FakeManagerAPI()
        api.catalogHandler = { ServiceCatalogSummary(services: []) }
        api.servicesHandler = { [makeService("api", actualState: "ready")] }
        let sut = controller(api)
        await sut.connect()

        let first = sut.logController(for: "api")
        let second = sut.logController(for: "api")
        XCTAssertNotNil(first)
        XCTAssertTrue(first === second, "re-focusing a service must resume the cached log tail")
        sut.stop()
    }

    /// The pinned daemon row must survive catalog reloads: its selection is not cleared and its
    /// log controller is not pruned like a dead service's.
    func testDaemonLogRowIsNotPrunedByRefresh() async {
        let api = FakeManagerAPI()
        api.catalogHandler = { ServiceCatalogSummary(services: []) }
        api.servicesHandler = { [makeService("api", actualState: "ready")] }
        api.daemonLogHandler = { makeLogSlice(data: "d\n", nextCursor: 0, generation: 0, reset: true) }
        api.logsHandler = { _, _ in throw FakeManagerAPI.Unimplemented(what: "logs") }

        let sut = controller(api)
        await sut.connect()
        sut.selectedServiceId = LogController.daemonServiceId
        let daemonLog = sut.logController(for: LogController.daemonServiceId)
        XCTAssertNotNil(daemonLog)

        await sut.refresh()

        XCTAssertEqual(sut.selectedServiceId, LogController.daemonServiceId,
            "the daemon row is not a service — refresh must not deselect it")
        XCTAssertTrue(sut.logController(for: LogController.daemonServiceId) === daemonLog,
            "the daemon log controller must not be pruned")
        sut.stop()
    }

    func testRefreshKeepsThePublishedSnapshotWhenOnlyTimestampsChange() async {
        let api = FakeManagerAPI()
        api.catalogHandler = { ServiceCatalogSummary(services: []) }
        let stamp = Box(0)
        api.servicesHandler = {
            stamp.value += 1
            return [
                ServiceLifecycleState(
                    serviceId: "api",
                    desiredState: "running",
                    actualState: "ready",
                    readiness: "ready",
                    generation: 1,
                    identity: nil,
                    readinessKind: nil,
                    readinessDetail: nil,
                    createdAt: "2026-01-01T00:00:00.000Z",
                    updatedAt: "2026-01-01T00:00:0\(stamp.value).000Z",
                    exitedAt: nil,
                    exitCode: nil,
                    error: nil,
                    currentOperationId: nil
                )
            ]
        }
        let sut = controller(api)
        await sut.connect()
        let published = sut.services[0].updatedAt

        await sut.refresh()

        XCTAssertEqual(sut.services[0].updatedAt, published, "timestamp-only polls must not republish the service list")
        sut.stop()
    }

    func testStopAllSkipsAlreadyStoppedServicesAndStopsQueuedStarts() async {
        let api = FakeManagerAPI()
        api.catalogHandler = { ServiceCatalogSummary(services: []) }
        api.servicesHandler = {
            [
                makeService("ready-one", actualState: "ready"),
                makeService("already-stopped", actualState: "stopped"),
                makeService("queued-one", actualState: "queued-start"),
            ]
        }
        let sut = controller(api)
        await sut.connect()

        api.performHandler = { _, _, _ in makeOperation(status: "queued") }
        api.operationHandler = { _ in makeOperation(status: "succeeded") }

        await sut.stopAll()

        XCTAssertEqual(Set(api.performed.map(\.serviceId)), ["ready-one", "queued-one"])
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
