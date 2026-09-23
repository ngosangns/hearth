import XCTest

@testable import HearthApp

@MainActor
final class LogControllerPollTests: XCTestCase {
    /// The tailing contract: pass back the previous slice's `nextCursor`/`generation`, starting
    /// with neither.
    func testFirstPollSendsNoCursorAndSubsequentPollsSendTheLastOne() async {
        let api = FakeManagerAPI()
        let slices = Box<[LogSlice]>([
            makeLogSlice(data: "one\n", nextCursor: 10, generation: 1),
            makeLogSlice(data: "two\n", nextCursor: 25, generation: 1),
        ])
        api.logsHandler = { _, _ in slices.value.removeFirst() }

        let sut = LogController(client: api, serviceId: "api")
        await sut.fetchOnce()
        await sut.fetchOnce()

        XCTAssertEqual(api.logRequests.count, 2)
        XCTAssertNil(api.logRequests[0].cursor)
        XCTAssertNil(api.logRequests[0].generation)
        XCTAssertEqual(api.logRequests[1].cursor, 10)
        XCTAssertEqual(api.logRequests[1].generation, 1)
        XCTAssertEqual(sut.text, "one\ntwo\n", "non-reset slices append")
    }

    /// `reset: true` means the log rotated or the cursor was stale — the buffer is replaced, not
    /// appended to, or the view would show the old tail stitched onto the new one.
    func testResetReplacesTheBufferInsteadOfAppending() async {
        let api = FakeManagerAPI()
        let slices = Box<[LogSlice]>([
            makeLogSlice(data: "old\n", nextCursor: 10, generation: 1),
            makeLogSlice(data: "fresh\n", nextCursor: 6, generation: 2, reset: true),
        ])
        api.logsHandler = { _, _ in slices.value.removeFirst() }

        let sut = LogController(client: api, serviceId: "api")
        await sut.fetchOnce()
        await sut.fetchOnce()

        XCTAssertEqual(sut.text, "fresh\n")
        XCTAssertEqual(api.logRequests[1].generation, 1, "the stale generation is what tells the daemon to reset")
    }

    /// A service removed from the catalog will never become valid again, so polling stops for good
    /// rather than hammering the daemon with the same 404.
    func testServiceNotFoundStopsPollingPermanently() async {
        let api = FakeManagerAPI()
        api.logsHandler = { _, _ in
            throw ManagerClientError.http(status: 404, code: "service_not_found", message: "Service not found")
        }

        let sut = LogController(client: api, serviceId: "gone")
        await sut.fetchOnce()

        XCTAssertTrue(sut.isGone)
        XCTAssertEqual(sut.lastError, "This service is no longer in the catalog.")

        // `start()` must not resurrect polling for a service that is gone... it resets `isGone`
        // deliberately (a fresh panel for a re-added service), so assert the loop exits instead.
        sut.start(interval: .milliseconds(1))
        try? await Task.sleep(for: .milliseconds(60))
        sut.stop()
        XCTAssertTrue(sut.isGone, "a 404 must re-latch immediately rather than polling forever")
    }

    func testATransientErrorIsSurfacedButDoesNotLatchGone() async {
        let api = FakeManagerAPI()
        struct Blip: Error, LocalizedError {
            var errorDescription: String? { "manager unavailable" }
        }
        api.logsHandler = { _, _ in throw Blip() }

        let sut = LogController(client: api, serviceId: "api")
        await sut.fetchOnce()

        XCTAssertFalse(sut.isGone)
        XCTAssertEqual(sut.lastError, "manager unavailable")
    }

    func testASuccessfulPollClearsAPreviousError() async {
        let api = FakeManagerAPI()
        struct Blip: Error {}
        let fail = Box<Bool>(true)
        api.logsHandler = { _, _ in
            if fail.value { throw Blip() }
            return makeLogSlice(data: "back\n", nextCursor: 5, generation: 1)
        }

        let sut = LogController(client: api, serviceId: "api")
        await sut.fetchOnce()
        XCTAssertNotNil(sut.lastError)

        fail.value = false
        await sut.fetchOnce()
        XCTAssertNil(sut.lastError)
        XCTAssertEqual(sut.text, "back\n")
    }
}
