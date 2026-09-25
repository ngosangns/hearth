import AppKit
import SwiftUI

struct WorkspaceDetailView: View {
    @EnvironmentObject private var workspaceStore: WorkspaceStore
    /// Resolved by the caller (`ContentView`) from the shared `WorkspaceControllerRegistry` — this
    /// view never creates its own; the registry owns the connection's lifetime so it survives the
    /// user switching away and back, and so the menu bar sees the same live state.
    @ObservedObject var controller: WorkspaceController
    let workspace: Workspace
    @State private var confirmStopDaemon = false

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
            // Available in every phase but `connecting` — a failed workspace is exactly where a
            // daemon swap is worth trying, and `connecting` is the in-flight state (the controller
            // guards re-entry on it too, so a double click cannot stack two swaps).
            if workspace.trusted, controller.phase != .connecting {
                ToolbarItem {
                    Button("Restart Daemon") { Task { await controller.restartDaemon() } }
                        .help("Replace this project's hearth daemon. Its services keep running and are re-adopted by the new daemon.")
                        .disabled(controller.daemonTransitionInFlight)
                }
            }
            // `stopDaemon` takes the daemon AND all its services down — destructive enough to
            // confirm, and only offered while the daemon may be alive (connected, or failed with a
            // possibly-wedged daemon still running).
            if workspace.trusted, controller.phase.mayHaveLiveDaemon {
                ToolbarItem {
                    Button("Stop Daemon") { confirmStopDaemon = true }
                        .disabled(controller.daemonTransitionInFlight)
                        .confirmationDialog(
                            "Stop this project's daemon?",
                            isPresented: $confirmStopDaemon,
                            titleVisibility: .visible
                        ) {
                            Button("Stop Daemon", role: .destructive) { Task { await controller.stopDaemon() } }
                            Button("Cancel", role: .cancel) {}
                        } message: {
                            Text("The daemon and every service it manages will be stopped.")
                        }
                }
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
        case .stopped:
            VStack(spacing: 12) {
                Image(systemName: "stop.circle").font(.largeTitle).foregroundStyle(.secondary)
                Text("Daemon stopped").foregroundStyle(.secondary)
                Button("Start Daemon") { Task { await controller.connect() } }
            }
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
                List(selection: $controller.selectedServiceId) {
                    // Pinned above the services: the daemon's own `daemon.log`, same panel.
                    Label("daemon log", systemImage: "terminal")
                        .tag(LogController.daemonServiceId)
                    ForEach(controller.services) { service in
                        ServiceRow(
                            service: service,
                            label: controller.catalog?.services.first(where: { $0.id == service.serviceId })?.displayName ?? service.serviceId,
                            busy: controller.actionsInFlight.contains(service.serviceId),
                            urls: controller.urls(for: service.serviceId),
                            onAction: { action, killUnowned in
                                Task { await controller.perform(action, serviceId: service.serviceId, killUnowned: killUnowned) }
                            }
                        )
                        .tag(service.serviceId)
                    }
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
            let label = selectedServiceId == LogController.daemonServiceId
                ? "daemon log"
                : controller.catalog?.services.first(where: { $0.id == selectedServiceId })?.displayName ?? selectedServiceId
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
    let onAction: (ManagerAction, Bool) -> Void
    @State private var confirmReclaim = false

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
                    // `externally-owned` means a process this daemon does not own holds the service's
                    // port — the daemon's own reason ("Port 1166 is held by an unowned process") is
                    // the only thing that explains why Stop cannot work here, so the row shows it
                    // rather than leaving a button that looks broken.
                    if let reason = service.error, service.displayState == "external" {
                        Text(reason).font(.caption).foregroundStyle(.orange).lineLimit(1)
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
                    Button("Stop") { onAction(.stop, false) }
                }
            } else {
                switch service.displayState {
                case "stopped", "failed":
                    Button("Start") { onAction(.start, false) }
                // `orphaned`: the daemon no longer owns the process it recorded (a reused pid, or a
                // program that replaced it). Start re-runs the catalog's run command; Stop runs its
                // `stop:` command — or reports why it cannot, which is the only honest answer for a
                // process this daemon does not own.
                case "orphaned":
                    Button("Start") { onAction(.start, false) }
                    Button("Stop") { onAction(.stop, false) }
                case "ready", "starting":
                    Button("Restart") { onAction(.restart, false) }
                    Button("Stop") { onAction(.stop, false) }
                // A service queued behind another operation on the same target stays `queued-start`
                // until something clears it — without this the row offered no way out of it at all.
                case "queued":
                    Button("Cancel") { onAction(.stop, false) }
                // `externally-owned`: a process this daemon does not own holds the service's port, so
                // it has no process to kill. "Kill & Start" asks the daemon to terminate that
                // process (SIGTERM, then SIGKILL) and continue the start — destructive, so it is
                // gated behind a confirmation. Stop is still offered because the catalog's `stop:`
                // command is the other lever that works here.
                case "external":
                    Button("Kill & Start") { confirmReclaim = true }
                        .confirmationDialog(
                            "Kill the process holding this port?",
                            isPresented: $confirmReclaim,
                            titleVisibility: .visible
                        ) {
                            Button("Kill & Start", role: .destructive) { onAction(.start, true) }
                            Button("Cancel", role: .cancel) {}
                        } message: {
                            Text(service.error ?? "A process this manager does not own holds the port.")
                        }
                    Button("Stop") { onAction(.stop, false) }
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
