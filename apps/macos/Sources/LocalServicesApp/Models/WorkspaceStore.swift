import Foundation

/// Persists the workspace list to `~/Library/Application Support/LocalServicesApp/workspaces.json`.
/// Deliberately plain `FileManager`/`JSONEncoder` (no Core Data / SwiftData) — a handful of folder
/// paths is a trivial amount of state, and a plain-text file is trivially inspectable/editable by
/// hand if something goes wrong.
@MainActor
final class WorkspaceStore: ObservableObject {
    @Published private(set) var workspaces: [Workspace] = []

    private let fileURL: URL

    init(fileURL: URL? = nil) {
        self.fileURL = fileURL ?? Self.defaultFileURL()
        load()
    }

    private static func defaultFileURL() -> URL {
        let base = FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask).first
            ?? FileManager.default.temporaryDirectory
        let dir = base.appendingPathComponent("LocalServicesApp", isDirectory: true)
        try? FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        return dir.appendingPathComponent("workspaces.json")
    }

    /// ISO8601, not `JSONDecoder`'s default (seconds since the 2001 reference date) — matches the
    /// ISO8601 timestamps this package's daemon itself uses everywhere (`state.ts`), and keeps
    /// `workspaces.json` actually readable by hand, per this type's own doc comment above.
    private static let decoder: JSONDecoder = {
        let decoder = JSONDecoder()
        decoder.dateDecodingStrategy = .iso8601
        return decoder
    }()
    private static let encoder: JSONEncoder = {
        let encoder = JSONEncoder()
        encoder.dateEncodingStrategy = .iso8601
        encoder.outputFormatting = [.prettyPrinted, .sortedKeys]
        return encoder
    }()

    private func load() {
        guard let data = try? Data(contentsOf: fileURL) else { return }
        workspaces = (try? Self.decoder.decode([Workspace].self, from: data)) ?? []
    }

    private func save() {
        guard let data = try? Self.encoder.encode(workspaces) else { return }
        try? data.write(to: fileURL, options: .atomic)
    }

    /// No-op (returns the existing entry) if `path` is already a workspace — adding the same folder
    /// twice should never produce two independent connections to the same daemon.
    @discardableResult
    func add(path: String) -> Workspace {
        if let existing = workspaces.first(where: { $0.path == path }) { return existing }
        let workspace = Workspace(path: path)
        workspaces.append(workspace)
        save()
        return workspace
    }

    func remove(id: UUID) {
        workspaces.removeAll { $0.id == id }
        save()
    }

    func setTrusted(_ trusted: Bool, id: UUID) {
        guard let index = workspaces.firstIndex(where: { $0.id == id }) else { return }
        workspaces[index].trusted = trusted
        save()
    }
}
