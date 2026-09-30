import AppKit
import SwiftUI

struct WorkspaceDetailView: View {
    @EnvironmentObject private var workspaceStore: WorkspaceStore
    @Environment(\.openWindow) private var openWindow
    /// Resolved by the caller (`ContentView`) from the shared `WorkspaceControllerRegistry` — this
    /// view never creates its own; the registry owns the connection's lifetime so it survives the
    /// user switching away and back, and so the menu bar sees the same live state.
    @ObservedObject var controller: WorkspaceController
    let workspace: Workspace
    @State private var confirmStopDaemon = false
    @State private var confirmRemoveWorkspace = false

    var body: some View {
        Group {
            if !workspace.trusted {
                TrustPromptView(workspace: workspace) {
                    workspaceStore.setTrusted(true, id: workspace.id)
                    Task { await controller.connect() }
                }
                .transition(.opacity)
            } else {
                connectedBody
                    .transition(.opacity)
            }
        }
        .navigationTitle(workspace.displayName)
        .modifier(DetailSubtitle(text: workspace.displayPath))
        .toolbar { toolbar }
        .animation(Motion.layout, value: workspace.trusted)
        .animation(Motion.layout, value: phaseKey)
        .task(id: workspace.id) {
            if workspace.trusted, controller.phase == .idle { await controller.connect() }
        }
        .onChange(of: workspace.id) { _ in
            confirmStopDaemon = false
            confirmRemoveWorkspace = false
        }
    }

    /// Coarse phase so a new failure message does not replay the transition.
    private var phaseKey: String {
        switch controller.phase {
        case .idle, .connecting: return "starting"
        case .failed: return "failed"
        case .stopped: return "stopped"
        case .connected: return "connected"
        }
    }

    /// Two labeled actions in the unified toolbar, matching ns-adeck's single primary control
    /// plus the rest behind a menu. Destructive items stay behind a second click.
    @ToolbarContentBuilder
    private var toolbar: some ToolbarContent {
        if workspace.trusted, controller.phase == .connected {
            ToolbarItem(id: "stopAll", placement: .primaryAction) {
                ToolbarButton(title: "Stop All", help: "Stop All Services", systemImage: "stop.circle") {
                    Task { await controller.stopAll() }
                }
            }
        }
        // The daemon lifecycle item stays visible in every phase but `connecting` — a failed
        // workspace is exactly where a daemon swap is worth trying (the controller guards
        // re-entry, so a double click cannot stack two swaps).
        if workspace.trusted, controller.phase != .connecting {
            ToolbarItem(id: "restartDaemon", placement: .primaryAction) {
                ToolbarButton(title: "Restart", help: "Restart Daemon", systemImage: "arrow.triangle.2.circlepath") {
                    Task { await controller.restartDaemon() }
                }
                .disabled(controller.daemonTransitionInFlight)
            }
        }
        ToolbarItem(id: "workspaceMenu", placement: .primaryAction) {
            Menu {
                Button {
                    openWindow(id: SharedWindow.id)
                } label: {
                    Label("Shared Services…", systemImage: "shippingbox")
                }
                Button {
                    NSWorkspace.shared.activateFileViewerSelecting([URL(fileURLWithPath: workspace.path)])
                } label: {
                    Label("Reveal in Finder", systemImage: "folder")
                }
                Button {
                    NSWorkspace.shared.open(AppLinks.releases)
                } label: {
                    Label("Check for Updates…", systemImage: "arrow.down.circle")
                }
                Divider()
                // `stopDaemon` takes the daemon AND all its services down — destructive
                // enough to confirm, and only offered while the daemon may be alive
                // (connected, or failed with a possibly-wedged daemon still running).
                if workspace.trusted, controller.phase != .connecting, controller.phase.mayHaveLiveDaemon {
                    Button(role: .destructive) { confirmStopDaemon = true } label: {
                        Label("Stop Daemon…", systemImage: "power")
                    }
                    .disabled(controller.daemonTransitionInFlight)
                }
                Button(role: .destructive) { confirmRemoveWorkspace = true } label: {
                    Label("Remove Workspace", systemImage: "trash")
                }
            } label: {
                Label {
                    Text("Actions")
                } icon: {
                    Image(systemName: "ellipsis.circle")
                }
            }
            .help("Workspace actions")
            .stopDaemonConfirmation(isPresented: $confirmStopDaemon) {
                Task { await controller.stopDaemon() }
            }
            .alert("Remove \"\(workspace.displayName)\"?", isPresented: $confirmRemoveWorkspace) {
                Button("Remove", role: .destructive) { workspaceStore.remove(id: workspace.id) }
            } message: {
                Text("Hearth will stop showing this folder. The folder stays on disk.")
            }
        }
    }

