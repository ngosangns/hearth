import Foundation

/// Persists the workspace list to `~/Library/Application Support/HearthApp/workspaces.json`.
/// Deliberately plain `FileManager`/`JSONEncoder` (no Core Data / SwiftData) — a handful of folder
/// paths is a trivial amount of state, and a plain-text file is trivially inspectable/editable by
/// hand if something goes wrong.
@MainActor
final class WorkspaceStore: ObservableObject {
    @Published private(set) var workspaces: [Workspace] = []
    @Published var selectedId: UUID? {
        didSet { UserDefaults.standard.set(selectedId?.uuidString, forKey: Self.selectionKey) }
    }
    /// Why `workspaces.json` could not be read. The sidebar shows it until dismissed.
    @Published private(set) var loadError: String?

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
        let dir = base.appendingPathComponent("HearthApp", isDirectory: true)
        try? FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        return dir.appendingPathComponent("workspaces.json")
    }

    /// ISO8601, not `JSONDecoder`'s default (seconds since the 2001 reference date) — matches the
    /// ISO8601 timestamps the daemon itself uses everywhere (`rust/crates/hearth-core/src/state.rs`), and keeps
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

    /// A file that exists but does not decode is moved aside, not read as `[]`: the next `save()`
    /// would otherwise overwrite the user's list with the empty one.
    private func load() {
        guard let data = try? Data(contentsOf: fileURL) else { return }
        do {
            workspaces = try Self.decoder.decode([Workspace].self, from: data)
        } catch {
            let stamp = Int(Date().timeIntervalSince1970)
            let aside = fileURL.deletingLastPathComponent().appendingPathComponent("\(fileURL.lastPathComponent).corrupt-\(stamp)")
            if (try? FileManager.default.moveItem(at: fileURL, to: aside)) != nil {
                loadError = "\(fileURL.lastPathComponent) could not be read and was moved to \(aside.path). Starting with an empty workspace list."
            } else {
                loadError = "\(fileURL.lastPathComponent) could not be read: \(error.localizedDescription)"
            }
        }
    }

    func dismissLoadError() {
        loadError = nil
    }

    /// One spelling per folder: a trailing `/`, `.`/`..` segments or a symlinked path must not
    /// register the same daemon twice.
    static func normalizedPath(_ path: String) -> String {
        URL(fileURLWithPath: (path as NSString).expandingTildeInPath).standardizedFileURL.resolvingSymlinksInPath().path
    }

    private func save() {
        guard let data = try? Self.encoder.encode(workspaces) else { return }
        try? data.write(to: fileURL, options: .atomic)
    }

    /// No-op (returns the existing entry) if `path` is already a workspace — adding the same folder
    /// twice should never produce two independent connections to the same daemon. `path` must be
    /// absolute (see `handleOpenURL`); it is stored normalized.
    @discardableResult
    func add(path: String) -> Workspace {
        let path = Self.normalizedPath(path)
        if let existing = workspaces.first(where: { Self.normalizedPath($0.path) == path }) {
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

    /// `hearth://open?path=/abs/folder` — adds the folder if needed and selects it.
    /// Does not auto-trust; the trust prompt still gates the first daemon spawn. A relative path is
    /// rejected: it would resolve against whatever this process's cwd happens to be.
    func handleOpenURL(_ url: URL) {
        guard url.scheme == "hearth" else { return }
        guard url.host == "open" else { return }
        guard let components = URLComponents(url: url, resolvingAgainstBaseURL: false),
              let path = components.queryItems?.first(where: { $0.name == "path" })?.value,
              (path as NSString).expandingTildeInPath.hasPrefix("/") else { return }
        _ = add(path: path)
    }

    func setTrusted(_ trusted: Bool, id: UUID) {
        guard let index = workspaces.firstIndex(where: { $0.id == id }) else { return }
        workspaces[index].trusted = trusted
        save()
    }
}
