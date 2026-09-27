import AppKit
import SwiftUI

/// One top-bar action — rendered as a toolbar button while it fits, collapsing into the trailing
/// menu when the window narrows. Suffix order is collapse order: the least-used actions move into
/// the menu first.
private struct TopBarAction: Identifiable {
    let id: String
    let title: String
    let systemImage: String
    var disabled = false
    let run: () -> Void
}

/// Width of the detail pane, measured by a `GeometryReader` in the view's background — the toolbar
/// itself proposes each item its ideal width and clips the overflow, so the items have to decide
/// how many of them fit from the pane's width instead.
private struct DetailWidthKey: PreferenceKey {
    static let defaultValue: CGFloat = .infinity
    static func reduce(value: inout CGFloat, nextValue: () -> CGFloat) { value = nextValue() }
}

struct WorkspaceDetailView: View {
    @EnvironmentObject private var workspaceStore: WorkspaceStore
    @Environment(\.openWindow) private var openWindow
    /// Resolved by the caller (`ContentView`) from the shared `WorkspaceControllerRegistry` — this
    /// view never creates its own; the registry owns the connection's lifetime so it survives the
    /// user switching away and back, and so the menu bar sees the same live state.
    @ObservedObject var controller: WorkspaceController
    let workspace: Workspace
    @State private var confirmStopDaemon = false
    @State private var detailWidth: CGFloat = .infinity

    /// Top-bar actions in priority order — the leftmost stays on the bar longest. Destructive
    /// items (`Stop Daemon…`, `Remove Workspace`) stay pinned to the menu: they were behind a
    /// second click before, and a one-click toolbar button should not get cheaper than that.
    private var topBarActions: [TopBarAction] {
        var actions: [TopBarAction] = []
        if workspace.trusted, controller.phase == .connected {
            actions.append(TopBarAction(id: "stopAll", title: "Stop All Services", systemImage: "stop.circle") { Task { await controller.stopAll() } })
        }
        // The daemon lifecycle items stay visible in every phase but `connecting` — a failed
        // workspace is exactly where a daemon swap is worth trying (the controller guards
        // re-entry, so a double click cannot stack two swaps).
        if workspace.trusted, controller.phase != .connecting {
            actions.append(TopBarAction(id: "restartDaemon", title: "Restart Daemon", systemImage: "arrow.triangle.2.circlepath", disabled: controller.daemonTransitionInFlight) { Task { await controller.restartDaemon() } })
        }
        actions.append(TopBarAction(id: "shared", title: "Shared Services…", systemImage: "shippingbox") { openWindow(id: SharedWindow.id) })
        actions.append(TopBarAction(id: "reveal", title: "Reveal in Finder", systemImage: "folder") {
            NSWorkspace.shared.activateFileViewerSelecting([URL(fileURLWithPath: workspace.path)])
        })
        actions.append(TopBarAction(id: "updates", title: "Check for Updates…", systemImage: "arrow.down.circle") {
            if let url = URL(string: "https://github.com/gnasdev/hearth/releases") {
                NSWorkspace.shared.open(url)
            }
        })
        return actions
    }

