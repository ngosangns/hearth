import SwiftUI
import HearthKit

/// Services grouped by direct `groupTree` membership. Each service is a card that carries its own
/// URLs and actions, so nothing here depends on a separate selection list (and a selection change
/// never rebuilds the scroll content).
struct ServiceBoardView: View {
    @Environment(AppModel.self) private var model

    var body: some View {
        let hasGroups = model.sections.contains { $0.name != nil }
        VStack(alignment: .leading, spacing: Theme.Space.lg) {
            ForEach(model.sections) { section in
                VStack(alignment: .leading, spacing: Theme.Space.sm) {
                    if let name = section.name {
                        GroupHeader(name: name, count: section.services.count)
                    } else if hasGroups {
                        SectionHeading(title: "Other", count: section.services.count)
                    } else {
                        SectionHeading(title: "Services", count: section.services.count)
                    }
                    ForEach(section.services) { line in
                        ServiceCard(line: line)
                    }
                }
            }
            if model.sections.allSatisfy({ $0.services.isEmpty }) {
                StateView(systemImage: Icon.service, title: "No services in this catalog")
                    .frame(minHeight: 160)
            }
        }
    }
}

struct GroupHeader: View {
    @Environment(AppModel.self) private var model
    let name: String
    let count: Int

    var body: some View {
        SectionHeading(title: name, count: count) {
            HStack(spacing: Theme.Space.sm) {
                if ServiceBoard.groupIsUp(model.sections, name: name) {
                    Button { model.restartGroup(name) } label: { Label("Restart group", systemImage: Icon.restart) }
                        .help("Restart every service in \(name)")
                } else {
                    Button { model.startGroup(name) } label: { Label("Start group", systemImage: Icon.play) }
                        .help("Start every service in \(name)")
                }
                Button { model.stopGroup(name) } label: { Label("Stop group", systemImage: Icon.stop) }
                    .help("Stop every running service in \(name)")
            }
            .buttonStyle(.bordered)
            .controlSize(.regular)
        }
    }
}

/// One service: name, tags, state, ports, error, its URLs, and action buttons.
struct ServiceCard: View {
    @Environment(AppModel.self) private var model
    let line: ServiceBoard.Line
    @State private var showInfo = false

    private var selected: Bool { model.selectedService == line.id }

    var body: some View {
        Card {
            VStack(alignment: .leading, spacing: Theme.Space.sm) {
                HStack(alignment: .firstTextBaseline, spacing: Theme.Space.sm) {
                    FlowLayout {
                        Text(line.label)
                            .font(.headline)
                            .foregroundStyle(line.disabled ? .secondary : .primary)
                            .textSelection(.enabled)
                        if line.shared { Tag(text: "shared", tint: .purple) }
                        if line.finite { Tag(text: "job") }
                        if line.disabled { Tag(text: "disabled") }
                    }
                    if line.shared || line.infra {
                        Button { showInfo = true } label: { Label("Service info", systemImage: Icon.infoOutline).labelStyle(.iconOnly) }
                            .buttonStyle(.borderless)
                            .help(line.shared ? "Who uses \(line.sharedInstance ?? line.label)" : "Infrastructure service")
                            .popover(isPresented: $showInfo, arrowEdge: .bottom) {
                                ServiceInfoPopover(line: line)
                            }
                    }
                    Spacer(minLength: 0)
                    if !line.disabled {
                        StatePill(state: line.state)
                    }
                }
                if !line.ports.isEmpty {
                    Text("Ports  \(line.ports)").font(.caption.monospaced()).foregroundStyle(.secondary)
                }
                if let error = line.error {
                    Label(error, systemImage: Icon.warning).font(.caption).foregroundStyle(.red)
                        .fixedSize(horizontal: false, vertical: true).textSelection(.enabled)
                }
                let urls = model.urls(for: line.id)
                if !urls.isEmpty {
                    VStack(alignment: .leading, spacing: 2) {
                        ForEach(urls) { url in
                            HStack(spacing: Theme.Space.sm) {
                                Button { open(url.url) } label: { Label(url.label, systemImage: Icon.link) }
                                    .buttonStyle(.borderless).help("Open \(url.url)")
                                Text(url.url).font(.caption.monospaced()).foregroundStyle(.secondary)
                                    .lineLimit(1).truncationMode(.middle).textSelection(.enabled)
                            }
                        }
                    }
                }
                ServiceActionBar(line: line)
            }
        }
        .overlay(RoundedRectangle(cornerRadius: Theme.Radius.md).strokeBorder(selected ? Color.accentColor : .clear, lineWidth: 1.5))
        .contentShape(RoundedRectangle(cornerRadius: Theme.Radius.md))
        .onTapGesture { model.selectService(line.id) }
        .accessibilityElement(children: .contain)
    }

