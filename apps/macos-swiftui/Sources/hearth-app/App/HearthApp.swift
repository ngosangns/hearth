import SwiftUI
import AppKit

@main
struct HearthApp: App {
    @State private var model = AppModel()
    @NSApplicationDelegateAdaptor(AppDelegate.self) private var delegate

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

final class AppDelegate: NSObject, NSApplicationDelegate {
    func applicationDidFinishLaunching(_ notification: Notification) {
        NSApp.setActivationPolicy(.regular)
        NSApp.activate(ignoringOtherApps: true)
    }
    func applicationShouldTerminateAfterLastWindowClosed(_ sender: NSApplication) -> Bool { true }
}

struct AppCommands: Commands {
    let model: AppModel

    var body: some Commands {
        CommandGroup(after: .sidebar) {
            Button("Refresh") { model.refresh() }.keyboardShortcut("r")
            Button(model.logOpen ? "Hide Log" : "Show Log") { model.toggleLog() }
                .keyboardShortcut("l", modifiers: [.command, .option])
        }
        CommandMenu("Workspace") {
            Button("Add Folder…") { model.chooseFolder() }.keyboardShortcut("o")
            Divider()
            Button("Workspaces") { model.pane = .workspaces }.keyboardShortcut("1")
            Button("Shared Services") { model.pane = .shared }.keyboardShortcut("2")
        }
    }
}
