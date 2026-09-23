import XCTest

@testable import HearthApp

final class LogControllerTrimTests: XCTestCase {
    func testTrimKeepsShortStringsUntouched() {
        XCTAssertEqual(LogController.trimmingToByteCount("hello", maxBytes: 64), "hello")
    }

    /// The bug this guards: the cap was tested in bytes but applied with `suffix(n)`, which counts
    /// Characters — so a multi-byte string stayed over the cap after "trimming", and every
    /// subsequent poll re-copied the whole buffer without ever converging.
    func testTrimConvergesBelowTheByteCapForMultiByteText() {
        let multiByte = String(repeating: "→", count: 500) // 3 bytes per scalar
        XCTAssertEqual(multiByte.utf8.count, 1500)
        let trimmed = LogController.trimmingToByteCount(multiByte, maxBytes: 100)
        XCTAssertLessThanOrEqual(trimmed.utf8.count, 100)
        // Idempotent: trimming again changes nothing, which is what "converges" means here.
        XCTAssertEqual(LogController.trimmingToByteCount(trimmed, maxBytes: 100), trimmed)
    }

    func testTrimCutsOnAScalarBoundaryRatherThanProducingReplacementCharacters() {
        // 100 is not a multiple of 3, so a raw byte suffix would land mid-scalar.
        let trimmed = LogController.trimmingToByteCount(String(repeating: "→", count: 500), maxBytes: 100)
        XCTAssertFalse(trimmed.contains("\u{FFFD}"), "trim split a scalar instead of cutting on a boundary")
        XCTAssertTrue(trimmed.allSatisfy { $0 == "→" })
    }

    func testTrimKeepsTheTailNotTheHead() {
        let trimmed = LogController.trimmingToByteCount("abcdefghij", maxBytes: 3)
        XCTAssertEqual(trimmed, "hij")
    }
}
