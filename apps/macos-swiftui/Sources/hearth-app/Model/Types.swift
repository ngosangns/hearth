import Foundation
import HearthKit

enum Pane: String, CaseIterable, Identifiable {
    case workspaces = "Workspaces"
    case shared = "Shared"
    var id: String { rawValue }
}

/// What the selected workspace's daemon looks like from this window.
enum DaemonPhase: Equatable {
    case idle
    case checking
    case missingFolder
    /// Not running. `message` is hearth's own text when it is more than "unavailable".
    case down(message: String?)
    /// Running, but this window holds no token. Start attaches.
    case detached(port: Int, proto: Int?)
    case attached(port: Int, proto: Int?)
    /// The user stopped it; it stays stopped until Start.
    case stopped
    /// The daemon refused our token.
    case sessionEnded

    var isAttached: Bool { if case .attached = self { true } else { false } }
}

struct WorkspaceItem: Identifiable, Equatable {
    let id: String
    let name: String
    let displayPath: String
    let path: String
    let trusted: Bool
    let missing: Bool
    let stopped: Bool
    let attached: Bool
}

struct Operation: Identifiable {
    let id = UUID()
    var title: String
}

struct Toast: Identifiable, Equatable {
    enum Style { case success, error, info }
    let id = UUID()
    let message: String
    let style: Style
}

struct Confirmation: Identifiable {
    let id = UUID()
    let title: String
    let message: String
    let confirmTitle: String
    let destructive: Bool
    let action: @MainActor () -> Void
}

/// What the Shared pane has selected: an installed instance or a catalog recipe.
enum SharedSelection: Hashable {
    case instance(String)
    case recipe(String)
}
