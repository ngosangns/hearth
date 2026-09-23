import AppKit
import SwiftUI

/// Aggregates every trusted workspace's live service counts — shared by the menu bar's compact
/// label and its dropdown content so the two never disagree. Reads `registry.controllers` directly
/// rather than owning any state of its own: `WorkspaceControllerRegistry` is the single source of
/// truth, kept alive at the app level (see its doc comment) specifically so this stays accurate even
/// when the main window is closed.
struct MenuBarSummary {
    let ready: Int
    let failed: Int
    let total: Int

    @MainActor
    static func compute(workspaces: [Workspace], registry: WorkspaceControllerRegistry) -> MenuBarSummary {
        var ready = 0
        var failed = 0
        var total = 0
        for workspace in workspaces where workspace.trusted {
            guard let controller = registry.controllers[workspace.id] else { continue }
            for service in controller.services {
                total += 1
                switch service.displayState {
                case "ready": ready += 1
                case "failed": failed += 1
                default: break
                }
            }
        }
        return MenuBarSummary(ready: ready, failed: failed, total: total)
    }

    var labelText: String {
        guard total > 0 else { return "Hearth" }
        return failed > 0 ? "\(ready)/\(total) · \(failed) failed" : "\(ready)/\(total)"
    }
}

struct MenuBarLabel: View {
    @EnvironmentObject private var workspaceStore: WorkspaceStore
    @EnvironmentObject private var registry: WorkspaceControllerRegistry
    @EnvironmentObject private var menuBarPulse: MenuBarPulse

    var body: some View {
        let _ = menuBarPulse.tick
        let summary = MenuBarSummary.compute(workspaces: workspaceStore.workspaces, registry: registry)
        // Explicit icon + text, not `Label`: a MenuBarExtra renders a `Label` as its icon alone, so
        // the counts were computed on every change and never shown — the status item was a bare
        // 38pt icon, which defeated the point of a glanceable summary with the window closed. Text
        // only appears once there is something to count, so an empty or still-connecting app keeps
        // a compact icon instead of the placeholder "Hearth".
        HStack(spacing: 4) {
            Image(systemName: "server.rack")
            if summary.total > 0 {
                Text(summary.labelText)
            }
        }
    }
}

struct MenuBarContentView: View {
    @EnvironmentObject private var workspaceStore: WorkspaceStore
    @EnvironmentObject private var registry: WorkspaceControllerRegistry
    @EnvironmentObject private var menuBarPulse: MenuBarPulse
    @Environment(\.openWindow) private var openWindow
    /// The workspace whose "Stop Daemon…" item armed the confirmation dialog — per-workspace, since
    /// one content view lists them all.
    @State private var stopDaemonTarget: Workspace?

    var body: some View {
        let _ = menuBarPulse.tick
        Group {
            if workspaceStore.workspaces.isEmpty {
                Text("No workspaces yet")
            } else {
                ForEach(workspaceStore.workspaces) { workspace in
                    Menu(menuTitle(for: workspace)) {
                        Button("Open") {
                            workspaceStore.selectedId = workspace.id
                            openWindow(id: "main")
                        }
                        if workspace.trusted, let controller = registry.controllers[workspace.id], controller.phase != .connecting {
                            Button("Restart Daemon") { Task { await controller.restartDaemon() } }
                                .disabled(controller.daemonTransitionInFlight)
                        }
                        // Destructive like the toolbar button — daemon AND all its services go down,
                        // so the menu item arms a confirmation rather than firing directly.
                        if workspace.trusted, let controller = registry.controllers[workspace.id], controller.phase.mayHaveLiveDaemon {
                            Button("Stop Daemon…") { stopDaemonTarget = workspace }
                                .disabled(controller.daemonTransitionInFlight)
                        }
                        if let controller = registry.controllers[workspace.id], controller.phase == .connected {
                            Button("Stop All") { Task { await controller.stopAll() } }
                        }
                    }
                }
            }
            Divider()
            Button("Open Hearth") { openWindow(id: "main") }
            Button("Check for Updates…") {
                if let url = URL(string: "https://github.com/gnasdev/hearth/releases") {
                    NSWorkspace.shared.open(url)
                }
            }
            Button("Quit") { NSApp.terminate(nil) }
        }
        .confirmationDialog(
            "Stop this project's daemon?",
            isPresented: Binding(
                get: { stopDaemonTarget != nil },
                set: { if !$0 { stopDaemonTarget = nil } }
            ),
            titleVisibility: .visible
        ) {
            if let target = stopDaemonTarget, let controller = registry.controllers[target.id] {
                Button("Stop Daemon", role: .destructive) { Task { await controller.stopDaemon() } }
            }
            Button("Cancel", role: .cancel) {}
        } message: {
            Text("The daemon and every service it manages will be stopped.")
        }
    }

    private func menuTitle(for workspace: Workspace) -> String {
        let status = statusText(for: workspace, controller: registry.controllers[workspace.id])
        return "\(workspace.displayName) — \(status)"
    }

    private func statusText(for workspace: Workspace, controller: WorkspaceController?) -> String {
        guard workspace.trusted else { return "not trusted" }
        guard let controller else { return "…" }
        switch controller.phase {
        case .idle, .connecting: return "connecting…"
        case .stopped: return "stopped"
        case .failed(let message): return message
        case .connected:
            let ready = controller.services.filter { $0.displayState == "ready" }.count
            let failed = controller.services.filter { $0.displayState == "failed" }.count
            return failed > 0 ? "\(ready)/\(controller.services.count) ready · \(failed) failed" : "\(ready)/\(controller.services.count) ready"
        }
    }
}
