import AppKit
import SwiftUI

/// The machine-global shared services window — a catalog browser over smp: which `name@version`
/// instances this machine has (with their live state and project attachments) plus which services
/// the remote registry offers for install. Lives in its own `Window` scene (`HearthApp`), opened
/// from the menu bar; smp is not per-workspace so it deliberately is not part of `ContentView`'s
/// workspace navigation.
struct SharedCatalogView: View {
    @ObservedObject var controller: SharedServicesController
    /// The instance id whose row armed the remove confirmation — destructive enough (deletes the
    /// install and its data dir) that it never fires on a bare click.
    @State private var removeTarget: String?
    /// Instance id whose tail this view started. Switching rows retargets the text view in place.
    @State private var trackedLogId: String?

    var body: some View {
        Group {
            switch controller.phase {
            case .idle, .connecting:
                ProgressView("Starting shared daemon…")
                    .frame(maxWidth: .infinity, maxHeight: .infinity)
                    .transition(.opacity)
            case .failed(let message):
                ContentUnavailableViewCompat(
                    title: "Shared services unavailable",
                    message: message,
                    systemImage: "exclamationmark.triangle.fill",
                    symbolColor: .orange
                ) {
                    Button("Retry") { Task { await controller.connect() } }
                        .buttonStyle(.borderedProminent)
                }
                .transition(.opacity)
            case .connected:
                catalogBody
                    .transition(.identity)
            }
        }
        .toolbar {
            if controller.phase == .connected {
                ToolbarItem(id: "refresh", placement: .primaryAction) {
                    ToolbarButton(title: "Refresh", systemImage: "arrow.clockwise") {
                        Task { await controller.refresh(); await controller.refreshCatalog() }
                    }
                }
            }
        }
        .animation(Motion.layout, value: phaseKey)
        .task {
            if controller.phase == .idle { await controller.connect() }
        }
        .onAppear { trackedLogId = controller.selectedId }
        .onChange(of: controller.selectedId) { handoffLog(to: $0) }
        .confirmationDialog(
            "Remove this shared service?",
            isPresented: Binding(get: { removeTarget != nil }, set: { if !$0 { removeTarget = nil } }),
            titleVisibility: .visible
        ) {
            if let id = removeTarget {
                let attachments = controller.instance(id)?.attachments.count ?? 0
                Button("Remove \(id)", role: .destructive) { controller.remove(id, force: attachments > 0) }
            }
            Button("Cancel", role: .cancel) {}
        } message: {
            let attachments = removeTarget.flatMap { controller.instance($0)?.attachments.count } ?? 0
            Text(attachments > 0
                ? "The service is stopped and its install and data under ~/.hearth/shared are deleted. \(attachments) attached project\(attachments == 1 ? "" : "s") will lose their connection."
                : "The service is stopped and its install and data under ~/.hearth/shared are deleted.")
        }
    }

    // MARK: - Catalog browser

    private var catalogBody: some View {
        NavigationSplitView {
            List(selection: $controller.selectedId) {
                if !controller.instances.isEmpty {
                    Section("Installed") {
                        ForEach(controller.instances) { instance in
                            SharedInstanceRow(instance: instance, busy: controller.actionsInFlight.contains(instance.id))
                                .tag(instance.id)
                        }
                    }
                }
                if let services = controller.catalogDoc?.services, !services.isEmpty {
                    Section("Available") {
                        ForEach(availableRows(services), id: \.self) { row in
                            SharedCatalogRow(name: row.name, version: row.version, installed: row.installed, busy: controller.actionsInFlight.contains(row.id))
                                .tag(row.id)
                        }
                    }
                }
                if let error = controller.catalogError {
                    Section {
                        Label("Registry unavailable", systemImage: "exclamationmark.triangle")
                            .help(error)
                    }
                }
            }
            .listStyle(.sidebar)
            .navigationTitle("Catalog")
        } detail: {
            detailPane
                .animation(Motion.layout, value: detailKind)
                .navigationTitle(detailTitle)
                .modifier(DetailSubtitle(text: detailSubtitle))
        }
        .navigationSplitViewStyle(.balanced)
        .navigationSplitViewColumnWidth(min: 220, ideal: 260, max: 400)
    }

    private var detailTitle: String {
        if let instance = controller.instance(controller.selectedId) { return instance.id }
        if let entry = controller.catalogEntry(controller.selectedId) { return entry.name }
        return "Shared Services"
    }

