// Regression test for a real bug caught while manually verifying the packaged .app: WorkspaceStore
// used JSONDecoder/Encoder's *default* Date strategy (seconds since the 2001 reference date), not
// ISO8601 — a hand-authored or otherwise-produced ISO8601 `addedAt` failed to decode, and
// `WorkspaceStore.load()`'s `try?` silently swallowed the error, leaving `workspaces` empty. Fixed to
// `.iso8601`, matching the ISO8601 timestamps this package's daemon uses everywhere else.

import XCTest
@testable import LocalServicesApp

@MainActor
final class WorkspaceStoreTests: XCTestCase {
    private func tempFileURL() -> URL {
        FileManager.default.temporaryDirectory.appendingPathComponent("workspace-store-test-\(UUID().uuidString).json")
    }

    func testDecodesAnISO8601AddedAtTimestamp() throws {
        let fileURL = tempFileURL()
        defer { try? FileManager.default.removeItem(at: fileURL) }
        let json = """
        [{"id":"11111111-1111-1111-1111-111111111111","path":"/tmp/example","trusted":true,"addedAt":"2026-09-19T00:00:00Z"}]
        """
        try Data(json.utf8).write(to: fileURL)

        let store = WorkspaceStore(fileURL: fileURL)
        XCTAssertEqual(store.workspaces.count, 1, "an ISO8601 addedAt must decode, not be silently dropped")
        XCTAssertEqual(store.workspaces.first?.path, "/tmp/example")
        XCTAssertTrue(store.workspaces.first?.trusted ?? false)
    }

    func testRoundTripsThroughAddAndReload() throws {
        let fileURL = tempFileURL()
        defer { try? FileManager.default.removeItem(at: fileURL) }
        let store = WorkspaceStore(fileURL: fileURL)
        store.add(path: "/tmp/roundtrip-example")
        store.setTrusted(true, id: store.workspaces[0].id)

        let reloaded = WorkspaceStore(fileURL: fileURL)
        XCTAssertEqual(reloaded.workspaces.count, 1)
        XCTAssertEqual(reloaded.workspaces.first?.path, "/tmp/roundtrip-example")
        XCTAssertTrue(reloaded.workspaces.first?.trusted ?? false)
    }

    func testAddingTheSamePathTwiceReturnsTheExistingWorkspace() {
        let store = WorkspaceStore(fileURL: tempFileURL())
        let first = store.add(path: "/tmp/dup")
        let second = store.add(path: "/tmp/dup")
        XCTAssertEqual(first.id, second.id)
        XCTAssertEqual(store.workspaces.count, 1)
    }
}
