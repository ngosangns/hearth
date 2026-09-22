import Foundation

/// Persists the workspace list to `~/Library/Application Support/LocalServicesApp/workspaces.json`.
/// Deliberately plain `FileManager`/`JSONEncoder` (no Core Data / SwiftData) — a handful of folder
/// paths is a trivial amount of state, and a plain-text file is trivially inspectable/editable by
/// hand if something goes wrong.
@MainActor
final class WorkspaceStore: ObservableObject {
    @Published private(set) var workspaces: [Workspace] = []
    @Published var selectedId: UUID? {
        didSet { UserDefaults.standard.set(selectedId?.uuidString, forKey: Self.selectionKey) }
    }

    private static let selectionKey = "selectedWorkspaceId"

    private let fileURL: URL

    init(fileURL: URL? = nil) {
        self.fileURL = fileURL ?? Self.defaultFileURL()
        load()
        if let raw = UserDefaults.standard.string(forKey: Self.selectionKey), let id = UUID(uuidString: raw),
           workspaces.contains(where: { $0.id == id }) {
            selectedId = id
        }
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
        if let existing = workspaces.first(where: { $0.path == path }) {
            selectedId = existing.id
            return existing
        }
        let workspace = Workspace(path: path)
        workspaces.append(workspace)
        selectedId = workspace.id
        save()
        return workspace
    }

    func remove(id: UUID) {
        workspaces.removeAll { $0.id == id }
        if selectedId == id { selectedId = workspaces.first?.id }
        save()
    }

    /// `local-services://open?path=/abs/folder` — adds the folder if needed and selects it.
    /// Does not auto-trust; the trust prompt still gates the first daemon spawn.
    func handleOpenURL(_ url: URL) {
        guard url.scheme == "local-services" else { return }
        guard url.host == "open" else { return }
        guard let components = URLComponents(url: url, resolvingAgainstBaseURL: false),
              let path = components.queryItems?.first(where: { $0.name == "path" })?.value,
              !path.isEmpty else { return }
        _ = add(path: path)
    }

    func setTrusted(_ trusted: Bool, id: UUID) {
        guard let index = workspaces.firstIndex(where: { $0.id == id }) else { return }
        workspaces[index].trusted = trusted
        save()
    }
}
