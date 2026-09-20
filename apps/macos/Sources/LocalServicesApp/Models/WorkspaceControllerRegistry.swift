import Combine
import Foundation

/// Owns every workspace's `WorkspaceController` at the app level (not per-view), so a connection
/// survives switching away from a workspace in the sidebar, and so the menu bar summary
/// (`MenuBarContentView`) has live status for every trusted workspace regardless of which one — if
/// any — is showing in the main window.
///
/// `controllers` is the single source of truth and `sync(_:)` is the only thing that mutates it:
/// views look controllers up read-only (`controllers[id]`) and never create one, because creating
/// one from inside a SwiftUI `body` would publish a change during a view update.
@MainActor
final class WorkspaceControllerRegistry: ObservableObject {
    @Published private(set) var controllers: [UUID: WorkspaceController] = [:]

    /// A `WorkspaceController` is itself an `ObservableObject`, and SwiftUI does NOT propagate a
    /// nested observable's changes through the object holding it. Without these forwarded
    /// subscriptions the menu bar — which observes only this registry, then reads through to
    /// `controller.services`/`controller.phase` — would render once and then never update again,
    /// since `controllers` only mutates when a workspace is added or removed.
    private var childChanges: [UUID: AnyCancellable] = [:]

    /// Reconciles the live controller set against the workspace list: creates one per workspace,
    /// tears down controllers for workspaces that are gone, and connects any trusted workspace that
    /// is not already connected/connecting. An untrusted workspace still gets a controller (the
    /// detail view needs one to render its trust gate) but is never auto-connected — the same trust
    /// gate as opening it by hand.
    func sync(_ workspaces: [Workspace]) {
        let live = Set(workspaces.map(\.id))
        for id in controllers.keys where !live.contains(id) {
            remove(id: id)
        }
        for workspace in workspaces {
            let controller = controller(for: workspace)
            if workspace.trusted, controller.phase == .idle {
                Task { await controller.connect() }
            }
        }
    }

    private func controller(for workspace: Workspace) -> WorkspaceController {
        if let existing = controllers[workspace.id] {
            return existing
        }
        let controller = WorkspaceController(workspace: workspace)
        childChanges[workspace.id] = controller.objectWillChange.sink { [weak self] _ in
            self?.objectWillChange.send()
        }
        controllers[workspace.id] = controller
        return controller
    }

    /// Stops the controller's poll loop and config watcher before dropping it — otherwise a removed
    /// workspace would keep polling its daemon and keep reloading its config on every file edit,
    /// forever, with no UI referencing it.
    private func remove(id: UUID) {
        controllers[id]?.stop()
        controllers.removeValue(forKey: id)
        childChanges.removeValue(forKey: id)
    }
}
