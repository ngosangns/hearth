import SwiftUI
import HearthKit

/// Second column: the selected workspace, or the selected shared instance or recipe.
struct DetailView: View {
    @Environment(AppModel.self) private var model

    var body: some View {
        switch model.pane {
        case .workspaces: WorkspaceDetailView()
        case .shared: SharedDetailView()
        }
    }
}
