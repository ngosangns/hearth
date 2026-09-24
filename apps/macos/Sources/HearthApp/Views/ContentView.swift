import AppKit
import SwiftUI

struct ContentView: View {
    @EnvironmentObject private var workspaceStore: WorkspaceStore
    @EnvironmentObject private var registry: WorkspaceControllerRegistry
    @Environment(\.openWindow) private var openWindow

    var body: some View {
        NavigationSplitView {
            List(workspaceStore.workspaces, selection: $workspaceStore.selectedId) { workspace in
                WorkspaceRow(workspace: workspace).tag(workspace.id)
            }
            .navigationTitle("Workspaces")
            .toolbar {
                ToolbarItem { Button(action: addWorkspace) { Label("Add Folder", systemImage: "plus") } }
            }
            .overlay {
                if workspaceStore.workspaces.isEmpty {
                    ContentUnavailableViewCompat(
                        title: "No workspaces",
                        message: "Add a folder that has a hearth.yaml to manage its services here.",
                        systemImage: "folder.badge.plus"
                    )
                }
            }
        } detail: {
            if let selection = workspaceStore.selectedId, let workspace = workspaceStore.workspaces.first(where: { $0.id == selection }),
               let controller = registry.controllers[workspace.id] {
                // A read-only lookup: the registry creates controllers in `sync(_:)`, never here —
                // creating one from inside `body` would publish a change during a view update. The
                // controller is resolved here (where `registry` — an @EnvironmentObject — is
                // actually available) and passed down, rather than WorkspaceDetailView resolving it
                // itself in an `init`, where @EnvironmentObject cannot be read yet.
                WorkspaceDetailView(controller: controller, workspace: workspace)
                    .id(workspace.id) // resets per-view state (e.g. an open log sheet) on selection change
            } else if workspaceStore.selectedId != nil {
                // Only reachable for the frame between a workspace being added and `sync(_:)`
                // running for the changed list.
                ContentUnavailableViewCompat(title: "Preparing…", message: "Setting up this workspace.", systemImage: "folder")
            } else {
                ContentUnavailableViewCompat(title: "Select a workspace", message: "Choose a folder from the sidebar.", systemImage: "folder")
            }
        }
        .onAppear { captureWindowOpener() }
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

private struct WorkspaceRow: View {
    let workspace: Workspace

    var body: some View {
        HStack {
            Image(systemName: workspace.existsOnDisk ? "folder" : "folder.badge.questionmark")
                .foregroundStyle(workspace.existsOnDisk ? .primary : .secondary)
            VStack(alignment: .leading) {
                Text(workspace.displayName)
                Text(workspace.path).font(.caption).foregroundStyle(.secondary).lineLimit(1).truncationMode(.middle)
            }
        }
    }
}

/// `ContentUnavailableView` is macOS 14+; this app targets macOS 13, so a small compatible stand-in.
struct ContentUnavailableViewCompat: View {
    let title: String
    let message: String
    let systemImage: String

    var body: some View {
        VStack(spacing: 8) {
            Image(systemName: systemImage).font(.system(size: 36)).foregroundStyle(.secondary)
            Text(title).font(.headline)
            Text(message).font(.subheadline).foregroundStyle(.secondary).multilineTextAlignment(.center)
        }
        .padding()
        .frame(maxWidth: .infinity, maxHeight: .infinity)
    }
}
