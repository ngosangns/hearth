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

    var body: some View {
        Group {
            switch controller.phase {
            case .idle, .connecting:
                ProgressView("Starting shared daemon…")
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
                catalogBody
            }
        }
        .navigationTitle("Shared Services")
        .toolbar {
            if controller.phase == .connected {
                ToolbarItem {
                    IconActionButton("Refresh", systemImage: "arrow.clockwise") {
                        Task { await controller.refresh(); await controller.refreshCatalog() }
                    }
                }
            }
        }
        .task {
            if controller.phase == .idle { await controller.connect() }
        }
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
                    Section("Installed on this machine") {
                        ForEach(controller.instances) { instance in
                            SharedInstanceRow(instance: instance, busy: controller.actionsInFlight.contains(instance.id))
                                .tag(instance.id)
                        }
                    }
                }
                if let services = controller.catalogDoc?.services, !services.isEmpty {
                    Section("Available") {
                        ForEach(availableRows(services), id: \.self) { row in
                            SharedCatalogRow(name: row.name, version: row.version, installed: row.installed, busy: controller.actionsInFlight.contains(row.id)) {
                                controller.install(row.id)
                            }
                            .tag(row.id)
                        }
                    }
                }
                if let error = controller.catalogError {
                    Section {
                        Label("Registry unavailable", systemImage: "exclamationmark.triangle")
                            .foregroundStyle(.orange)
                        Text(error).font(.caption2).foregroundStyle(.secondary)
                    }
                }
            }
            .navigationTitle("Catalog")
            .overlay {
                if controller.instances.isEmpty && (controller.catalogDoc?.services.isEmpty ?? true) {
                    ContentUnavailableViewCompat(
                        title: controller.catalogError != nil ? "Registry unavailable" : "Empty catalog",
                        message: controller.catalogError ?? "The shared-services registry lists nothing this machine can install.",
                        systemImage: "shippingbox"
                    )
                }
            }
        } detail: {
            detailPane
        }
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
                busy: controller.actionsInFlight.contains(instance.id),
                log: controller.logController(for: instance.id),
                onAction: { controller.perform($0, instance.id) },
                onRemove: { removeTarget = instance.id }
            )
            .id(instance.id)
        } else if let entry = controller.catalogEntry(controller.selectedId) {
            VStack(spacing: 12) {
                Image(systemName: "shippingbox").font(.system(size: 40)).foregroundStyle(.secondary)
                Text("\(entry.name) \(entry.version)").font(.title2).bold()
                Text("Not installed — the first project that needs it will install it, or install it now.")
                    .font(.subheadline).foregroundStyle(.secondary).multilineTextAlignment(.center).frame(maxWidth: 380)
                Button("Install \(entry.name)@\(entry.version)") { controller.install("\(entry.name)@\(entry.version)") }
                    .buttonStyle(.borderedProminent)
                    .disabled(controller.actionsInFlight.contains("\(entry.name)@\(entry.version)"))
            }
            .frame(maxWidth: .infinity, maxHeight: .infinity)
        } else {
            VStack(spacing: 8) {
                Image(systemName: "server.rack").font(.system(size: 32)).foregroundStyle(.secondary)
                Text("Select a shared service").foregroundStyle(.secondary)
            }
            .frame(maxWidth: .infinity, maxHeight: .infinity)
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
            StatusDot(state: instance.displayState)
            VStack(alignment: .leading, spacing: 2) {
                Text(instance.id).fontWeight(.medium)
                HStack(spacing: 6) {
                    Text(instance.displayState)
                        .font(.caption)
                        .foregroundStyle(StatusStyle.color(for: instance.displayState))
                    Text("port \(instance.port)").font(.caption).foregroundStyle(.secondary)
                    if instance.installState == "installing" {
                        Text("installing").font(.caption).foregroundStyle(.orange)
                    }
                    if let error = instance.installError {
                        Text(error).font(.caption).foregroundStyle(.red).lineLimit(1)
                    }
                    let attached = instance.attachments.count
                    if attached > 0 {
                        Text("\(attached) project\(attached == 1 ? "" : "s")").font(.caption).foregroundStyle(.secondary)
                    }
                }
            }
            Spacer()
            if busy { ActionSpinner() }
        }
        .padding(.vertical, 2)
        .contentShape(Rectangle())
    }
}

