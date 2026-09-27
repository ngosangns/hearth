import XCTest
@testable import HearthApp

final class ManagerSSETests: XCTestCase {
    func testParsesAReplayFrame() {
        let frame = "event: replay\ndata: {\"epoch\":\"abc\",\"reset\":true,\"latestSequence\":12}"
        XCTAssertEqual(SSEParser.parseFrame(frame), .replay(epoch: "abc", reset: true, latestSequence: 12))
    }

    func testParsesAManagerEventFrame() {
        let frame = "event: service.lifecycle\ndata: {\"sequence\":4,\"at\":\"t\",\"type\":\"service.lifecycle\",\"data\":{}}"
        XCTAssertEqual(SSEParser.parseFrame(frame), .manager(sequence: 4, type: "service.lifecycle"))
    }

    func testACommentOnlyFrameParsesToNothing() {
        XCTAssertNil(SSEParser.parseFrame(": keepalive"))
    }

    private func split(_ raw: String) throws -> [String] {
        var splitter = SSEFrameSplitter()
        var frames: [String] = []
        for byte in raw.utf8 {
            if let frame = try splitter.push(byte) { frames.append(frame) }
        }
        return frames
    }

    /// The blank line is the frame delimiter — the bug this guards read the stream through
    /// `AsyncBytes.lines`, which drops blank lines, so no frame ever completed.
    func testSplitterEndsAFrameOnABlankLineAndHoldsAPartialOne() throws {
        let frames = try split("event: a\ndata: 1\n\nevent: b\ndata: 2\n\nevent: partial")
        XCTAssertEqual(frames, ["event: a\ndata: 1", "event: b\ndata: 2"])
    }

    func testSplitterAcceptsCRLFAndBareCRLineEndings() throws {
        XCTAssertEqual(try split("event: a\r\ndata: 1\r\n\r\n"), ["event: a\ndata: 1"])
        XCTAssertEqual(try split("event: a\rdata: 1\r\r"), ["event: a\ndata: 1"])
    }

    func testSplitterIgnoresRunsOfBlankLines() throws {
        XCTAssertEqual(try split("\n\n\nevent: a\ndata: 1\n\n\n\n"), ["event: a\ndata: 1"])
    }

    func testSplitterRejectsAnOversizedFrame() {
        XCTAssertThrowsError(try split("data: " + String(repeating: "x", count: SSEParser.maxFrameBytes + 1)))
    }

    // MARK: - The real `watchEvents` byte path

    /// Drives `ManagerClient.watchEvents` end to end over a stubbed HTTP response, delivered in
    /// chunks that cut frames (and a `\r\n`) in half — the path the previous `.lines` reader broke.
    func testWatchEventsDeliversEveryFrameFromAChunkedStream() async throws {
        let body = "event: replay\ndata: {\"epoch\":\"e1\",\"reset\":false,\"latestSequence\":3}\n\n"
            + ": keepalive\n\n"
            + "id: 4\r\nevent: service.lifecycle\r\ndata: {\"sequence\":4,\"at\":\"t\",\"type\":\"service.lifecycle\",\"data\":{}}\r\n\r\n"
            + "id: 5\nevent: service.log\ndata: {\"sequence\":5,\"at\":\"t\",\"type\":\"service.log\",\"data\":{}}\n\n"
        let bytes = Array(body.utf8)
        StubSSEProtocol.chunks = stride(from: 0, to: bytes.count, by: 7).map { Data(bytes[$0..<min($0 + 7, bytes.count)]) }
        let configuration = URLSessionConfiguration.ephemeral
        configuration.protocolClasses = [StubSSEProtocol.self]
        let client = ManagerClient(
            connection: ManagerConnection(port: 50101, token: "tok3n", protocolVersion: ManagerClient.supportedProtocolVersion),
            session: URLSession(configuration: configuration)
        )

        var events: [ManagerStreamEvent] = []
        for try await event in client.watchEvents(after: 2, epoch: "e1") {
            events.append(event)
        }

        XCTAssertEqual(events, [
            .replay(epoch: "e1", reset: false, latestSequence: 3),
            .manager(sequence: 4, type: "service.lifecycle"),
            .manager(sequence: 5, type: "service.log"),
        ])
        let request = try XCTUnwrap(StubSSEProtocol.lastRequest)
        XCTAssertEqual(request.url?.path, "/v1/events/stream")
        XCTAssertEqual(request.url?.query, "after=2&epoch=e1")
        XCTAssertEqual(request.value(forHTTPHeaderField: "x-hearth-protocol"), String(ManagerClient.supportedProtocolVersion))
    }
}

/// Answers every request with `200 text/event-stream` and `chunks` as separate body deliveries.
private final class StubSSEProtocol: URLProtocol {
    static var chunks: [Data] = []
    static var lastRequest: URLRequest?

    override class func canInit(with request: URLRequest) -> Bool { true }
    override class func canonicalRequest(for request: URLRequest) -> URLRequest { request }

    override func startLoading() {
        Self.lastRequest = request
        let response = HTTPURLResponse(url: request.url!, statusCode: 200, httpVersion: "HTTP/1.1", headerFields: ["content-type": "text/event-stream"])!
        client?.urlProtocol(self, didReceive: response, cacheStoragePolicy: .notAllowed)
        for chunk in Self.chunks {
            client?.urlProtocol(self, didLoad: chunk)
        }
        client?.urlProtocolDidFinishLoading(self)
    }

    override func stopLoading() {}
}
