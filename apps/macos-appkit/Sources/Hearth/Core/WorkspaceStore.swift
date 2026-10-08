import Foundation

/// `~/Library/Application Support/HearthApp/workspaces.json`, shared with `hearth tui`.
/// Rows are `{ id, path, trusted, addedAt }` in document order. `addedAt` is ISO-8601 UTC
/// with no fractional seconds. A file that does not decode is moved aside on the first
/// open of this process. A later reload keeps the in-memory list and leaves the file put.
struct WorkspaceRecord {
    let id: String
    let path: String
    var trusted: Bool
    let addedAt: String
}

final class WorkspaceStore {
    static let shared = WorkspaceStore()

    private(set) var rows: [WorkspaceRecord] = []
    private(set) var loadError: String?
    let path: String

    init(path: String? = nil) {
        let home = FileManager.default.homeDirectoryForCurrentUser.path
        self.path = path ?? "\(home)/Library/Application Support/HearthApp/workspaces.json"
        try? FileManager.default.createDirectory(
            atPath: (self.path as NSString).deletingLastPathComponent,
            withIntermediateDirectories: true
        )
        loadInitial()
    }

    func get(_ id: String) -> WorkspaceRecord? {
        rows.first { $0.id == id }
    }

    /// New folders start untrusted. Adding the same folder again returns the existing row.
    @discardableResult
    func add(_ input: String) throws -> (record: WorkspaceRecord, created: Bool) {
        let path = try Self.normalize(input)
        var isDir: ObjCBool = false
        guard FileManager.default.fileExists(atPath: path, isDirectory: &isDir), isDir.boolValue else {
            throw NSError(domain: "hearth", code: 1, userInfo: [NSLocalizedDescriptionKey: "folder does not exist: \(path)"])
        }
        if let row = rows.first(where: { $0.path == path }) {
            return (row, false)
        }
        let record = WorkspaceRecord(
            id: UUID().uuidString.uppercased(),
            path: path,
            trusted: false,
            addedAt: Self.addedNow()
        )
        rows.append(record)
        do {
            try save()
        } catch {
            rows.removeLast()
            throw error
        }
        return (record, true)
    }

    @discardableResult
    func trust(_ id: String) throws -> WorkspaceRecord {
        guard let index = rows.firstIndex(where: { $0.id == id }) else {
            throw NSError(domain: "hearth", code: 2, userInfo: [NSLocalizedDescriptionKey: "workspace not found"])
        }
        let previous = rows[index].trusted
        rows[index].trusted = true
        do {
            try save()
        } catch {
            rows[index].trusted = previous
            throw error
        }
        return rows[index]
    }

    @discardableResult
    func remove(_ id: String) throws -> Bool {
        guard let index = rows.firstIndex(where: { $0.id == id }) else { return false }
        let removed = rows[index]
        rows.remove(at: index)
        do {
            try save()
        } catch {
            rows.insert(removed, at: index)
            throw error
        }
        return true
    }

    /// Re-read the file. A parse failure keeps the current rows and does not quarantine.
    @discardableResult
    func reload() -> String? {
        guard FileManager.default.fileExists(atPath: path) else {
            rows = []
            loadError = nil
            return nil
        }
        guard let data = FileManager.default.contents(atPath: path) else {
            return "could not read \(path)"
        }
        if data.isEmpty {
            rows = []
            loadError = nil
            return nil
        }
        do {
            rows = try Self.decode(data)
            loadError = nil
            return nil
        } catch {
            return "\(path) could not be read: \(error.localizedDescription)"
        }
    }

    static func normalize(_ input: String) throws -> String {
        var input = input
        if input.isEmpty || input.contains("\0") {
            throw NSError(domain: "hearth", code: 3, userInfo: [NSLocalizedDescriptionKey: "path must be an absolute folder"])
        }
        if input == "~" || input.hasPrefix("~/") {
            let home = FileManager.default.homeDirectoryForCurrentUser.path
            guard !home.isEmpty else {
                throw NSError(domain: "hearth", code: 3, userInfo: [NSLocalizedDescriptionKey: "HOME is not set"])
            }
            input = input == "~" ? home : home + input.dropFirst()
        }
        guard input.hasPrefix("/") else {
            throw NSError(domain: "hearth", code: 3, userInfo: [NSLocalizedDescriptionKey: "path must be absolute"])
        }
        if FileManager.default.fileExists(atPath: input) {
            input = (input as NSString).resolvingSymlinksInPath
        }
        return input
    }

    static func displayPath(_ path: String) -> String {
        let home = FileManager.default.homeDirectoryForCurrentUser.path
        guard !home.isEmpty else { return path }
        if path == home { return "~" }
        let prefix = home + "/"
        if path.hasPrefix(prefix) { return "~/" + path.dropFirst(prefix.count) }
        return path
    }

    static func folderName(_ path: String) -> String {
        let name = (path as NSString).lastPathComponent
        return name.isEmpty || name == "/" ? path : name
    }

    static func addedNow() -> String {
        let formatter = ISO8601DateFormatter()
        formatter.formatOptions = [.withInternetDateTime]
        return formatter.string(from: Date())
    }

    private func loadInitial() {
        guard FileManager.default.fileExists(atPath: path),
              let data = FileManager.default.contents(atPath: path),
              !data.isEmpty else { return }
        do {
            rows = try Self.decode(data)
        } catch {
            quarantine(error.localizedDescription)
        }
    }

    private func quarantine(_ error: String) {
        let name = (path as NSString).lastPathComponent
        let aside = (path as NSString).deletingLastPathComponent + "/\(name).corrupt-\(Int(Date().timeIntervalSince1970))"
        if (try? FileManager.default.moveItem(atPath: path, toPath: aside)) != nil {
            loadError = "\(name) could not be read and was moved to \(aside). Starting with an empty workspace list."
        } else {
            loadError = "\(name) could not be read: \(error)"
        }
    }

    private static func decode(_ data: Data) throws -> [WorkspaceRecord] {
        guard let list = try JSONSerialization.jsonObject(with: data) as? [[String: Any]] else {
            throw NSError(domain: "hearth", code: 4, userInfo: [NSLocalizedDescriptionKey: "expected a list of objects"])
        }
        var rows: [WorkspaceRecord] = []
        for row in list {
            guard let id = row["id"] as? String,
                  let path = row["path"] as? String,
                  let trusted = row["trusted"] as? Bool,
                  let addedAt = row["addedAt"] as? String
            else {
                throw NSError(domain: "hearth", code: 4, userInfo: [NSLocalizedDescriptionKey: "rows need id/path/trusted/addedAt"])
            }
            rows.append(WorkspaceRecord(id: id, path: path, trusted: trusted, addedAt: addedAt))
        }
        return rows
    }

    private func save() throws {
        let list = rows.map { row -> [String: Any] in
            ["id": row.id, "path": row.path, "trusted": row.trusted, "addedAt": row.addedAt]
        }
        let data = try JSONSerialization.data(withJSONObject: list, options: [.prettyPrinted, .sortedKeys, .withoutEscapingSlashes])
        do {
            try (data + Data("\n".utf8)).write(to: URL(fileURLWithPath: path), options: .atomic)
        } catch {
            throw NSError(domain: "hearth", code: 5, userInfo: [NSLocalizedDescriptionKey: "cannot save \(path)"])
        }
    }
}