    private func open(_ string: String) {
        guard let url = URL(string: string), ["http", "https"].contains(url.scheme) else { return }
        NSWorkspace.shared.open(url)
    }
}

/// Start/Stop, Restart, and Reclaim for one service. Labels stay visible; the bar wraps.
struct ServiceActionBar: View {
    @Environment(AppModel.self) private var model
    let line: ServiceBoard.Line

    var body: some View {
        FlowLayout(spacing: Theme.Space.sm, lineSpacing: Theme.Space.sm) {
            if line.disabled {
                Text("This service is disabled.").font(.callout).foregroundStyle(.secondary)
            } else {
                if ServiceBoard.showsStop(line.state) {
                    Button { model.stop(line.id) } label: { Label("Stop", systemImage: Icon.stop) }
                        .help("Stop \(line.label)")
                } else {
                    Button { model.start(line.id) } label: { Label("Start", systemImage: Icon.play) }
                        .buttonStyle(.borderedProminent)
                        .help("Start \(line.label)")
                }
                Button { model.restart(line.id) } label: { Label("Restart", systemImage: Icon.restart) }
                    .help("Restart \(line.label)")
                if line.state == "externally-owned" {
                    Button(role: .destructive) { model.requestReclaim(line.id) } label: {
                        Label("Reclaim port", systemImage: Icon.reclaim)
                    }
                    .help("Kill the process holding the port, then start")
                }
            }
        }
    }
}

/// Popover for the info button: infrastructure note, and who uses a shared instance.
struct ServiceInfoPopover: View {
    @Environment(AppModel.self) private var model
    let line: ServiceBoard.Line

    var body: some View {
        VStack(alignment: .leading, spacing: Theme.Space.md) {
            if line.infra {
                Label("Infrastructure", systemImage: Icon.infoOutline)
                    .font(.headline)
                Text("\(line.label) is an infrastructure service.")
                    .font(.callout)
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
            }
            if let instance = line.sharedInstance {
                HStack(spacing: Theme.Space.sm) {
                    Image(systemName: Icon.shared).foregroundStyle(.secondary)
                    Text(instance).font(.headline)
                }
                let here = model.sections.flatMap(\.services).filter { $0.sharedInstance == instance }
                VStack(alignment: .leading, spacing: Theme.Space.xs) {
                    SectionHeading(title: "Services in this workspace", count: here.count)
                    ForEach(here) { service in
                        HStack(spacing: Theme.Space.sm) {
                            Text(service.label)
                            Spacer(minLength: Theme.Space.md)
                            if !service.disabled { StatePill(state: service.state) }
                        }
                    }
                }
                VStack(alignment: .leading, spacing: Theme.Space.xs) {
                    let roots = model.sharedInfo[instance]?.attachmentRoots
                    SectionHeading(title: "Attached workspaces", count: roots?.count)
                    if let roots {
                        if roots.isEmpty { Text("None.").font(.callout).foregroundStyle(.secondary) }
                        ForEach(roots, id: \.self) { root in
                            VStack(alignment: .leading, spacing: 0) {
                                Text(WorkspaceStore.folderName(root))
                                Text(WorkspaceStore.displayPath(root)).font(.caption).foregroundStyle(.secondary)
                            }
                        }
                    } else if model.sharedInfoFailed {
                        Text("Could not read the shared registry.").font(.callout).foregroundStyle(.secondary)
                    } else {
                        ProgressView().controlSize(.small)
                    }
                }
            }
        }
        .padding(Theme.Space.lg)
        .frame(minWidth: 260, maxWidth: 380, alignment: .leading)
        .task { if line.shared { await model.loadSharedInfo() } }
    }
}
