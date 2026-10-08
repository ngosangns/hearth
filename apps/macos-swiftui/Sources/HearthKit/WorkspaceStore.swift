import Foundation

/// One row of `workspaces.json`, the app's workspace list: `{ id, path, trusted, addedAt }`.
public struct WorkspaceRecord: Sendable, Equatable, Identifiable, Codable {
    public let id: String
    public let path: String
    public var trusted: Bool
    public let addedAt: String

    public init(id: String, path: String, trusted: Bool, addedAt: String) {
        self.id = id
        self.path = path
        self.trusted = trusted
        self.addedAt = addedAt
    }
}

public enum WorkspaceError: Error, LocalizedError, Equatable {
    case notAbsolute
    case missingFolder(String)
    case notFound
    case cannotSave(String)

    public var errorDescription: String? {
        switch self {
        case .notAbsolute: "Path must be an absolute folder."
        case .missingFolder(let p): "Folder does not exist: \(p)"
        case .notFound: "Workspace not found."
        case .cannotSave(let p): "Cannot save \(p)."
        }
    }
}

/// `~/Library/Application Support/HearthApp/workspaces.json`. Rows keep document order,
/// `addedAt` is ISO-8601 UTC without fractional seconds, ids are uppercase. A file that does
/// not decode is moved aside on the first open of this process; a later `reload` keeps the
/// in-memory rows and leaves the file where it is.
public final class WorkspaceStore: @unchecked Sendable {
    public private(set) var rows: [WorkspaceRecord] = []
    public private(set) var loadError: String?
    public let path: String

    public static func defaultPath(home: String = NSHomeDirectory(), environment: [String: String] = ProcessInfo.processInfo.environment) -> String {
        environment["HEARTH_WORKSPACE_FILE"].flatMap { $0.isEmpty ? nil : $0 }
            ?? "\(home)/Library/Application Support/HearthApp/workspaces.json"
    }

    public init(path: String = WorkspaceStore.defaultPath()) {
        self.path = path
        try? FileManager.default.createDirectory(
            atPath: (path as NSString).deletingLastPathComponent, withIntermediateDirectories: true)
        loadInitial()
    }

    public func get(_ id: String) -> WorkspaceRecord? { rows.first { $0.id == id } }

    /// New folders start untrusted. Adding a folder again returns the existing row.
    @discardableResult
    public func add(_ input: String) throws -> (record: WorkspaceRecord, created: Bool) {
        let path = try Self.normalize(input)
        var isDir: ObjCBool = false
        guard FileManager.default.fileExists(atPath: path, isDirectory: &isDir), isDir.boolValue else {
            throw WorkspaceError.missingFolder(path)
        }
        if let row = rows.first(where: { $0.path == path }) { return (row, false) }
        let record = WorkspaceRecord(id: UUID().uuidString.uppercased(), path: path, trusted: false, addedAt: Self.addedNow())
        rows.append(record)
        do { try save() } catch { rows.removeLast(); throw error }
        return (record, true)
    }

    @discardableResult
    public func trust(_ id: String) throws -> WorkspaceRecord {
        guard let index = rows.firstIndex(where: { $0.id == id }) else { throw WorkspaceError.notFound }
        let previous = rows[index].trusted
        rows[index].trusted = true
        do { try save() } catch { rows[index].trusted = previous; throw error }
        return rows[index]
    }

    @discardableResult
    public func remove(_ id: String) throws -> Bool {
        guard let index = rows.firstIndex(where: { $0.id == id }) else { return false }
        let removed = rows.remove(at: index)
        do { try save() } catch { rows.insert(removed, at: index); throw error }
        return true
    }

    /// Re-read the file. A parse failure keeps the current rows and does not quarantine.
    @discardableResult
    public func reload() -> String? {
        guard FileManager.default.fileExists(atPath: path) else { rows = []; loadError = nil; return nil }
        guard let data = FileManager.default.contents(atPath: path) else { return "Could not read \(path)." }
        if data.isEmpty { rows = []; loadError = nil; return nil }
        do {
            rows = try Self.decode(data)
            loadError = nil
            return nil
        } catch {
            return "\(path) could not be read: \(error.localizedDescription)"
        }
    }

    // MARK: Paths

    public static func normalize(_ input: String) throws -> String {
        var input = input.trimmingCharacters(in: .whitespacesAndNewlines)
        if input.isEmpty || input.contains("\0") { throw WorkspaceError.notAbsolute }
        if input == "~" || input.hasPrefix("~/") {
            let home = NSHomeDirectory()
            input = input == "~" ? home : home + input.dropFirst()
        }
        guard input.hasPrefix("/") else { throw WorkspaceError.notAbsolute }
        if FileManager.default.fileExists(atPath: input) { input = (input as NSString).resolvingSymlinksInPath }
        return input
    }

    public static func displayPath(_ path: String, home: String = NSHomeDirectory()) -> String {
        guard !home.isEmpty else { return path }
        if path == home { return "~" }
        return path.hasPrefix(home + "/") ? "~/" + path.dropFirst(home.count + 1) : path
    }

    public static func folderName(_ path: String) -> String {
        let name = (path as NSString).lastPathComponent
        return name.isEmpty || name == "/" ? path : name
    }

    static func addedNow() -> String {
        let formatter = ISO8601DateFormatter()
        formatter.formatOptions = [.withInternetDateTime]
        return formatter.string(from: Date())
    }

    // MARK: Persistence

    private func loadInitial() {
        guard let data = FileManager.default.contents(atPath: path), !data.isEmpty else { return }
        do { rows = try Self.decode(data) } catch { quarantine(error.localizedDescription) }
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

    static func decode(_ data: Data) throws -> [WorkspaceRecord] {
        try JSONDecoder().decode([WorkspaceRecord].self, from: data)
    }

    private func save() throws {
        let encoder = JSONEncoder()
        encoder.outputFormatting = [.prettyPrinted, .sortedKeys, .withoutEscapingSlashes]
        do {
            let data = try encoder.encode(rows) + Data("\n".utf8)
            try data.write(to: URL(fileURLWithPath: path), options: .atomic)
        } catch {
            throw WorkspaceError.cannotSave(path)
        }
    }
}
