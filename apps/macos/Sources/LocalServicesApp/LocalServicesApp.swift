import SwiftUI

@main
struct LocalServicesApp: App {
    @StateObject private var workspaceStore = WorkspaceStore()

    var body: some Scene {
        WindowGroup("Local Services") {
            ContentView()
                .environmentObject(workspaceStore)
                .frame(minWidth: 760, minHeight: 480)
        }
        .windowResizability(.contentSize)
    }
}