private struct SharedCatalogRow: View {
    let name: String
    let version: String
    let installed: Bool
    let busy: Bool
    let onInstall: () -> Void

    var body: some View {
        HStack(spacing: 8) {
            Image(systemName: "shippingbox").foregroundStyle(Color.accentColor)
            Text(name).fontWeight(.medium)
            Text(version)
                .font(.caption.monospaced())
                .foregroundStyle(.secondary)
                .padding(.horizontal, 6)
                .padding(.vertical, 1)
                .background(.quaternary, in: Capsule())
            Spacer()
            if busy {
                ActionSpinner()
            } else if installed {
                Label("Installed", systemImage: "checkmark.circle.fill")
                    .labelStyle(.iconOnly)
                    .foregroundStyle(.green)
                    .help("Already installed on this machine")
            } else {
                IconActionButton("Install \(name)@\(version)", systemImage: "square.and.arrow.down", action: onInstall)
            }
        }
        .contentShape(Rectangle())
    }
}

/// The detail pane for one registered instance: live state, which projects are attached (with their
/// rendered connection info, copyable), lifecycle actions, and the instance's log (install progress
/// and the service's own output both land there).
private struct SharedInstanceDetail: View {
    let instance: SharedInstance
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
                Text(instance.id).font(.headline)
                HStack(spacing: 8) {
                    Text(instance.displayState)
                    Text("port \(instance.port)")
                    Text(instance.installState)
                }
                .font(.caption).foregroundStyle(.secondary)
                if let error = instance.installError {
                    Text(error).font(.caption).foregroundStyle(.red)
                }
            }
            Spacer()
            if busy { ActionSpinner() }
            // Icon-only like the service rows — the tooltip carries the name.
            LifecycleButtons(state: instance.displayState, onAction: onAction)
            Button("Remove…", role: .destructive, action: onRemove)
                .padding(.leading, 8)
        }
        .padding(12)
    }

    @ViewBuilder
    private var attachments: some View {
        if instance.attachments.isEmpty {
            Text("No projects attached").font(.callout).foregroundStyle(.secondary).padding(12)
        } else {
            VStack(alignment: .leading, spacing: 8) {
                Text("Attached projects").font(.caption).foregroundStyle(.secondary).textCase(.uppercase)
                ForEach(instance.attachments, id: \.projectId) { attachment in
                    AttachmentRow(attachment: attachment)
                }
            }
            .padding(12)
        }
    }
}

private struct AttachmentRow: View {
    let attachment: SharedAttachment

    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            HStack {
                Image(systemName: "folder").foregroundStyle(.secondary)
                Text(attachment.projectRoot).font(.callout).lineLimit(1).truncationMode(.middle)
                if !attachment.provisioned {
                    Text("provisioning…").font(.caption).foregroundStyle(.orange)
                }
                Spacer()
                if let url = attachment.connection?.url {
                    IconActionButton("Copy connection URL", systemImage: "doc.on.doc") { Pasteboard.copy(url) }
                        .font(.caption)
                }
            }
            if let env = attachment.connection?.env, !env.isEmpty {
                ForEach(env.keys.sorted(), id: \.self) { key in
                    HStack {
                        Text(key).font(.caption.monospaced()).foregroundStyle(.secondary)
                        Text(env[key] ?? "").font(.caption.monospaced()).foregroundStyle(.secondary).lineLimit(1).truncationMode(.middle)
                        Spacer(minLength: 4)
                        IconActionButton("Copy \(key)", systemImage: "doc.on.doc") { Pasteboard.copy("\(key)=\(env[key] ?? "")") }
                            .font(.caption)
                    }
                }
            }
        }
    }
}
