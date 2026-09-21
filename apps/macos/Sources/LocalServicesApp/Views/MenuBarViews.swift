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
        guard total > 0 else { return "Local Services" }
        return failed > 0 ? "\(ready)/\(total) · \(failed) failed" : "\(ready)/\(total)"
    }
}

struct MenuBarLabel: View {
    @EnvironmentObject private var workspaceStore: WorkspaceStore
    @EnvironmentObject private var registry: WorkspaceControllerRegistry

    var body: some View {
        let summary = MenuBarSummary.compute(workspaces: workspaceStore.workspaces, registry: registry)
        // Explicit icon + text, not `Label`: a MenuBarExtra renders a `Label` as its icon alone, so
        // the counts were computed on every change and never shown — the status item was a bare
        // 38pt icon, which defeated the point of a glanceable summary with the window closed. Text
        // only appears once there is something to count, so an empty or still-connecting app keeps
        // a compact icon instead of the placeholder "Local Services".
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
    @Environment(\.openWindow) private var openWindow

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            if workspaceStore.workspaces.isEmpty {
                Text("No workspaces yet").foregroundStyle(.secondary).padding()
            } else {
                ForEach(workspaceStore.workspaces) { workspace in
                    workspaceRow(workspace)
                    Divider()
                }
            }
            Button("Open Local Services") { openWindow(id: "main") }
                .padding(.horizontal, 12).padding(.vertical, 8)
            Divider()
            Button("Quit") { NSApp.terminate(nil) }
                .padding(.horizontal, 12).padding(.bottom, 8)
        }
        .frame(width: 280)
    }

    @ViewBuilder
    private func workspaceRow(_ workspace: Workspace) -> some View {
        let controller = registry.controllers[workspace.id]
        HStack {
            Circle().fill(dotColor(for: workspace, controller: controller)).frame(width: 8, height: 8)
            VStack(alignment: .leading, spacing: 1) {
                Text(workspace.displayName)
                Text(statusText(for: workspace, controller: controller)).font(.caption).foregroundStyle(.secondary)
            }
            Spacer()
            if let controller, controller.phase == .connected {
                Button("Start All") { Task { await controller.startAll() } }.controlSize(.small)
                Button("Stop All") { Task { await controller.stopAll() } }.controlSize(.small)
            }
        }
        .padding(.horizontal, 12).padding(.vertical, 6)
    }

    private func statusText(for workspace: Workspace, controller: WorkspaceController?) -> String {
        guard workspace.trusted else { return "not trusted" }
        guard let controller else { return "…" }
        switch controller.phase {
        case .idle, .connecting: return "connecting…"
        case .failed(let message): return message
        case .connected:
            let ready = controller.services.filter { $0.displayState == "ready" }.count
            let failed = controller.services.filter { $0.displayState == "failed" }.count
            return failed > 0 ? "\(ready)/\(controller.services.count) ready · \(failed) failed" : "\(ready)/\(controller.services.count) ready"
        }
    }

    private func dotColor(for workspace: Workspace, controller: WorkspaceController?) -> Color {
        guard workspace.trusted, let controller else { return .gray }
        switch controller.phase {
        case .failed: return .red
        case .connected:
            return controller.services.contains { $0.displayState == "failed" } ? .red : .green
        default: return .yellow
        }
    }
}
