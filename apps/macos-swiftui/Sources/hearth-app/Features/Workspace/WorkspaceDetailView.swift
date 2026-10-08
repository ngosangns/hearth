import SwiftUI
import HearthKit

/// Second column for a workspace: hero card with daemon actions, then services by group.
struct WorkspaceDetailView: View {
    @Environment(AppModel.self) private var model

    var body: some View {
        VStack(spacing: 0) {
            PaneHeader(title: model.selectedItem?.name ?? "Details", subtitle: model.summary.isEmpty ? nil : model.summary,
                       systemImage: model.selectedItem == nil ? nil : Icon.folder)
            if let item = model.selectedItem {
                content(item)
            } else {
                StateView(systemImage: Icon.sparkles, title: "Select a workspace",
                          message: "Pick a workspace from the sidebar, or add a project folder.") {
                    LogoMark(size: 56)
                }
            }
        }
    }

    @ViewBuilder private func content(_ item: WorkspaceItem) -> some View {
        switch model.phase {
        case .idle, .checking where model.sections.isEmpty:
            WorkingView(message: "Checking daemon…")
        default:
            ScrollView {
                VStack(alignment: .leading, spacing: Theme.Space.lg) {
                    WorkspaceHero(item: item)
                    if model.phase.isAttached || !model.sections.isEmpty {
                        ServiceBoardView()
                    } else {
                        PhaseNotice(item: item)
                    }
                }
                .frame(maxWidth: .infinity, alignment: .leading)
                .padding(Theme.Space.lg)
            }
        }
    }
}

private struct WorkspaceHero: View {
    @Environment(AppModel.self) private var model
    let item: WorkspaceItem

    var body: some View {
        Card {
            VStack(alignment: .leading, spacing: Theme.Space.md) {
                HeroHeader(systemImage: Icon.folderFilled, title: item.name, subtitle: item.path) {
                    if item.missing {
                        Tag(text: "missing", tint: .red, systemImage: Icon.warning)
                    } else if !item.trusted {
                        Tag(text: "untrusted", tint: .orange)
                    } else {
                        Tag(text: "trusted", tint: .green)
                    }
                    switch model.phase {
                    case .attached(let port, _): Tag(text: "daemon · port \(port)", tint: .blue)
                    case .detached(let port, _): Tag(text: "daemon up · port \(port) · not attached", tint: .orange)
                    case .stopped: Tag(text: "daemon stopped")
                    case .sessionEnded: Tag(text: "session ended", tint: .orange)
                    case .down: Tag(text: "daemon not running")
                    default: EmptyView()
                    }
                }
                ActionBar(isBusy: model.isBusy) {
                    if !item.trusted && !item.missing {
                        Button { model.requestTrust() } label: { Label("Trust and start", systemImage: Icon.trust) }
                            .buttonStyle(.borderedProminent)
                            .help("Trust this folder and start its daemon")
                    }
                    if item.trusted && !item.missing && !model.phase.isAttached {
                        Button { model.startDaemon() } label: { Label("Start daemon", systemImage: Icon.play) }
                            .buttonStyle(.borderedProminent)
                            .help("Start the daemon and attach this window")
                    }
                    if model.phase.isAttached {
                        Button { model.startAll() } label: { Label("Start all", systemImage: Icon.play) }
                            .buttonStyle(.borderedProminent)
                            .help("Start every service")
                        Button { model.stopAll() } label: { Label("Stop all", systemImage: Icon.stop) }
                            .help("Stop every running service")
                    }
                    if item.trusted && !item.missing && !item.stopped {
                        Button { model.requestRestartDaemon() } label: { Label("Restart daemon", systemImage: Icon.restart) }
                            .help("Restart the daemon; services keep running")
                        Button { model.requestStopDaemon() } label: { Label("Stop daemon", systemImage: Icon.stop) }
                            .help("Stop the daemon and its services")
                    }
                    if !item.missing {
                        Button { Finder.reveal(item.path) } label: { Label("Reveal", systemImage: Icon.reveal) }
                            .help("Reveal \(item.path) in Finder")
                    }
                    Button(role: .destructive) { model.requestForget() } label: { Label("Forget", systemImage: Icon.remove) }
                        .help("Remove from this list; services keep running")
                }
                .disabled(model.operations.contains { $0.title.hasPrefix("Starting daemon") })
            }
        }
    }
}

/// Explains a daemon that is not serving the board.
private struct PhaseNotice: View {
    @Environment(AppModel.self) private var model
    let item: WorkspaceItem

    var body: some View {
        let (icon, title, message): (String, String, String) = switch model.phase {
        case .missingFolder: (Icon.workspaceMissing, "Folder is missing", "\(item.displayPath) is not on disk. Forget removes it from the list.")
        case .down(let m): (Icon.flame, item.trusted ? "Daemon is not running" : "\(item.name) is untrusted",
                            m ?? (item.trusted ? "Start daemon runs it for \(item.displayPath)." : "Trusting starts a daemon that runs the commands in this folder's hearth.yaml."))
        case .detached(let port, _): (Icon.flame, "Daemon is up on port \(port)", "Start daemon attaches this window.")
        case .stopped: (Icon.stop, "Daemon stopped", "It stays stopped until you start it again.")
        case .sessionEnded: (Icon.warning, "Daemon session ended", "The daemon no longer accepts this window's token. Start daemon attaches again.")
        default: (Icon.flame, "No services", "")
        }
        StateView(systemImage: icon, title: title, message: message.isEmpty ? nil : message)
            .frame(minHeight: 200)
    }
}
