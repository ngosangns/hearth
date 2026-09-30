import AppKit
import SwiftUI

/// Aggregates every trusted workspace's live service counts — shared by the menu bar's compact
/// label and its dropdown content so the two never disagree. Reads `registry.controllers` directly
/// rather than owning any state of its own: `WorkspaceControllerRegistry` is the single source of
/// truth, kept alive at the app level (see its doc comment) specifically so this stays accurate even
/// when the main window is closed.
struct MenuBarSummary {
    let counts: ServiceCounts
    var ready: Int { counts.ready }
    var failed: Int { counts.failed }
    var total: Int { counts.total }

    @MainActor
    static func compute(workspaces: [Workspace], registry: WorkspaceControllerRegistry) -> MenuBarSummary {
        var counts = ServiceCounts()
        for workspace in workspaces where workspace.trusted {
            guard let controller = registry.controllers[workspace.id] else { continue }
            let workspaceCounts = controller.counts
            counts.ready += workspaceCounts.ready
            counts.failed += workspaceCounts.failed
            counts.total += workspaceCounts.total
        }
        return MenuBarSummary(counts: counts)
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
                            openWindow(id: MainWindow.id)
                        }
                        if workspace.trusted, let controller = registry.controllers[workspace.id], controller.phase != .connecting {
                            Button("Restart Daemon") { Task { await controller.restartDaemon() } }
                                .disabled(controller.daemonTransitionInFlight)
                        }
                        // Destructive like the toolbar button — daemon AND all its services go down,
                        // so the menu item confirms rather than firing directly. An `NSAlert`, not
                        // a `confirmationDialog` (see `StopDaemonConfirmation.runModal`), shown
                        // after the menu has finished closing.
                        if workspace.trusted, let controller = registry.controllers[workspace.id], controller.phase.mayHaveLiveDaemon {
                            Button("Stop Daemon…") {
                                DispatchQueue.main.async {
                                    guard StopDaemonConfirmation.runModal(workspaceName: workspace.displayName) else { return }
                                    Task { await controller.stopDaemon() }
                                }
                            }
                            .disabled(controller.daemonTransitionInFlight)
                        }
                        if let controller = registry.controllers[workspace.id], controller.phase == .connected {
                            Button("Stop All") { Task { await controller.stopAll() } }
                        }
                    }
                }
            }
            Divider()
            Button("Shared Services…") { openWindow(id: SharedWindow.id) }
            Divider()
            Button("Open Hearth") { openWindow(id: MainWindow.id) }
            Button("Check for Updates…") { UpdateController.shared.checkForUpdates() }
            if UpdateController.shared.isRunning {
                Toggle("Check for Updates Automatically", isOn: UpdateController.shared.automaticChecksBinding)
            }
            Button("Quit") { NSApp.terminate(nil) }
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
            let counts = controller.counts
            return counts.failed > 0 ? "\(counts.ready)/\(counts.total) ready · \(counts.failed) failed" : "\(counts.ready)/\(counts.total) ready"
        }
    }
}