    /// Title+icon toolbar buttons size to their label — estimate ~7.5pt per character plus the
    /// icon and padding. Reserve room for the window title and the sidebar toggle, and for the
    /// overflow menu itself while it still holds collapsed items.
    private var visibleTopBarCount: Int {
        let items = topBarActions
        let budget = detailWidth - 300
        func width(_ item: TopBarAction) -> CGFloat { CGFloat(item.title.count) * 7.5 + 44 }
        if items.reduce(0, { $0 + width($1) }) <= budget { return items.count }
        var used: CGFloat = 44
        var count = 0
        for item in items {
            used += width(item)
            if used > budget { break }
            count += 1
        }
        return count
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
        .background(GeometryReader { geo in Color.clear.preference(key: DetailWidthKey.self, value: geo.size.width) })
        .onPreferenceChange(DetailWidthKey.self) { detailWidth = $0 }
        .navigationTitle(workspace.displayName)
        .toolbar {
            ToolbarItemGroup(placement: .primaryAction) {
                let actions = topBarActions
                let visible = visibleTopBarCount
                ForEach(actions.prefix(visible)) { action in
                    Button(action: action.run) {
                        Label(action.title, systemImage: action.systemImage)
                            .labelStyle(.titleAndIcon)
                    }
                    .disabled(action.disabled)
                    .help(action.title)
                }
                Menu {
                    ForEach(actions.dropFirst(visible)) { action in
                        Button(action: action.run) {
                            Label(action.title, systemImage: action.systemImage)
                        }
                        .disabled(action.disabled)
                    }
                    if visible < actions.count {
                        Divider()
                    }
                    // `stopDaemon` takes the daemon AND all its services down — destructive
                    // enough to confirm, and only offered while the daemon may be alive
                    // (connected, or failed with a possibly-wedged daemon still running).
                    if workspace.trusted, controller.phase != .connecting, controller.phase.mayHaveLiveDaemon {
                        Button(role: .destructive) { confirmStopDaemon = true } label: {
                            Label("Stop Daemon…", systemImage: "power")
                        }
                        .disabled(controller.daemonTransitionInFlight)
                    }
                    Divider()
                    Button(role: .destructive) { workspaceStore.remove(id: workspace.id) } label: {
                        Label("Remove Workspace", systemImage: "trash")
                    }
                } label: {
                    Label("Workspace actions", systemImage: "ellipsis.circle")
                        .labelStyle(.iconOnly)
                }
                .help("Workspace actions")
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
        .task {
            if workspace.trusted, controller.phase == .idle { await controller.connect() }
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
        case .failed(let message):
            VStack(spacing: 12) {
                Image(systemName: "exclamationmark.triangle.fill")
                    .font(.system(size: 40)).foregroundStyle(.orange)
                Text("Daemon failed to start").font(.title3).fontWeight(.medium)
                Text(message).multilineTextAlignment(.center).foregroundStyle(.secondary)
                    .textSelection(.enabled).frame(maxWidth: 420)
                Button("Retry") { Task { await controller.connect() } }.buttonStyle(.borderedProminent)
            }
            .padding()
            .frame(maxWidth: .infinity, maxHeight: .infinity)
        case .stopped:
            VStack(spacing: 12) {
                Image(systemName: "stop.circle").font(.system(size: 40)).foregroundStyle(.secondary)
                Text("Daemon stopped").font(.title3).fontWeight(.medium)
                Text("Services are down. Start the daemon to bring the workspace back.")
                    .font(.callout).foregroundStyle(.secondary)
                Button("Start Daemon") { Task { await controller.connect() } }.buttonStyle(.borderedProminent)
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
            Image(systemName: "shield.lefthalf.filled").font(.system(size: 44)).foregroundStyle(Color.accentColor)
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
                                ForEach(controller.services.filter { section.serviceIds.contains($0.serviceId) }) { service in
                                    ServiceRow(
                                        service: service,
                                        label: controller.catalog?.services.first(where: { $0.id == service.serviceId })?.displayName ?? service.serviceId,
                                        busy: controller.actionsInFlight.contains(service.serviceId),
                                        disabled: controller.isDisabled(service.serviceId),
                                        urls: controller.urls(for: service.serviceId),
                                        onAction: { action, killUnowned in
                                            Task { await controller.perform(action, serviceId: service.serviceId, killUnowned: killUnowned) }
                                        }
                                    )
                                    .tag(service.serviceId)
                                }
                            } header: {
                                if let name = section.name {
                                    // Group actions expand past `disabled: true` members — the
                                    // daemon would skip them in a bulk start anyway.
                                    let enabledIds = section.serviceIds.filter { !controller.isDisabled($0) }
                                    let allStarted = !enabledIds.isEmpty && enabledIds.allSatisfy { id in
                                        controller.services.first { $0.serviceId == id }?.displayState == "ready"
                                    }
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

            logPanel
                .frame(minWidth: 360, maxWidth: .infinity, maxHeight: .infinity)
        }
    }

    @ViewBuilder
    private var logPanel: some View {
        if let selectedServiceId = controller.selectedServiceId, let logController = controller.logController(for: selectedServiceId) {
            let service = controller.services.first(where: { $0.serviceId == selectedServiceId })
            let label = selectedServiceId == LogController.daemonServiceId
                ? "daemon log"
                : controller.catalog?.services.first(where: { $0.id == selectedServiceId })?.displayName ?? selectedServiceId
            ServiceLogPanel(serviceLabel: label, state: service?.displayState, log: logController)
                .id(selectedServiceId)
        } else {
            ContentUnavailableViewCompat(
                title: "No selection",
                message: "Select a service to view its logs.",
                systemImage: "doc.plaintext"
            )
        }
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

private struct ServiceRow: View {
    let service: ServiceLifecycleState
    let label: String
    let busy: Bool
    let disabled: Bool
    let urls: [ResolvedServiceUrl]
    let onAction: (ManagerAction, Bool) -> Void
    @State private var confirmReclaim = false

    /// Same rule as `hearthd urls` and both TUIs: in-flight states count as running.
    private var isRunning: Bool { ["ready", "starting"].contains(service.displayState) }

    var body: some View {
        HStack(alignment: .top, spacing: 8) {
            StatusDot(state: service.displayState)
                .padding(.top, 4)
            VStack(alignment: .leading, spacing: 2) {
                Text(label).fontWeight(.medium)
                    .foregroundStyle(disabled ? .secondary : .primary)
                HStack(spacing: 6) {
                    Text(service.displayState)
                        .font(.caption)
                        .foregroundStyle(StatusStyle.color(for: service.displayState))
                    if disabled {
                        Text("disabled")
                            .font(.caption)
                            .foregroundStyle(.secondary)
                            .padding(.horizontal, 6)
                            .padding(.vertical, 1)
                            .background(.quaternary, in: Capsule())
                    }
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
                            .padding(.horizontal, 6)
                            .padding(.vertical, 1)
                            .background(.quaternary, in: Capsule())
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
                case "stopped", "failed":
                    IconActionButton("Start", systemImage: "play.fill") { onAction(.start, false) }
                // `orphaned`: the daemon no longer owns the process it recorded (a reused pid, or a
                // program that replaced it). Start re-runs the catalog's run command; Stop runs its
                // `stop:` command — or reports why it cannot, which is the only honest answer for a
                // process this daemon does not own.
                case "orphaned":
                    IconActionButton("Start", systemImage: "play.fill") { onAction(.start, false) }
                    IconActionButton("Stop", systemImage: "stop.fill") { onAction(.stop, false) }
                case "ready", "starting":
                    IconActionButton("Restart", systemImage: "arrow.clockwise") { onAction(.restart, false) }
                    IconActionButton("Stop", systemImage: "stop.fill") { onAction(.stop, false) }
                // A service queued behind another operation on the same target stays `queued-start`
                // until something clears it — without this the row offered no way out of it at all.
                case "queued":
                    IconActionButton("Cancel start", systemImage: "xmark") { onAction(.stop, false) }
                // `externally-owned`: a process this daemon does not own holds the service's port, so
                // it has no process to kill. "Kill & Start" asks the daemon to terminate that
                // process (SIGTERM, then SIGKILL) and continue the start — destructive, so it is
                // gated behind a confirmation and rendered red. Stop is still offered because the
                // catalog's `stop:` command is the other lever that works here.
                case "external":
                    Button(role: .destructive) { confirmReclaim = true } label: {
                        Label("Kill the process holding the port, then start", systemImage: "bolt.fill")
                            .labelStyle(.iconOnly)
                    }
                    .buttonStyle(.borderless)
                    .help("Kill the process holding the port, then start")
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
                    IconActionButton("Stop", systemImage: "stop.fill") { onAction(.stop, false) }
                // `stopping` is genuinely in-flight — no extra action, the poll will move it.
                default:
                    EmptyView()
                }
            }
        }
        }
    }
}
