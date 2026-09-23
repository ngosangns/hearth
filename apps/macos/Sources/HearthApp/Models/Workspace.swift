import Foundation

/// A folder the user added — the macOS-app equivalent of `--root` for the CLI/TUI. Adding a folder
/// never runs anything by itself; `trusted` gates the first `DaemonConnection.ensure` call for it
/// (see `WorkspaceController`), since a folder's `hearth.yaml` names arbitrary commands to
/// run — no different from opening an untrusted repo in an editor with tasks auto-run enabled.
struct Workspace: Codable, Equatable, Identifiable {
    let id: UUID
    var path: String
    var trusted: Bool
    let addedAt: Date

    init(id: UUID = UUID(), path: String, trusted: Bool = false, addedAt: Date = Date()) {
        self.id = id
        self.path = path
        self.trusted = trusted
        self.addedAt = addedAt
    }

    var displayName: String {
        URL(fileURLWithPath: path).lastPathComponent
    }

    /// True when the folder itself still exists on disk — a workspace is never silently dropped from
    /// the list just because its folder moved or was deleted; the UI surfaces this instead.
    var existsOnDisk: Bool {
        var isDirectory: ObjCBool = false
        return FileManager.default.fileExists(atPath: path, isDirectory: &isDirectory) && isDirectory.boolValue
    }
}
