import AppKit
import SwiftUI

struct ContentView: View {
    @EnvironmentObject private var workspaceStore: WorkspaceStore
    @EnvironmentObject private var registry: WorkspaceControllerRegistry
    @Environment(\.openWindow) private var openWindow
    /// Workspace whose log tails are polling. Switching away stops them without tearing the detail down.
    @State private var visibleWorkspaceId: UUID?

    var body: some View {
        NavigationSplitView {
            workspaceList
                .navigationTitle("Workspaces")
                .toolbar {
                    ToolbarItem(placement: .primaryAction) {
                        Button(action: addWorkspace) {
                            Label("Add Folder", systemImage: "plus")
                        }
                        .help("Add Folder")
                    }
                }
        } detail: {
            detail
                .animation(Motion.layout, value: detailBranch)
        }
        .navigationSplitViewStyle(.balanced)
        .navigationSplitViewColumnWidth(min: 220, ideal: 260, max: 400)
        .safeAreaInset(edge: .top) {
            if let error = workspaceStore.loadError {
                ErrorBanner(text: error) { workspaceStore.dismissLoadError() }
            }
        }
        .onAppear {
            captureWindowOpener()
            visibleWorkspaceId = workspaceStore.selectedId
        }
        .onChange(of: workspaceStore.selectedId) { newId in
            if let previous = visibleWorkspaceId, previous != newId {
                registry.controllers[previous]?.stopLogTails()
            }
            visibleWorkspaceId = newId
        }
    }

    /// Which placeholder or workspace pane is showing. Stays `"workspace"` across sidebar switches
    /// so that change updates the mounted detail in place instead of crossfading the log.
    private var detailBranch: String {
        if let selection = workspaceStore.selectedId, workspaceStore.workspaces.contains(where: { $0.id == selection }) {
            return registry.controllers[selection] == nil ? "preparing" : "workspace"
        }
        return workspaceStore.workspaces.isEmpty ? "empty" : "unselected"
    }

    private var workspaceList: some View {
        List(selection: $workspaceStore.selectedId) {
            ForEach(workspaceStore.workspaces) { workspace in
                WorkspaceRow(workspace: workspace, controller: registry.controllers[workspace.id]) {
                    workspaceStore.remove(id: workspace.id)
                }
                .tag(workspace.id)
            }
            // In the scroll content, under the last row. With no workspaces it is the first row.
            Button(action: addWorkspace) {
                Label("Add Folder", systemImage: "plus")
            }
            .buttonStyle(.plain)
            .modifier(SidebarAddRow())
        }
        .listStyle(.sidebar)
    }

    @ViewBuilder
    private var detail: some View {
        if let selection = workspaceStore.selectedId, let workspace = workspaceStore.workspaces.first(where: { $0.id == selection }),
           let controller = registry.controllers[workspace.id] {
            // A read-only lookup: the registry creates controllers in `sync(_:)`, never here —
            // creating one from inside `body` would publish a change during a view update. The
            // controller is resolved here (where `registry` — an @EnvironmentObject — is
            // actually available) and passed down, rather than WorkspaceDetailView resolving it
            // itself in an `init`, where @EnvironmentObject cannot be read yet.
            // No `.id(workspace.id)`: that destroyed the split view and the log text view on every
            // sidebar click. Dialog state resets inside the detail when `workspace.id` changes.
            WorkspaceDetailView(controller: controller, workspace: workspace)
                .transition(.opacity)
        } else if workspaceStore.selectedId != nil {
            // Only reachable for the frame between a workspace being added and `sync(_:)`
            // running for the changed list.
            ContentUnavailableViewCompat(title: "Preparing…", message: "Setting up this workspace.", systemImage: "folder")
                .navigationTitle("Hearth")
                .transition(.opacity)
        } else if workspaceStore.workspaces.isEmpty {
            ContentUnavailableViewCompat(
                title: "No workspaces",
                message: "Add a folder that has a hearth.yaml to manage its services here.",
                systemImage: "folder.badge.plus"
            ) {
                Button("Add Folder", action: addWorkspace)
                    .buttonStyle(.borderedProminent)
            }
            .navigationTitle("Hearth")
            .transition(.opacity)
        } else {
            ContentUnavailableViewCompat(title: "Select a workspace", message: "Choose a folder from the sidebar.", systemImage: "folder")
                .navigationTitle("Hearth")
                .transition(.opacity)
        }
    }

    private func addWorkspace() {
        let panel = NSOpenPanel()
        panel.canChooseDirectories = true
        panel.canChooseFiles = false
        panel.allowsMultipleSelection = false
        panel.prompt = "Add"
        guard panel.runModal() == .OK, let url = panel.url else { return }
        _ = workspaceStore.add(path: url.path)
    }

    /// Captures the WindowGroup's `openWindow` action so the AppDelegate can reopen the main window
    /// after it has been closed (dock reopen, second-instance handoff).
    private func captureWindowOpener() {
        MainWindow.open = { openWindow(id: MainWindow.id) }
    }
}

