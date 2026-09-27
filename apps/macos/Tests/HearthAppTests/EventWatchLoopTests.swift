import XCTest
@testable import HearthApp

/// Drives the shared `EventWatchLoop` through the controllers that own it, with a fake event
/// stream — before this every controller test ran the poll fallback, never the event branch.
@MainActor
final class EventWatchLoopTests: XCTestCase {
    private func workspace() -> Workspace {
        Workspace(id: UUID(), path: "/tmp/project", trusted: true, addedAt: Date())
    }

    /// A stream the test pushes into — the continuation is only reachable through the box.
    private func makeStream() -> (AsyncThrowingStream<ManagerStreamEvent, Error>, Box<AsyncThrowingStream<ManagerStreamEvent, Error>.Continuation?>) {
        let box = Box<AsyncThrowingStream<ManagerStreamEvent, Error>.Continuation?>(nil)
        let stream = AsyncThrowingStream<ManagerStreamEvent, Error> { box.value = $0 }
        return (stream, box)
    }

    private func waitUntil(_ what: String, timeout: Duration = .seconds(5), _ condition: () -> Bool) async {
        let deadline = ContinuousClock.now.advanced(by: timeout)
        while !condition() {
            if ContinuousClock.now >= deadline { return XCTFail("timed out waiting for \(what)") }
            try? await Task.sleep(for: .milliseconds(10))
        }
    }

    /// A pushed lifecycle event must reach the published state on its own — the poll interval is
    /// an hour, so only the event branch can explain the update.
    func testALifecycleEventRefreshesServices() async {
        let api = FakeManagerAPI()
        api.catalogHandler = { ServiceCatalogSummary(services: []) }
        let state = Box("stopped")
        api.servicesHandler = { [makeService("api", actualState: state.value)] }
        let (stream, continuation) = makeStream()
        api.watchEventsHandler = { _, _ in stream }

        let sut = WorkspaceController(workspace: workspace(), pollInterval: .seconds(3600), watchesConfigFile: false, connector: { _ in api })
        await sut.connect()
        await waitUntil("the loop to subscribe") { continuation.value != nil }

        state.value = "ready"
        continuation.value?.yield(.replay(epoch: "e1", reset: false, latestSequence: 0))
        continuation.value?.yield(.manager(sequence: 1, type: "service.lifecycle"))

        await waitUntil("the event to refresh services") { sut.services.first?.actualState == "ready" }
        sut.stop()
    }

    /// `service.log` events are high-volume and carry no status — they must not refetch.
    func testLogEventsDoNotRefetch() async throws {
        let api = FakeManagerAPI()
        api.catalogHandler = { ServiceCatalogSummary(services: []) }
        api.servicesHandler = { [makeService("api", actualState: "ready")] }
        let (stream, continuation) = makeStream()
        api.watchEventsHandler = { _, _ in stream }

        let sut = WorkspaceController(workspace: workspace(), pollInterval: .seconds(3600), watchesConfigFile: false, connector: { _ in api })
        await sut.connect()
        await waitUntil("the loop to subscribe") { continuation.value != nil }
        let before = api.servicesCalls

        for sequence in 1...20 {
            continuation.value?.yield(.manager(sequence: UInt64(sequence), type: "service.log"))
        }
        try await Task.sleep(for: .milliseconds(200))

        XCTAssertEqual(api.servicesCalls, before)
        sut.stop()
    }

    /// A burst of events coalesces into a refetch or two, not one `GET /v1/services` each.
    func testABurstOfEventsIsCoalesced() async throws {
        let api = FakeManagerAPI()
        api.catalogHandler = { ServiceCatalogSummary(services: []) }
        api.servicesHandler = { [makeService("api", actualState: "ready")] }
        let (stream, continuation) = makeStream()
        api.watchEventsHandler = { _, _ in stream }

        let sut = WorkspaceController(workspace: workspace(), pollInterval: .seconds(3600), watchesConfigFile: false, connector: { _ in api })
        await sut.connect()
        await waitUntil("the loop to subscribe") { continuation.value != nil }
        let before = api.servicesCalls

        for sequence in 1...20 {
            continuation.value?.yield(.manager(sequence: UInt64(sequence), type: "service.lifecycle"))
        }
        await waitUntil("a refetch") { api.servicesCalls > before }
        try await Task.sleep(for: .milliseconds(200))

        XCTAssertLessThanOrEqual(api.servicesCalls - before, 3, "20 events must not mean 20 refetches")
        sut.stop()
    }

