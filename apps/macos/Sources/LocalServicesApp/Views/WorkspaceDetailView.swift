import AppKit
import SwiftUI

struct WorkspaceDetailView: View {
    @EnvironmentObject private var workspaceStore: WorkspaceStore
    @StateObject private var controller: WorkspaceController
    let workspace: Workspace

    init(workspace: Workspace) {
        self.workspace = workspace
        _controller = StateObject(wrappedValue: WorkspaceController(workspace: workspace))
    }

    var body: some View {
        Group {
            if !workspace.trusted {
                TrustPromptView(workspace: workspace) {
                    workspaceStore.setTrusted(true, id: workspace.id)
                    Task { await controller.connect() }
                }
            } else {
                connectedBody
            }
        }
        .navigationTitle(workspace.displayName)
        .toolbar {
            ToolbarItem {
                Menu {
                    Button("Reveal in Finder") {
                        NSWorkspace.shared.activateFileViewerSelecting([URL(fileURLWithPath: workspace.path)])
                    }
                    Divider()
                    Button("Remove Workspace", role: .destructive) { workspaceStore.remove(id: workspace.id) }
                } label: {
                    Label("More", systemImage: "ellipsis.circle")
                }
            }
        }
        .task {
            if workspace.trusted, controller.phase == .idle { await controller.connect() }
        }
    }

    @ViewBuilder
    private var connectedBody: some View {
        switch controller.phase {
        case .idle, .connecting:
            ProgressView("Starting daemon…")
                .frame(maxWidth: .infinity, maxHeight: .infinity)
        case .failed(let message):
            VStack(spacing: 12) {
                Image(systemName: "exclamationmark.triangle").font(.largeTitle).foregroundStyle(.orange)
                Text(message).multilineTextAlignment(.center).foregroundStyle(.secondary).textSelection(.enabled)
                Button("Retry") { Task { await controller.connect() } }
            }
            .padding()
            .frame(maxWidth: .infinity, maxHeight: .infinity)
        case .connected:
            ServiceListView(controller: controller)
        }
    }
}

private struct TrustPromptView: View {
    let workspace: Workspace
    let onTrust: () -> Void

    var body: some View {
        VStack(spacing: 16) {
            Image(systemName: "shield.lefthalf.filled").font(.system(size: 40)).foregroundStyle(.secondary)
            Text("Trust this folder?").font(.title2).bold()
            Text("\(workspace.path)\n\nThis folder's local-services.yaml (or local-services.config.ts) names commands the daemon will run on your behalf. Only trust folders you wrote or downloaded from somewhere you trust.")
                .font(.subheadline)
                .foregroundStyle(.secondary)
                .multilineTextAlignment(.center)
                .frame(maxWidth: 420)
            Button("Trust and Start", action: onTrust).buttonStyle(.borderedProminent)
        }
        .padding()
        .frame(maxWidth: .infinity, maxHeight: .infinity)
    }
}

private struct ServiceListView: View {
    @ObservedObject var controller: WorkspaceController

    var body: some View {
        VStack(spacing: 0) {
            if let error = controller.lastActionError {
                HStack {
                    Image(systemName: "exclamationmark.circle").foregroundStyle(.red)
                    Text(error).font(.callout).lineLimit(2)
                    Spacer()
                    Button("Dismiss") { controller.lastActionError = nil }
                }
                .padding(8)
                .background(.red.opacity(0.1))
            }
            List(controller.services) { service in
                ServiceRow(
                    service: service,
                    label: controller.catalog?.services.first(where: { $0.id == service.serviceId })?.displayName ?? service.serviceId,
                    busy: controller.actionsInFlight.contains(service.serviceId),
                    onAction: { action in Task { await controller.perform(action, serviceId: service.serviceId) } }
                )
            }
            .listStyle(.inset)
        }
    }
}

private struct ServiceRow: View {
    let service: ServiceLifecycleState
    let label: String
    let busy: Bool
    let onAction: (ManagerAction) -> Void

    var body: some View {
        HStack {
            StatusDot(state: service.displayState)
            VStack(alignment: .leading, spacing: 2) {
                Text(label)
                HStack(spacing: 6) {
                    Text(service.displayState).font(.caption).foregroundStyle(.secondary)
                    if let pid = service.identity?.pid {
                        Text("pid \(pid)").font(.caption).foregroundStyle(.secondary)
                    }
                    if let detail = service.readinessDetail, service.displayState == "failed" {
                        Text(detail).font(.caption).foregroundStyle(.red).lineLimit(1)
                    }
                }
            }
            Spacer()
            if busy {
                ProgressView().controlSize(.small)
            } else {
                actions
            }
        }
        .padding(.vertical, 2)
    }

    @ViewBuilder
    private var actions: some View {
        switch service.displayState {
        case "stopped", "failed", "orphaned":
            Button("Start") { onAction(.start) }
        case "ready", "starting":
            HStack(spacing: 6) {
                Button("Restart") { onAction(.restart) }
                Button("Stop") { onAction(.stop) }
            }
        default:
            EmptyView()
        }
    }
}

private struct StatusDot: View {
    let state: String

    private var color: Color {
        switch state {
        case "ready": return .green
        case "starting", "queued": return .yellow
        case "failed": return .red
        case "stopping": return .orange
        default: return .gray
        }
    }

    var body: some View {
        Circle().fill(color).frame(width: 8, height: 8)
    }
}
