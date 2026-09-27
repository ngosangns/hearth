// Regression test for a real bug caught while manually verifying the packaged .app: WorkspaceStore
// used JSONDecoder/Encoder's *default* Date strategy (seconds since the 2001 reference date), not
// ISO8601 — a hand-authored or otherwise-produced ISO8601 `addedAt` failed to decode, and
// `WorkspaceStore.load()`'s `try?` silently swallowed the error, leaving `workspaces` empty. Fixed to
// `.iso8601`, matching the ISO8601 timestamps this package's daemon uses everywhere else.

import XCTest
@testable import HearthApp

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

    func testOpenURLSelectsAndAddsThePath() {
        let store = WorkspaceStore(fileURL: tempFileURL())
        store.handleOpenURL(URL(string: "hearth://open?path=/tmp/from-url")!)
        XCTAssertEqual(store.workspaces.map(\.path), ["/tmp/from-url"])
        XCTAssertEqual(store.selectedId, store.workspaces.first?.id)
        XCTAssertFalse(store.workspaces[0].trusted, "URL open must not auto-trust")
    }

    /// A file that does not decode used to become `[]` — and the next save overwrote the user's
    /// list with it. It must be moved aside intact, with the reason surfaced.
    func testACorruptFileIsMovedAsideNotOverwritten() throws {
        let directory = FileManager.default.temporaryDirectory.appendingPathComponent("workspace-store-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: directory) }
        let fileURL = directory.appendingPathComponent("workspaces.json")
        try Data("{ not json".utf8).write(to: fileURL)

        let store = WorkspaceStore(fileURL: fileURL)
        XCTAssertTrue(store.workspaces.isEmpty)
        XCTAssertNotNil(store.loadError)

        store.add(path: "/tmp/after-corruption")
        let aside = try FileManager.default.contentsOfDirectory(atPath: directory.path).filter { $0.hasPrefix("workspaces.json.corrupt-") }
        XCTAssertEqual(aside.count, 1)
        XCTAssertEqual(try String(contentsOf: directory.appendingPathComponent(aside[0]), encoding: .utf8), "{ not json")
    }

    func testSpellingsOfTheSameFolderAreOneWorkspace() {
        let store = WorkspaceStore(fileURL: tempFileURL())
        let first = store.add(path: "/tmp/dup")
        XCTAssertEqual(store.add(path: "/tmp/dup/").id, first.id)
        XCTAssertEqual(store.add(path: "/tmp/./other/../dup").id, first.id)
        XCTAssertEqual(store.workspaces.count, 1)
    }

    func testARelativeDeepLinkPathIsRejected() {
        let store = WorkspaceStore(fileURL: tempFileURL())
        store.handleOpenURL(URL(string: "hearth://open?path=relative/folder")!)
        XCTAssertTrue(store.workspaces.isEmpty)
    }
}