    /// The reconnect cursor: after a stream ends, the next subscription resumes from the last
    /// sequence and epoch seen.
    func testTheNextSubscriptionResumesFromTheLastSequence() async {
        let api = FakeManagerAPI()
        api.catalogHandler = { ServiceCatalogSummary(services: []) }
        api.servicesHandler = { [makeService("api", actualState: "ready")] }
        let first = Box(true)
        api.watchEventsHandler = { _, _ in
            AsyncThrowingStream { continuation in
                if first.value {
                    first.value = false
                    continuation.yield(.replay(epoch: "e1", reset: false, latestSequence: 6))
                    continuation.yield(.manager(sequence: 7, type: "service.lifecycle"))
                    continuation.finish()
                }
            }
        }

        let sut = WorkspaceController(workspace: workspace(), pollInterval: .seconds(3600), watchesConfigFile: false, connector: { _ in api })
        await sut.connect()
        await waitUntil("a second subscription") { api.watchRequests.count >= 2 }

        XCTAssertNil(api.watchRequests[0].after)
        XCTAssertEqual(api.watchRequests[1].after, 7)
        XCTAssertEqual(api.watchRequests[1].epoch, "e1")
        sut.stop()
    }

    /// The daemon was replaced behind the app's back (CLI `manager restart`): the stream drops,
    /// the old client stops answering, and the controller must `ensure` its way to the new one
    /// instead of polling the dead port until someone clicks Retry.
    func testAReplacedDaemonIsRediscovered() async {
        let old = FakeManagerAPI()
        old.catalogHandler = { ServiceCatalogSummary(services: []) }
        old.servicesHandler = { [makeService("api", actualState: "ready")] }
        old.watchEventsHandler = { _, _ in AsyncThrowingStream { $0.finish(throwing: URLError(.networkConnectionLost)) } }
        let new = FakeManagerAPI()
        new.catalogHandler = { ServiceCatalogSummary(services: []) }
        new.servicesHandler = { [makeService("api", actualState: "stopped")] }

        let connects = Counter()
        let sut = WorkspaceController(workspace: workspace(), pollInterval: .milliseconds(20), watchesConfigFile: false, connector: { _ in
            connects.increment()
            return connects.value == 1 ? old : new
        })
        await sut.connect()
        // Runs before the loop's first pass — the test holds the main actor until it suspends.
        old.servicesHandler = { throw ManagerClientError.transport(URLError(.cannotConnectToHost)) }

        await waitUntil("the reconnect") { connects.value >= 2 && sut.phase == .connected }
        XCTAssertEqual(sut.services.map(\.actualState), ["stopped"], "state must come from the rediscovered daemon")
        sut.stop()
    }

    func testTheSharedControllerFollowsItsStreamToo() async {
        let api = FakeManagerAPI()
        let state = Box("stopped")
        api.sharedInstancesHandler = { [makeSharedInstance("redis@7.2", state: state.value)] }
        api.sharedCatalogHandler = { SharedCatalogDocument(version: 1, services: [:]) }
        let (stream, continuation) = makeStream()
        api.watchEventsHandler = { _, _ in stream }

        let sut = SharedServicesController(pollInterval: .seconds(3600), connector: { api })
        await sut.connect()
        await waitUntil("the loop to subscribe") { continuation.value != nil }

        state.value = "ready"
        continuation.value?.yield(.manager(sequence: 1, type: "service.lifecycle"))

        await waitUntil("the event to refresh instances") { sut.instances.first?.displayState == "ready" }
    }
}
