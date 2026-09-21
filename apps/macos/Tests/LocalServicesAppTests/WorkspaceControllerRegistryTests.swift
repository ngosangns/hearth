import Combine
import XCTest

@testable import LocalServicesApp

@MainActor
final class WorkspaceControllerRegistryTests: XCTestCase {
    /// Untrusted on purpose: `sync` never auto-connects one, so these exercise creation, pruning
    /// and change forwarding without reaching for a real daemon.
    private func untrusted(_ path: String) -> Workspace {
        Workspace(id: UUID(), path: path, trusted: false, addedAt: Date())
    }

    func testSyncCreatesOneControllerPerWorkspace() {
        let registry = WorkspaceControllerRegistry()
        let a = untrusted("/tmp/a")
        let b = untrusted("/tmp/b")

        registry.sync([a, b])

        XCTAssertEqual(Set(registry.controllers.keys), Set([a.id, b.id]))
    }

    func testSyncReusesTheExistingControllerForAWorkspaceItAlreadyHas() {
        let registry = WorkspaceControllerRegistry()
        let a = untrusted("/tmp/a")
        registry.sync([a])
        let first = registry.controllers[a.id]

        registry.sync([a])

        XCTAssertTrue(registry.controllers[a.id] === first, "a re-sync must not rebuild a live controller")
    }

    /// The leak this pins: the only teardown path had no callers, so a removed workspace kept
    /// polling its daemon and kept reloading its config on every file edit, forever.
    func testSyncDropsControllersForWorkspacesThatAreGone() {
        let registry = WorkspaceControllerRegistry()
        let a = untrusted("/tmp/a")
        let b = untrusted("/tmp/b")
        registry.sync([a, b])

        registry.sync([a])

        XCTAssertEqual(Array(registry.controllers.keys), [a.id])
    }

    /// The staleness this pins: the menu bar observes the registry but reads through to each
    /// controller, and SwiftUI does not propagate a nested observable's changes — so the whole
    /// "live status with the window closed" feature rendered once and then froze.
    func testAChildControllersChangeNotifiesTheRegistry() {
        let registry = WorkspaceControllerRegistry()
        let a = untrusted("/tmp/a")
        registry.sync([a])

        var notifications = 0
        let cancellable = registry.objectWillChange.sink { _ in notifications += 1 }
        defer { cancellable.cancel() }

        registry.controllers[a.id]?.lastActionError = "something happened"

        XCTAssertEqual(notifications, 1, "a nested controller's change must reach the registry's observers")
    }

    func testARemovedControllersChangeNoLongerNotifiesTheRegistry() {
        let registry = WorkspaceControllerRegistry()
        let a = untrusted("/tmp/a")
        registry.sync([a])
        let orphan = registry.controllers[a.id]
        registry.sync([])

        var notifications = 0
        let cancellable = registry.objectWillChange.sink { _ in notifications += 1 }
        defer { cancellable.cancel() }

        orphan?.lastActionError = "ignored"

        XCTAssertEqual(notifications, 0, "a dropped controller must not keep the registry subscribed")
    }
}
