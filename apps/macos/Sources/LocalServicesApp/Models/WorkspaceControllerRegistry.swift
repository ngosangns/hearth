import Foundation

/// Owns every trusted workspace's `WorkspaceController` at the app level (not per-view), so a
/// connection survives switching away from a workspace in the sidebar, and so the menu bar summary
/// (`MenuBarContentView`) has live status for every trusted workspace regardless of which one — if
/// any — is showing in the main window.
@MainActor
final class WorkspaceControllerRegistry: ObservableObject {
    @Published private(set) var controllers: [UUID: WorkspaceController] = [:]

    func controller(for workspace: Workspace) -> WorkspaceController {
        if let existing = controllers[workspace.id] {
            return existing
        }
        let controller = WorkspaceController(workspace: workspace)
        controllers[workspace.id] = controller
        return controller
    }

    func remove(id: UUID) {
        controllers[id]?.stop()
        controllers.removeValue(forKey: id)
    }

    /// Connects every trusted workspace not already connected/connecting. Called on launch and
    /// whenever the workspace list changes (a folder added, or trusted for the first time) — an
    /// untrusted workspace is never auto-connected, same trust gate as opening it by hand.
    func connectTrusted(_ workspaces: [Workspace]) {
        for workspace in workspaces where workspace.trusted {
            let controller = controller(for: workspace)
            if controller.phase == .idle {
                Task { await controller.connect() }
            }
        }
    }
}