/// `selectionDisabled` is macOS 14. The Add row must not become the list selection.
private struct SidebarAddRow: ViewModifier {
    func body(content: Content) -> some View {
        if #available(macOS 14, *) {
            content.selectionDisabled(true)
        } else {
            content
        }
    }
}

private struct WorkspaceRow: View {
    let workspace: Workspace
    /// The live controller, if the registry has made one — drives the status line.
    let controller: WorkspaceController?
    let onRemove: () -> Void
    @State private var confirmRemove = false

    var body: some View {
        if let controller {
            LiveWorkspaceRow(workspace: workspace, controller: controller, onRemove: onRemove)
        } else {
            row(status: workspace.trusted ? workspace.displayPath : "Not trusted", failed: false)
        }
    }

    private func row(status: String, failed: Bool) -> some View {
        HStack(spacing: 8) {
            Label {
                VStack(alignment: .leading, spacing: 2) {
                    Text(workspace.displayName)
                        .lineLimit(1)
                    Text(status)
                        .font(.caption)
                        .foregroundStyle(failed ? AnyShapeStyle(.red) : AnyShapeStyle(.secondary))
                        .lineLimit(1)
                }
            } icon: {
                Image(systemName: workspace.existsOnDisk ? "folder" : "folder.badge.questionmark")
                    .foregroundStyle(workspace.existsOnDisk ? Color.accentColor : .secondary)
            }
            Spacer(minLength: 0)
            SidebarRemoveButton { confirmRemove = true }
        }
        .help(workspace.path)
        .contextMenu {
            Button("Reveal in Finder") {
                NSWorkspace.shared.activateFileViewerSelecting([URL(fileURLWithPath: workspace.path)])
            }
            Button("Remove Workspace", role: .destructive) { confirmRemove = true }
        }
        .alert("Remove \"\(workspace.displayName)\"?", isPresented: $confirmRemove) {
            Button("Remove", role: .destructive, action: onRemove)
        } message: {
            Text("Hearth will stop showing this folder. The folder stays on disk.")
        }
    }
}

/// Observes the controller so the status line tracks live counts. `WorkspaceRow` gets the
/// controller as a plain value from `ContentView`, which does not observe controllers.
private struct LiveWorkspaceRow: View {
    let workspace: Workspace
    @ObservedObject var controller: WorkspaceController
    let onRemove: () -> Void
    @State private var confirmRemove = false

    var body: some View {
        HStack(spacing: 8) {
            Label {
                VStack(alignment: .leading, spacing: 2) {
                    Text(workspace.displayName)
                        .lineLimit(1)
                    Text(statusLine)
                        .font(.caption)
                        .foregroundStyle(failed ? AnyShapeStyle(.red) : AnyShapeStyle(.secondary))
                        .lineLimit(1)
                }
            } icon: {
                Image(systemName: workspace.existsOnDisk ? "folder" : "folder.badge.questionmark")
                    .foregroundStyle(workspace.existsOnDisk ? Color.accentColor : .secondary)
            }
            Spacer(minLength: 0)
            SidebarRemoveButton { confirmRemove = true }
        }
        .help(workspace.path)
        .contextMenu {
            Button("Reveal in Finder") {
                NSWorkspace.shared.activateFileViewerSelecting([URL(fileURLWithPath: workspace.path)])
            }
            Button("Remove Workspace", role: .destructive) { confirmRemove = true }
        }
        .alert("Remove \"\(workspace.displayName)\"?", isPresented: $confirmRemove) {
            Button("Remove", role: .destructive, action: onRemove)
        } message: {
            Text("Hearth will stop showing this folder. The folder stays on disk.")
        }
    }

    private var failed: Bool {
        if case .failed = controller.phase { return true }
        return controller.phase == .connected && controller.counts.failed > 0
    }

    private var statusLine: String {
        if !workspace.trusted { return "Not trusted" }
        switch controller.phase {
        case .connected:
            let counts = controller.counts
            if counts.total == 0 { return "No services" }
            if counts.failed > 0 { return "\(counts.ready)/\(counts.total) · \(counts.failed) failed" }
            return "\(counts.ready)/\(counts.total) ready"
        case .connecting:
            return "Starting…"
        case .idle:
            // Resting state — the daemon has never been started for this workspace in this
            // session. A spinner here made every untouched row look stuck loading.
            return workspace.displayPath
        case .failed:
            return "Daemon failed"
        case .stopped:
            return "Daemon stopped"
        }
    }
}

/// Trailing control on a sidebar row. A real button so the click does not also select the row.
private struct SidebarRemoveButton: View {
    let action: () -> Void

    var body: some View {
        Button(action: action) {
            Image(systemName: "trash")
        }
        .buttonStyle(.borderless)
        .controlSize(.small)
        .foregroundStyle(.secondary)
        .accessibilityLabel("Remove Workspace")
        .help("Remove Workspace")
    }
}


