import AppKit
import SwiftUI

struct ContentView: View {
    @EnvironmentObject private var workspaceStore: WorkspaceStore
    @EnvironmentObject private var registry: WorkspaceControllerRegistry
    @Environment(\.openWindow) private var openWindow

    var body: some View {
        NavigationSplitView {
            List(workspaceStore.workspaces, selection: $workspaceStore.selectedId) { workspace in
                WorkspaceRow(workspace: workspace, controller: registry.controllers[workspace.id])
                    .tag(workspace.id)
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
            .safeAreaInset(edge: .top) {
                if let error = workspaceStore.loadError {
                    ErrorBanner(text: error) { workspaceStore.dismissLoadError() }
                }
            }
            .safeAreaInset(edge: .bottom) {
                SummaryStrip(workspaces: workspaceStore.workspaces, registry: registry, pulse: registry.menuBarPulse)
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

/// Xcode-style status strip: the same glanceable summary the menu bar shows, so the number stays
/// visible even while the sidebar is scrolled. Its own view observing `menuBarPulse`: `ContentView`
/// deliberately does not observe the controllers (see `WorkspaceControllerRegistry`), so computed
/// inline there it only redrew when something unrelated invalidated the window.
private struct SummaryStrip: View {
    let workspaces: [Workspace]
    let registry: WorkspaceControllerRegistry
    @ObservedObject var pulse: MenuBarPulse

    var body: some View {
        let summary = MenuBarSummary.compute(workspaces: workspaces, registry: registry)
        if summary.total > 0 {
            HStack(spacing: 6) {
                Image(systemName: "server.rack")
                if summary.failed > 0 {
                    Text("\(summary.ready)/\(summary.total) ready · \(summary.failed) failed")
                        .foregroundStyle(.red)
                } else {
                    Text("\(summary.ready)/\(summary.total) services ready")
                        .foregroundStyle(.secondary)
                }
            }
            .font(.caption)
            .padding(.horizontal, 12)
            .padding(.vertical, 6)
            .frame(maxWidth: .infinity, alignment: .leading)
            .background(.bar)
        }
    }
}

private struct WorkspaceRow: View {
    let workspace: Workspace
    /// The live controller, if the registry has made one — drives the trailing status.
    let controller: WorkspaceController?

    var body: some View {
        let exists = workspace.existsOnDisk
        HStack(spacing: 8) {
            Image(systemName: exists ? "folder.fill" : "folder.badge.questionmark")
                .foregroundStyle(exists ? Color.accentColor : .secondary)
            VStack(alignment: .leading, spacing: 2) {
                Text(workspace.displayName)
                    .fontWeight(.medium)
                Text(workspace.path)
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
                    .truncationMode(.middle)
            }
            Spacer()
            if let controller {
                WorkspaceRowStatus(controller: controller)
            } else if !workspace.trusted {
                Image(systemName: "lock").foregroundStyle(.secondary)
            }
        }
        .padding(.vertical, 2)
    }
}

/// A compact trailing status: a ready/total count while connected, a glyph for anything else.
/// A separate view so it can observe the controller — `WorkspaceRow` gets it as a plain value from
/// `ContentView`, which does not observe controllers, so the count went stale there.
private struct WorkspaceRowStatus: View {
    @ObservedObject var controller: WorkspaceController

    var body: some View {
        switch controller.phase {
        case .connected:
            let counts = controller.counts
            if counts.failed > 0 {
                Text("\(counts.ready)/\(counts.total) · \(counts.failed) failed")
                    .font(.caption).monospacedDigit().foregroundStyle(.red)
            } else {
                Text("\(counts.ready)/\(counts.total)")
                    .font(.caption).monospacedDigit().foregroundStyle(.secondary)
            }
        case .connecting:
            ActionSpinner()
        case .idle:
            // Resting state — the daemon has never been started for this workspace in this
            // session. Showing a spinner here made every untouched row look stuck loading.
            EmptyView()
        case .failed:
            Image(systemName: "exclamationmark.triangle.fill").foregroundStyle(.orange)
        case .stopped:
            Image(systemName: "stop.circle").foregroundStyle(.secondary)
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
