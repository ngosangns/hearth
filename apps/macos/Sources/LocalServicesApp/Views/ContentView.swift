import AppKit
import SwiftUI

struct ContentView: View {
    @EnvironmentObject private var workspaceStore: WorkspaceStore
    @EnvironmentObject private var registry: WorkspaceControllerRegistry
    @State private var selection: UUID?

    var body: some View {
        NavigationSplitView {
            List(workspaceStore.workspaces, selection: $selection) { workspace in
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
                        message: "Add a folder that has a local-services.yaml to manage its services here.",
                        systemImage: "folder.badge.plus"
                    )
                }
            }
        } detail: {
            if let selection, let workspace = workspaceStore.workspaces.first(where: { $0.id == selection }) {
                // The controller is resolved here (where `registry` — an @EnvironmentObject — is
                // actually available) and passed down, rather than WorkspaceDetailView resolving it
                // itself in an `init`, where @EnvironmentObject cannot be read yet.
                WorkspaceDetailView(controller: registry.controller(for: workspace), workspace: workspace)
                    .id(workspace.id) // resets per-view state (e.g. an open log sheet) on selection change
            } else {
                ContentUnavailableViewCompat(title: "Select a workspace", message: "Choose a folder from the sidebar.", systemImage: "folder")
            }
        }
    }

    private func addWorkspace() {
        let panel = NSOpenPanel()
        panel.canChooseDirectories = true
        panel.canChooseFiles = false
        panel.allowsMultipleSelection = false
        panel.prompt = "Add"
        guard panel.runModal() == .OK, let url = panel.url else { return }
        let workspace = workspaceStore.add(path: url.path)
        selection = workspace.id
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
private struct ContentUnavailableViewCompat: View {
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