    @ViewBuilder
    private var connectedBody: some View {
        switch controller.phase {
        case .idle, .connecting:
            VStack(spacing: 10) {
                ProgressView().controlSize(.large)
                Text("Starting daemon…").font(.callout).foregroundStyle(.secondary)
            }
            .frame(maxWidth: .infinity, maxHeight: .infinity)
            .transition(.opacity)
        case .failed(let message):
            ContentUnavailableViewCompat(
                title: "Daemon failed to start",
                message: message,
                systemImage: "exclamationmark.triangle.fill",
                symbolColor: .orange
            ) {
                Button("Retry") { Task { await controller.connect() } }
                    .buttonStyle(.borderedProminent)
            }
            .transition(.opacity)
        case .stopped:
            ContentUnavailableViewCompat(
                title: "Daemon stopped",
                message: "Services are down. Start the daemon to bring the workspace back.",
                systemImage: "stop.circle"
            ) {
                Button("Start Daemon") { Task { await controller.connect() } }
                    .buttonStyle(.borderedProminent)
            }
            .transition(.opacity)
        case .connected:
            ServiceListView(controller: controller)
                .transition(.identity)
        }
    }
}

private struct TrustPromptView: View {
    let workspace: Workspace
    let onTrust: () -> Void

    var body: some View {
        ContentUnavailableViewCompat(
            title: "Trust this folder?",
            message: "\(workspace.path)\n\nThis folder's hearth.yaml names commands the daemon will run on your behalf. Only trust folders you wrote or downloaded from somewhere you trust.",
            systemImage: "shield.lefthalf.filled",
            symbolColor: .accentColor
        ) {
            Button("Trust and Start", action: onTrust)
                .buttonStyle(.borderedProminent)
        }
    }
}

/// A master-detail split: the service list on the left drives a live log panel on the right for
/// whichever row is focused/selected. Selection and log tails live on `WorkspaceController`.
/// The log text view stays mounted across row and workspace switches. `LogTextView` retargets it.
private struct ServiceListView: View {
    @ObservedObject var controller: WorkspaceController

    var body: some View {
        HSplitView {
            VStack(spacing: 0) {
                if let error = controller.lastActionError {
                    ErrorBanner(text: error) { controller.lastActionError = nil }
                }
                List(selection: $controller.selectedServiceId) {
                    // Pinned above the services: the daemon's own `daemon.log`, same panel.
                    Section("Daemon") {
                        Label("daemon log", systemImage: "terminal")
                            .foregroundStyle(.secondary)
                            .tag(LogController.daemonServiceId)
                    }
                    // Services render in their declared `groups:` order — each service sits in the
                    // first group listing it directly; leftovers form a trailing "Services" section.
                    let sections = controller.catalog?.serviceSections(serviceOrder: controller.services.map(\.serviceId))
                        ?? [(name: nil as String?, serviceIds: controller.services.map(\.serviceId))]
                    if controller.services.isEmpty {
                        Section("Services") {
                            Text("No services — add service blocks to hearth.yaml.")
                                .font(.callout).foregroundStyle(.secondary)
                        }
                    } else {
                        ForEach(Array(sections.enumerated()), id: \.offset) { _, section in
                            Section {
                                ForEach(section.serviceIds.compactMap { controller.service($0) }) { service in
                                    ServiceRow(
                                        workspaceID: controller.workspace.id,
                                        service: service,
                                        label: controller.displayName(for: service.serviceId),
                                        busy: controller.actionsInFlight.contains(service.serviceId),
                                        disabled: controller.isDisabled(service.serviceId),
                                        finite: controller.isFinite(service.serviceId),
                                        urls: controller.urls(for: service.serviceId),
                                        onAction: { action, killUnowned in
                                            Task { await controller.perform(action, serviceId: service.serviceId, killUnowned: killUnowned) }
                                        }
                                    )
                                    .equatable()
                                    .tag(service.serviceId)
                                }
                            } header: {
                                if let name = section.name {
                                    // Group actions expand past `disabled: true` members — the
                                    // daemon would skip them in a bulk start anyway.
                                    let enabledIds = section.serviceIds.filter { !controller.isDisabled($0) }
                                    // Finite jobs don't count as "up". A group of only those stays
                                    // on Start so it can be run again; start/stop still include them.
                                    let longLivedIds = enabledIds.filter { !controller.isFinite($0) }
                                    let allStarted = !longLivedIds.isEmpty && longLivedIds.allSatisfy { controller.service($0)?.isUp ?? false }
                                    if enabledIds.isEmpty {
                                        Text(name)
                                    } else {
                                        GroupSectionHeader(
                                            name: name,
                                            busy: enabledIds.contains { controller.actionsInFlight.contains($0) },
                                            allStarted: allStarted,
                                            onStart: { Task { await controller.startGroup(enabledIds) } },
                                            onRestart: { Task { await controller.restartGroup(enabledIds) } },
                                            onStop: { Task { await controller.stopGroup(enabledIds) } }
                                        )
                                    }
                                } else {
                                    Text(sections.count > 1 ? "Other services" : "Services")
                                }
                            }
                        }
                    }
                }
                .listStyle(.inset)
            }
            .frame(minWidth: 220, idealWidth: 240, maxWidth: 320)

            LogPane(controller: controller, selection: controller.selection)
                .frame(minWidth: 360, maxWidth: .infinity, maxHeight: .infinity)
        }
    }
}

