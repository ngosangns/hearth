import SwiftUI

@main
struct LocalServicesApp: App {
    @StateObject private var workspaceStore = WorkspaceStore()
    @StateObject private var registry = WorkspaceControllerRegistry()

    var body: some Scene {
        WindowGroup("Local Services", id: "main") {
            ContentView()
                .environmentObject(workspaceStore)
                .environmentObject(registry)
                .frame(minWidth: 760, minHeight: 480)
                .task { registry.connectTrusted(workspaceStore.workspaces) }
                .onChange(of: workspaceStore.workspaces) { workspaces in registry.connectTrusted(workspaces) }
        }
        .windowResizability(.contentSize)

        MenuBarExtra {
            MenuBarContentView()
                .environmentObject(workspaceStore)
                .environmentObject(registry)
        } label: {
            MenuBarLabel()
                .environmentObject(workspaceStore)
                .environmentObject(registry)
        }
        .menuBarExtraStyle(.window)
    }
}
