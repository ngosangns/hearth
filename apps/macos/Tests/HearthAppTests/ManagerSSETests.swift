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

    func testTakeFramesSplitsOnBlankLinesAndLeavesAPartial() {
        var buffer = "event: replay\ndata: {\"epoch\":\"e\",\"reset\":false,\"latestSequence\":1}\n\nevent: partial"
        let frames = SSEParser.takeFrames(from: &buffer)
        XCTAssertEqual(frames.count, 1)
        XCTAssertEqual(buffer, "event: partial")
        XCTAssertEqual(SSEParser.parseFrame(frames[0]), .replay(epoch: "e", reset: false, latestSequence: 1))
    }
}
