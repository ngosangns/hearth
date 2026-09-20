import AppKit
import SwiftUI

struct WorkspaceDetailView: View {
    @EnvironmentObject private var workspaceStore: WorkspaceStore
    /// Resolved by the caller (`ContentView`) from the shared `WorkspaceControllerRegistry` — this
    /// view never creates its own; the registry owns the connection's lifetime so it survives the
    /// user switching away and back, and so the menu bar sees the same live state.
    @ObservedObject var controller: WorkspaceController
    let workspace: Workspace

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
            if workspace.trusted, controller.phase == .connected {
                ToolbarItem { Button("Start All") { Task { await controller.startAll() } } }
                ToolbarItem { Button("Stop All") { Task { await controller.stopAll() } } }
            }
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

/// A master-detail split: the service list on the left drives a live log panel on the right for
/// whichever row is focused/selected — no separate button-into-modal-sheet step. This also closes the
/// bug class a modal log sheet had: a sheet captured a serviceId once, at the moment it was opened, so
/// a service removed from the catalog underneath it (a config-file edit + hot-reload) left the sheet
/// polling a dead id forever, surfacing a raw `service_not_found` error with no way to recover short
/// of closing it. Selection here is just a `String?` re-checked against the live `controller.services`
/// on every update (`onChange` below), so a vanished service clears itself automatically.
private struct ServiceListView: View {
    @ObservedObject var controller: WorkspaceController
    @State private var selectedServiceId: String?

    var body: some View {
        HSplitView {
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
                List(controller.services, selection: $selectedServiceId) { service in
                    ServiceRow(
                        service: service,
                        label: controller.catalog?.services.first(where: { $0.id == service.serviceId })?.displayName ?? service.serviceId,
                        busy: controller.actionsInFlight.contains(service.serviceId),
                        onAction: { action in Task { await controller.perform(action, serviceId: service.serviceId) } }
                    )
                    .tag(service.serviceId)
                }
                .listStyle(.inset)
            }
            .frame(minWidth: 220, idealWidth: 240, maxWidth: 320)

            logPanel
                .frame(minWidth: 360, maxWidth: .infinity, maxHeight: .infinity)
        }
        // A hot-reloaded catalog can drop the selected service between polls (see this type's own
        // doc comment) — never leave the panel pointed at an id that no longer exists.
        .onChange(of: controller.services) { services in
            if let id = selectedServiceId, !services.contains(where: { $0.serviceId == id }) {
                selectedServiceId = nil
            }
        }
    }

    @ViewBuilder
    private var logPanel: some View {
        if let selectedServiceId, let logController = controller.makeLogController(serviceId: selectedServiceId) {
            let label = controller.catalog?.services.first(where: { $0.id == selectedServiceId })?.displayName ?? selectedServiceId
            ServiceLogPanel(serviceLabel: label, controller: logController)
                .id(selectedServiceId) // fresh LogController (and its poll loop) per selected service
        } else {
            VStack(spacing: 8) {
                Image(systemName: "doc.plaintext").font(.system(size: 32)).foregroundStyle(.secondary)
                Text("Select a service to view its logs").foregroundStyle(.secondary)
            }
            .frame(maxWidth: .infinity, maxHeight: .infinity)
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
        .contentShape(Rectangle()) // the whole row is clickable/selectable, not just the text
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
        // A service parked behind a dependency that never came up stays `queued-start` until
        // something clears it, and `Stop All` deliberately skips that state — so without this the
        // row offered no way out of it at all.
        case "queued":
            Button("Cancel") { onAction(.stop) }
        // Externally-owned (an adopted docker/tailnet unit): the daemon observes it rather than
        // owning it, so Restart is not ours to offer, but Stop is what the CLI does here too.
        case "external":
            Button("Stop") { onAction(.stop) }
        // `stopping` is genuinely in-flight — no action, the poll will move it to `stopped`.
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