    private var detailSubtitle: String {
        if let instance = controller.instance(controller.selectedId) {
            return "port \(instance.port)"
        }
        if let entry = controller.catalogEntry(controller.selectedId) { return entry.version }
        return ""
    }

    /// Every `(service, version)` in the registry, sorted for display, flagged when an instance
    /// with the same `name@version` already exists.
    private func availableRows(_ services: [String: SharedFamily]) -> [CatalogRow] {
        services.keys.sorted().flatMap { name in
            (services[name]?.versions.keys.sorted() ?? []).map { version in
                CatalogRow(id: "\(name)@\(version)", name: name, version: version, installed: controller.instances.contains { $0.id == "\(name)@\(version)" })
            }
        }
    }

    @ViewBuilder
    private var detailPane: some View {
        if let instance = controller.instance(controller.selectedId) {
            SharedInstanceDetail(
                instance: instance,
                finite: controller.catalogDoc?.services[instance.name]?.versions[instance.version]?.isFinite ?? false,
                busy: controller.actionsInFlight.contains(instance.id),
                log: controller.logController(for: instance.id),
                onAction: { controller.perform($0, instance.id) },
                onRemove: { removeTarget = instance.id }
            )
            .transition(.identity)
        } else if let entry = controller.catalogEntry(controller.selectedId) {
            let id = "\(entry.name)@\(entry.version)"
            ContentUnavailableViewCompat(
                title: "\(entry.name) \(entry.version)",
                message: "Not installed. The first project that needs it will install it, or install it now.",
                systemImage: "shippingbox"
            ) {
                Button("Install \(id)") { controller.install(id) }
                    .buttonStyle(.borderedProminent)
                    .disabled(controller.actionsInFlight.contains(id))
            }
            .transition(.opacity)
        } else if controller.instances.isEmpty && (controller.catalogDoc?.services.isEmpty ?? true) {
            ContentUnavailableViewCompat(
                title: controller.catalogError != nil ? "Registry unavailable" : "Empty catalog",
                message: controller.catalogError ?? "The shared-services registry lists nothing this machine can install.",
                systemImage: "shippingbox"
            )
            .transition(.opacity)
        } else {
            ContentUnavailableViewCompat(
                title: "Select a shared service",
                message: "Choose an installed instance or a service from the catalog.",
                systemImage: "server.rack"
            )
            .transition(.opacity)
        }
    }

    /// Stays `"instance"` across installed-row switches so the log text view is retargeted, not rebuilt.
    private var detailKind: String {
        if controller.instance(controller.selectedId) != nil { return "instance" }
        if controller.catalogEntry(controller.selectedId) != nil { return "entry" }
        if controller.instances.isEmpty && (controller.catalogDoc?.services.isEmpty ?? true) { return "empty" }
        return "select"
    }

    private var phaseKey: String {
        switch controller.phase {
        case .idle, .connecting: return "starting"
        case .failed: return "failed"
        case .connected: return "connected"
        }
    }

    /// Stops the tail being left and starts the one being shown. Catalog rows that are not installed have no log.
    private func handoffLog(to newId: String?) {
        if removeTarget != nil { removeTarget = nil }
        if let previous = trackedLogId, previous != newId {
            controller.cachedLogController(for: previous)?.stop()
        }
        trackedLogId = newId
        if let newId, controller.instance(newId) != nil {
            controller.logController(for: newId)?.start()
        }
    }

    private struct CatalogRow: Hashable {
        let id: String
        let name: String
        let version: String
        let installed: Bool
    }
}

private struct SharedInstanceRow: View {
    let instance: SharedInstance
    let busy: Bool

    var body: some View {
        HStack(spacing: 8) {
            Label {
                VStack(alignment: .leading, spacing: 2) {
                    Text(instance.id)
                        .lineLimit(1)
                    Text(statusLine)
                        .font(.caption)
                        .foregroundStyle(instance.installError != nil ? AnyShapeStyle(.red) : AnyShapeStyle(.secondary))
                        .lineLimit(1)
                }
            } icon: {
                Image(systemName: "server.rack")
                    .foregroundStyle(Color.accentColor)
            }
            Spacer(minLength: 0)
        }
        .help(help)
    }

    private var statusLine: String {
        if busy || instance.installState == "installing" { return "Installing…" }
        if let error = instance.installError { return error }
        let attached = instance.attachments.count
        if attached > 0 {
            return "\(instance.displayState) · \(attached) project\(attached == 1 ? "" : "s")"
        }
        return instance.displayState
    }