/// Observes selection on its own, so a row click does not rebuild the service list.
private struct LogPane: View {
    @ObservedObject var controller: WorkspaceController
    @ObservedObject var selection: ServiceSelection

    var body: some View {
        Group {
            if let selectedServiceId = selection.serviceId, let logController = controller.logController(for: selectedServiceId) {
                let service = controller.service(selectedServiceId)
                let label = selectedServiceId == LogController.daemonServiceId
                    ? "daemon log"
                    : controller.displayName(for: selectedServiceId)
                ServiceLogPanel(serviceLabel: label, state: service?.displayState, log: logController)
                    .transition(.identity)
            } else {
                ContentUnavailableViewCompat(
                    title: "No selection",
                    message: "Select a service to view its logs.",
                    systemImage: "doc.plaintext"
                )
                .transition(.opacity)
            }
        }
        // Only the empty ↔ log swap fades. Switching rows keeps the text view and skips the animation.
        .animation(Motion.layout, value: selection.serviceId == nil)
    }
}

/// Section header for one `groups:` entry — name plus Start/Stop acting on the group's members.
/// Same button footprint as `ServiceRow`'s actions so the two action areas line up.
private struct GroupSectionHeader: View {
    let name: String
    let busy: Bool
    /// Every enabled member is `ready` — Start becomes Restart, mirroring a service row's state.
    let allStarted: Bool
    let onStart: () -> Void
    let onRestart: () -> Void
    let onStop: () -> Void

    var body: some View {
        HStack {
            Text(name)
            Spacer()
            HStack(spacing: 8) {
                if busy {
                    ActionSpinner()
                    IconActionButton("Stop all \(name) services", systemImage: "stop.fill", action: onStop)
                } else {
                    if allStarted {
                        IconActionButton("Restart all \(name) services", systemImage: "arrow.clockwise", action: onRestart)
                    } else {
                        IconActionButton("Start all \(name) services", systemImage: "play.fill", action: onStart)
                    }
                    IconActionButton("Stop all \(name) services", systemImage: "stop.fill", action: onStop)
                }
            }
        }
    }
}

private struct ServiceRow: View, Equatable {
    let workspaceID: UUID
    let service: ServiceLifecycleState
    let label: String
    let busy: Bool
    let disabled: Bool
    /// `readiness: exit` — the primary button says Run, including before the first start.
    let finite: Bool
    let urls: [ResolvedServiceUrl]
    let onAction: (ManagerAction, Bool) -> Void
    @State private var confirmReclaim = false

    static func == (lhs: ServiceRow, rhs: ServiceRow) -> Bool {
        lhs.workspaceID == rhs.workspaceID
            && lhs.service.isVisuallyEqual(to: rhs.service)
            && lhs.label == rhs.label
            && lhs.busy == rhs.busy
            && lhs.disabled == rhs.disabled
            && lhs.finite == rhs.finite
            && lhs.urls == rhs.urls
    }

    /// Same rule as `hearthd urls` and both TUIs: in-flight states count as running.
    private var isRunning: Bool { ["ready", "running", "starting"].contains(service.displayState) }

