import SwiftUI
import AppKit
import NSShell

@main
struct HearthApp: App {
    @State private var model = AppModel()
    @NSApplicationDelegateAdaptor(ShellAppDelegate.self) private var delegate

    var body: some Scene {
        Window("Hearth", id: "main") {
            RootView()
                .environment(model)
                .task { model.start() }
                .frame(minWidth: 980, minHeight: 560)
        }
        .defaultSize(width: 1280, height: 780)
        .commands { AppCommands(model: model) }
    }
}

struct AppCommands: Commands {
    let model: AppModel

    var body: some Commands {
        CommandGroup(after: .sidebar) {
            command(.refresh)
            command(.toggleLog)
        }
        CommandMenu("Workspace") {
            command(.addFolder)
            Divider()
            command(.showWorkspaces)
            command(.showShared)
        }
    }

    private func command(_ id: HearthCommand) -> some View {
        Button(id.title(model)) { id.run(model) }
            .keyboardShortcut(id.shortcut, modifiers: id.modifiers)
    }
}
