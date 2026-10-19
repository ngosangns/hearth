import SwiftUI
import NSShell

/// One command, one title, one shortcut, one call.
/// Menus in `HearthApp` only bind these. Same split as Zed's `actions!` list
/// and the menu code that references it.
enum HearthCommand: String {
    case refresh
    case toggleLog
    case addFolder
    case showWorkspaces
    case showShared

    @MainActor
    func title(_ model: AppModel) -> String {
        switch self {
        case .refresh: "Refresh"
        case .toggleLog: model.logOpen ? "Hide Log" : "Show Log"
        case .addFolder: "Add Folder…"
        case .showWorkspaces: "Workspaces"
        case .showShared: "Shared Services"
        }
    }

    var shortcut: KeyEquivalent {
        switch self {
        case .refresh: "r"
        case .toggleLog: "l"
        case .addFolder: "o"
        case .showWorkspaces: "1"
        case .showShared: "2"
        }
    }

    var modifiers: EventModifiers {
        switch self {
        case .toggleLog: [.command, .option]
        default: .command
        }
    }

    @MainActor
    func run(_ model: AppModel) {
        switch self {
        case .refresh: model.refresh()
        case .toggleLog: model.toggleLog()
        case .addFolder: model.chooseFolder()
        case .showWorkspaces: model.pane = .workspaces
        case .showShared: model.pane = .shared
        }
    }
}