    var body: some View {
        HStack(alignment: .top, spacing: 8) {
            StatusDot(state: service.displayState)
                .padding(.top, 4)
            VStack(alignment: .leading, spacing: 2) {
                Text(label).fontWeight(.medium)
                    .foregroundStyle(disabled ? .secondary : .primary)
                metadata
                if !urls.isEmpty {
                    links
                }
            }
            Spacer()
            actions
        }
        .padding(.vertical, 2)
        .contentShape(Rectangle()) // the whole row is clickable/selectable, not just the text
        .onChange(of: workspaceID) { _ in confirmReclaim = false }
    }

    /// State, then the facts that qualify it, in one secondary line — the same shape as a Finder
    /// list subtitle. The state keeps its status color; everything else is secondary except a
    /// failure reason.
    private var metadata: some View {
        HStack(spacing: 0) {
            Text(service.displayState)
                .foregroundStyle(StatusStyle.color(for: service.displayState))
                .layoutPriority(1)
            if disabled {
                Text(" · disabled").foregroundStyle(.secondary)
            }
            if let pid = service.identity?.pid {
                Text(" · pid \(pid)").foregroundStyle(.secondary)
            }
            if let detail = service.readinessDetail, service.displayState == "failed" {
                Text(" · \(detail)")
                    .foregroundStyle(.red)
                    .lineLimit(1)
                    .truncationMode(.tail)
            }
            // `externally-owned` means a process this daemon does not own holds the service's
            // port — the daemon's own reason ("Port 1166 is held by an unowned process") is
            // the only thing that explains why Stop cannot work here, so the row shows it
            // rather than leaving a button that looks broken.
            if let reason = service.error, service.displayState == "external" {
                Text(" · \(reason)")
                    .foregroundStyle(.orange)
                    .lineLimit(1)
                    .truncationMode(.tail)
            }
        }
        .font(.caption)
        .lineLimit(1)
    }

    /// One link per registered URL. A URL that needs the service running is dimmed while it is not,
    /// so a dead link is recognisable before anyone clicks it — still clickable, since the catalog
    /// can't know for certain (another process may be serving it).
    private var links: some View {
        HStack(spacing: 10) {
            ForEach(urls, id: \.self) { entry in
                if let destination = URL(string: entry.url) {
                    let stale = entry.requiresRunning && !isRunning
                    Link(entry.displayName, destination: destination)
                        .font(.caption)
                        .opacity(stale ? 0.45 : 1)
                        .help(stale ? "\(entry.url) — \(label) is not running" : entry.url)
                }
            }
        }
    }

    @ViewBuilder
    private var actions: some View {
        // `disabled: true` in the catalog means no lifecycle action is valid for this row —
        // offering buttons here only produced daemon-side `service_disabled` rejections.
        if disabled {
            EmptyView()
        } else {
            HStack(spacing: 8) {
                if busy {
                    ActionSpinner()
                }
                if busy {
                    // Keep Stop available for the whole start/restart flight so the user can abort
                    // without waiting for readiness.
                    if service.displayState != "stopping" {
                        IconActionButton("Stop", systemImage: "stop.fill") { onAction(.stop, false) }
                    }
                } else {
                    switch service.displayState {
                    // `orphaned`: the daemon no longer owns the process it recorded (a reused pid, or a
                    // program that replaced it). Start re-runs the catalog's run command; Stop runs its
                    // `stop:` command — or reports why it cannot, which is the only honest answer for a
                    // process this daemon does not own.
                    case "orphaned":
                        IconActionButton("Start", systemImage: "play.fill") { onAction(.start, false) }
                        IconActionButton("Stop", systemImage: "stop.fill") { onAction(.stop, false) }
                    // `externally-owned`: a process this daemon does not own holds the service's port, so
                    // it has no process to kill. "Kill & Start" asks the daemon to terminate that
                    // process (SIGTERM, then SIGKILL) and continue the start — destructive, so it is
                    // gated behind a confirmation and rendered red. Stop is still offered because the
                    // catalog's `stop:` command is the other lever that works here.
                    case "external":
                        IconActionButton("Kill the process holding the port, then start", systemImage: "bolt.fill", role: .destructive) {
                            confirmReclaim = true
                        }
                        IconActionButton("Stop", systemImage: "stop.fill") { onAction(.stop, false) }
                    default:
                        LifecycleButtons(state: service.displayState, finite: finite) { onAction($0, false) }
                    }
                }
            }
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
        }
    }
}
