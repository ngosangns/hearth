import Combine
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

    /// The daemon log row is a `LogController` too — `fetchOnce` must hit `GET /v1/daemon/log`
    /// (never `/v1/logs/$daemon`), and every answer is a `reset` tail the buffer replaces.
    func testDaemonLogPollsTheDaemonEndpointAndReplaces() async {
        let api = FakeManagerAPI()
        let slices = Box<[LogSlice]>([
            makeLogSlice(data: "old\n", nextCursor: 0, generation: 0, reset: true),
            makeLogSlice(data: "fresh tail\n", nextCursor: 0, generation: 0, reset: true),
        ])
        api.daemonLogHandler = { slices.value.removeFirst() }
        api.logsHandler = { _, _ in
            XCTFail("the daemon log must not go through the per-service endpoint")
            throw FakeManagerAPI.Unimplemented(what: "logs")
        }

        let sut = LogController(client: api, daemonLog: ())
        await sut.fetchOnce()
        await sut.fetchOnce()

        XCTAssertEqual(api.daemonLogCalls, 2)
        XCTAssertEqual(sut.text, "fresh tail\n", "daemon.log is a full tail — each slice replaces")
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

    // MARK: - Deltas (what `LogTextView` applies in place)

    /// Replays `deltas` onto a mirror the way the text view does — it must always equal `text`.
    private func mirror(_ sut: LogController) -> (Box<NSMutableString>, AnyCancellable) {
        let mirror = Box(NSMutableString(string: sut.text))
        let subscription = sut.deltas.sink { delta in
            switch delta {
            case .reset(let text):
                mirror.value.setString(text)
            case .append(let text, let trimmedUTF16):
                mirror.value.deleteCharacters(in: NSRange(location: 0, length: trimmedUTF16))
                mirror.value.append(text)
            }
        }
        return (mirror, subscription)
    }

    func testAppendsAndResetsArePublishedAsDeltas() async {
        let api = FakeManagerAPI()
        let slices = Box<[LogSlice]>([
            makeLogSlice(data: "one\n", nextCursor: 4, generation: 1),
            makeLogSlice(data: "", nextCursor: 4, generation: 1),
            makeLogSlice(data: "fresh\n", nextCursor: 6, generation: 2, reset: true),
        ])
        api.logsHandler = { _, _ in slices.value.removeFirst() }
        let sut = LogController(client: api, serviceId: "api")
        var deltas: [LogDelta] = []
        let subscription = sut.deltas.sink { deltas.append($0) }
        defer { subscription.cancel() }

        await sut.fetchOnce()
        await sut.fetchOnce()
        await sut.fetchOnce()

        XCTAssertEqual(deltas, [.append("one\n", trimmedUTF16: 0), .reset("fresh\n")], "an empty slice publishes nothing")
        XCTAssertTrue(sut.hasText)
    }

    /// Past the byte cap the head is trimmed — as a head-delete delta, so the text view trims its
    /// storage in place instead of replacing 256KB. Multi-byte text keeps UTF-16 and UTF-8 apart.
    func testTrimmingPublishesAHeadDeleteThatKeepsTheMirrorInSync() async {
        let api = FakeManagerAPI()
        let chunk = String(repeating: "→ log line with multi-byte text\n", count: 1_500) // ~51KB
        api.logsHandler = { _, _ in makeLogSlice(data: chunk, nextCursor: 1, generation: 1) }
        let sut = LogController(client: api, serviceId: "api")
        let (mirror, subscription) = mirror(sut)
        defer { subscription.cancel() }

        for _ in 0..<12 {
            await sut.fetchOnce()
            XCTAssertEqual(mirror.value as String, sut.text)
        }
        XCTAssertLessThanOrEqual(sut.text.utf8.count, 262_144)
    }

    /// A single slice bigger than the cap cannot be expressed as "trim, then append the slice".
    func testASliceLargerThanTheCapIsAReset() async {
        let api = FakeManagerAPI()
        api.logsHandler = { _, _ in makeLogSlice(data: String(repeating: "x", count: 300_000), nextCursor: 1, generation: 1) }
        let sut = LogController(client: api, serviceId: "api")
        let (mirror, subscription) = mirror(sut)
        defer { subscription.cancel() }

        await sut.fetchOnce()

        XCTAssertEqual(mirror.value as String, sut.text)
        XCTAssertLessThanOrEqual(sut.text.utf8.count, 262_144)
    }

    /// Every daemon-log answer is a whole 128KB tail — an unchanged one must not repaint.
    func testAnUnchangedDaemonLogTailPublishesNothing() async {
        let api = FakeManagerAPI()
        api.daemonLogHandler = { makeLogSlice(data: "same tail\n", nextCursor: 0, generation: 0, reset: true) }
        let sut = LogController(client: api, daemonLog: ())
        var deltas: [LogDelta] = []
        let subscription = sut.deltas.sink { deltas.append($0) }
        defer { subscription.cancel() }

        await sut.fetchOnce()
        await sut.fetchOnce()

        XCTAssertEqual(deltas, [.reset("same tail\n")])
        XCTAssertEqual(sut.pollInterval, .seconds(2), "the daemon log polls slower than a service log")
    }
}
