import SwiftUI

@main
struct HearthApp: App {
    @NSApplicationDelegateAdaptor(AppDelegate.self) private var appDelegate
    @StateObject private var workspaceStore = WorkspaceStore()
    @StateObject private var registry = WorkspaceControllerRegistry()

    var body: some Scene {
        WindowGroup("Hearth", id: MainWindow.id) {
            ContentView()
                .environmentObject(workspaceStore)
                .environmentObject(registry)
                .frame(minWidth: 760, minHeight: 480)
                .background(WindowFrameAutosave(name: "HearthMain"))
                .task { registry.sync(workspaceStore.workspaces) }
                .onChange(of: workspaceStore.workspaces) { workspaces in registry.sync(workspaces) }
                .onOpenURL { url in
                    workspaceStore.handleOpenURL(url)
                    registry.sync(workspaceStore.workspaces)
                }
        }
        .defaultSize(width: 960, height: 640)
        .handlesExternalEvents(matching: Set(arrayLiteral: "*"))

        MenuBarExtra {
            MenuBarContentView()
                .environmentObject(workspaceStore)
                .environmentObject(registry)
                .environmentObject(registry.menuBarPulse)
        } label: {
            MenuBarLabel()
                .environmentObject(workspaceStore)
                .environmentObject(registry)
                .environmentObject(registry.menuBarPulse)
        }
        .menuBarExtraStyle(.menu)
    }
}
