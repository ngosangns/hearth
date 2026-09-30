import SwiftUI

@main
struct HearthApp: App {
    @NSApplicationDelegateAdaptor(AppDelegate.self) private var appDelegate
    @StateObject private var workspaceStore = WorkspaceStore()
    @StateObject private var registry = WorkspaceControllerRegistry()
    /// The smp (shared services) connection — one machine-global daemon, so one app-level
    /// controller regardless of how many workspaces are open.
    @StateObject private var sharedController = SharedServicesController()

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
        .windowToolbarStyle(.unified)
        .handlesExternalEvents(matching: Set(arrayLiteral: "*"))

        // `Window` (not `WindowGroup`) — the shared catalog is a singleton: one smp daemon per
        // machine, so a second window would only show the same controller's state. `Window` scenes
        // also get a Window-menu entry for free.
        Window("Shared Services", id: SharedWindow.id) {
            SharedCatalogView(controller: sharedController)
                .frame(minWidth: 700, minHeight: 440)
                .background(WindowFrameAutosave(name: "HearthShared"))
        }
        .windowToolbarStyle(.unified)

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
