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

    /// Nested controller updates must reach the menu bar pulse, not the registry itself — publishing
    /// through the registry rebuilt the main window on every 2s poll.
    func testAChildControllersChangePulsesTheMenuBarNotTheRegistry() {
        let registry = WorkspaceControllerRegistry()
        let a = untrusted("/tmp/a")
        registry.sync([a])

        var registryNotes = 0
        var pulseNotes = 0
        let registryWatch = registry.objectWillChange.sink { registryNotes += 1 }
        let pulseWatch = registry.menuBarPulse.objectWillChange.sink { pulseNotes += 1 }
        defer {
            registryWatch.cancel()
            pulseWatch.cancel()
        }

        registry.controllers[a.id]?.lastActionError = "something happened"

        XCTAssertEqual(registryNotes, 0, "a nested controller's change must not rebuild ContentView")
        XCTAssertEqual(pulseNotes, 1, "the menu bar pulse must still see nested controller changes")
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