    private var help: String {
        var parts = ["port \(instance.port)", instance.displayState]
        if let error = instance.installError { parts.append(error) }
        return parts.joined(separator: " · ")
    }
}

private struct SharedCatalogRow: View {
    let name: String
    let version: String
    let installed: Bool
    let busy: Bool

    var body: some View {
        HStack(spacing: 8) {
            Label {
                VStack(alignment: .leading, spacing: 2) {
                    Text(name)
                        .lineLimit(1)
                    Text(busy ? "Installing…" : version)
                        .font(.caption)
                        .foregroundStyle(.secondary)
                        .lineLimit(1)
                        .truncationMode(.middle)
                }
            } icon: {
                Image(systemName: installed ? "shippingbox.fill" : "shippingbox")
                    .foregroundStyle(installed ? AnyShapeStyle(.secondary) : AnyShapeStyle(Color.accentColor))
            }
            Spacer(minLength: 0)
        }
        .help(installed ? "\(name) \(version) is already installed on this machine" : "\(name) \(version)")
    }
}

/// The detail pane for one registered instance: live state, which projects are attached (with their
/// rendered connection info, copyable), lifecycle actions, and the instance's log (install progress
/// and the service's own output both land there).
private struct SharedInstanceDetail: View {
    let instance: SharedInstance
    var finite: Bool = false
    let busy: Bool
    let log: LogController?
    let onAction: (ManagerAction) -> Void
    let onRemove: () -> Void

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            header
            Divider()
            attachments
            Divider()
            if let log {
                ServiceLogPanel(serviceLabel: instance.id, state: instance.displayState, log: log)
                    .frame(minHeight: 160)
            }
        }
    }

    private var header: some View {
        HStack {
            VStack(alignment: .leading, spacing: 4) {
                Text(instance.id).font(.title3.weight(.semibold))
                HStack(spacing: 0) {
                    Text(instance.displayState)
                        .foregroundStyle(StatusStyle.color(for: instance.displayState))
                    Text(" · port \(instance.port) · \(instance.installState)")
                        .foregroundStyle(.secondary)
                }
                .font(.subheadline)
                if let error = instance.installError {
                    Text(error).font(.callout).foregroundStyle(.red).textSelection(.enabled)
                }
            }
            Spacer()
            if busy { ActionSpinner() }
            // Icon-only like the service rows — the tooltip carries the name.
            LifecycleButtons(state: instance.displayState, finite: finite, onAction: onAction)
            Button("Remove…", role: .destructive, action: onRemove)
                .buttonStyle(.bordered)
                .controlSize(.small)
        }
        .padding(12)
    }

    @ViewBuilder
    private var attachments: some View {
        if instance.attachments.isEmpty {
            Text("No projects attached")
                .font(.callout)
                .foregroundStyle(.secondary)
                .frame(maxWidth: .infinity, alignment: .leading)
                .padding(12)
        } else {
            // A grouped form is the system key-value layout, and it hugs its rows so the log
            // below keeps the rest of the pane.
            Form {
                Section("Attached projects") {
                    ForEach(instance.attachments, id: \.projectId) { attachment in
                        AttachmentRow(attachment: attachment)
                    }
                }
            }
            .formStyle(.grouped)
            .scrollContentBackground(.hidden)
            .fixedSize(horizontal: false, vertical: true)
        }
    }
}

private struct AttachmentRow: View {
    let attachment: SharedAttachment

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            LabeledContent {
                HStack(spacing: 4) {
                    if !attachment.provisioned {
                        Text("provisioning…").foregroundStyle(.orange)
                    }
                    if let url = attachment.connection?.url {
                        IconActionButton("Copy connection URL", systemImage: "doc.on.doc") { Pasteboard.copy(url) }
                    }
                }
            } label: {
                Label(attachment.projectRoot, systemImage: "folder")
                    .lineLimit(1)
                    .truncationMode(.middle)
                    .textSelection(.enabled)
            }
            if let env = attachment.connection?.env, !env.isEmpty {
                ForEach(env.keys.sorted(), id: \.self) { key in
                    LabeledContent {
                        HStack(spacing: 4) {
                            Text(env[key] ?? "")
                                .font(.body.monospaced())
                                .lineLimit(1)
                                .truncationMode(.middle)
                                .textSelection(.enabled)
                            IconActionButton("Copy \(key)", systemImage: "doc.on.doc") {
                                Pasteboard.copy("\(key)=\(env[key] ?? "")")
                            }
                        }
                    } label: {
                        Text(key).font(.body.monospaced())
                    }
                }
            }
        }
    }
}
