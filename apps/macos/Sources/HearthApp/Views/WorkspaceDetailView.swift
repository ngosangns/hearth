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
                ToolbarItem { Button("Stop All") { Task { await controller.stopAll() } } }
            }
            ToolbarItem {
                Menu {
                    Button("Reveal in Finder") {
                        NSWorkspace.shared.activateFileViewerSelecting([URL(fileURLWithPath: workspace.path)])
                    }
                    Button("Check for Updates…") {
                        if let url = URL(string: "https://github.com/gnasdev/hearth/releases") {
                            NSWorkspace.shared.open(url)
                        }
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
            Text("\(workspace.path)\n\nThis folder's hearth.yaml names commands the daemon will run on your behalf. Only trust folders you wrote or downloaded from somewhere you trust.")
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
/// whichever row is focused/selected. Selection and log tails live on `WorkspaceController` so they
/// survive switching workspaces (this view is recreated via `.id(workspace.id)`).
private struct ServiceListView: View {
    @ObservedObject var controller: WorkspaceController

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
                List(controller.services, selection: $controller.selectedServiceId) { service in
                    ServiceRow(
                        service: service,
                        label: controller.catalog?.services.first(where: { $0.id == service.serviceId })?.displayName ?? service.serviceId,
                        busy: controller.actionsInFlight.contains(service.serviceId),
                        urls: controller.urls(for: service.serviceId),
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
    }

    @ViewBuilder
    private var logPanel: some View {
        if let selectedServiceId = controller.selectedServiceId, let logController = controller.logController(for: selectedServiceId) {
            let label = controller.catalog?.services.first(where: { $0.id == selectedServiceId })?.displayName ?? selectedServiceId
            ServiceLogPanel(serviceLabel: label, log: logController)
                .id(selectedServiceId)
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
    let urls: [ResolvedServiceUrl]
    let onAction: (ManagerAction) -> Void

    /// Same rule as `hearthd urls` and both TUIs: in-flight states count as running.
    private var isRunning: Bool { ["ready", "starting"].contains(service.displayState) }

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
                if !urls.isEmpty {
                    links
                }
            }
            Spacer()
            actions
        }
        .padding(.vertical, 2)
        .contentShape(Rectangle()) // the whole row is clickable/selectable, not just the text
    }

    /// One link per registered URL. A URL that needs the service running is dimmed while it is not,
    /// so a dead link is recognisable before anyone clicks it — still clickable, since the catalog
    /// can't know for certain (another process may be serving it).
    private var links: some View {
        HStack(spacing: 8) {
            ForEach(urls, id: \.self) { entry in
                if let destination = URL(string: entry.url) {
                    let stale = entry.requiresRunning && !isRunning
                    Link(destination: destination) {
                        Label(entry.displayName, systemImage: "arrow.up.right.square")
                            .labelStyle(.titleAndIcon)
                            .lineLimit(1)
                    }
                    .font(.caption)
                    .opacity(stale ? 0.45 : 1)
                    .help(stale ? "\(entry.url) — \(label) is not running" : entry.url)
                }
            }
        }
    }

    @ViewBuilder
    private var actions: some View {
        HStack(spacing: 6) {
            if busy {
                ProgressView().controlSize(.small)
            }
            if busy {
                // Keep Stop available for the whole start/restart flight so the user can abort
                // without waiting for readiness.
                if service.displayState != "stopping" {
                    Button("Stop") { onAction(.stop) }
                }
            } else {
                switch service.displayState {
                case "stopped", "failed", "orphaned":
                    Button("Start") { onAction(.start) }
                case "ready", "starting":
                    Button("Restart") { onAction(.restart) }
                    Button("Stop") { onAction(.stop) }
                // A service queued behind another operation on the same target stays `queued-start`
                // until something clears it — without this the row offered no way out of it at all.
                case "queued":
                    Button("Cancel") { onAction(.stop) }
                // Externally-owned (an adopted docker/tailnet unit): the daemon observes it rather
                // than owning it, so Restart is not ours to offer, but Stop is what the CLI does.
                case "external":
                    Button("Stop") { onAction(.stop) }
                // `stopping` is genuinely in-flight — no extra action, the poll will move it.
                default:
                    EmptyView()
                }
            }
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
