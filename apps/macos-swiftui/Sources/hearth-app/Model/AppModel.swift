import SwiftUI
import Observation
import HearthKit

/// Single source of truth for UI state. Views read; user intents call the methods in this type and
/// its extensions. Every `hearth` call goes through `begin`/`end`, which drives the status bar.
///
/// Daemon sessions (bearer tokens) live only in `sessions`, in memory. A `manager ensure` or
/// `restart` re-attaches.
@Observable @MainActor
final class AppModel {
    // MARK: Environment
    let cli: HearthCLI?
    var cliVersion: String?
    var cliChecked = false
    var cliMissing: Bool { cliChecked && cliVersion == nil }

    // MARK: Workspaces
    let store: WorkspaceStore
    var workspaces: [WorkspaceItem] = []
    var selectedId: String?
    var pane: Pane = .workspaces {
        didSet { if pane == .shared, oldValue != .shared { loadShared() } }
    }

    // MARK: Selected workspace board
    var phase: DaemonPhase = .idle
    var sections: [ServiceBoard.Section] = []
    var summary = ""
    var urls: [ServiceBoard.VisibleURL] = []
    var groupCatalog = Catalog()
    var selectedService: String?
    var log = LogBuffer()
    var logOpen = true

    // MARK: Shared pane
    var recipes: [SharedCatalog.Recipe] = []
    var instances: [SharedInstance] = []
    var smpLive = false
    var sharedLoaded = false
    var sharedSelection: SharedSelection?
    /// Registry rows (with attachments) keyed by instance id; filled when an info popover opens.
    var sharedInfo: [String: SharedInstance] = [:]
    var sharedInfoFailed = false

    // MARK: Feedback
    var operations: [Operation] = []
    var isBusy: Bool { !operations.isEmpty }
    var toast: Toast?
    var confirmation: Confirmation?

    // MARK: Internals shared with the extensions
    var sessions: [String: Session] = [:]
    /// Workspace ids whose daemon the user stopped in this window.
    var stopped: Set<String> = []
    var catalogStamp: Int?
    var boardSeq = 0
    var logEpoch = 0
    var logLoading = false
    var discoverTask: Task<Void, Never>?
    var pollTask: Task<Void, Never>?
    private var toastTask: Task<Void, Never>?

    init(cli: HearthCLI? = .locate(), store: WorkspaceStore = WorkspaceStore()) {
        self.cli = cli
        self.store = store
        syncRows()
        selectedId = store.rows.first?.id
        if let error = store.loadError { toast = Toast(message: error, style: .error) }
    }

    // MARK: Derived

    var selectedRecord: WorkspaceRecord? { selectedId.flatMap { store.get($0) } }
    var selectedItem: WorkspaceItem? { workspaces.first { $0.id == selectedId } }
    var showsLog: Bool { logOpen && pane == .workspaces && phase.isAttached }

    func urls(for serviceId: String) -> [ServiceBoard.VisibleURL] {
        urls.filter { $0.serviceId == serviceId }
    }

    func line(_ id: String) -> ServiceBoard.Line? {
        sections.lazy.flatMap(\.services).first { $0.id == id }
    }

    // MARK: Lifecycle

    func start() {
        guard pollTask == nil else { return }
        Task {
            cliVersion = await cli?.version()
            cliChecked = true
            await attachAll()
            await discover()
        }
        pollTask = Task { [weak self] in
            while !Task.isCancelled {
                try? await Task.sleep(for: .seconds(2))
                await self?.tick()
            }
        }
    }

    /// `manager ensure` for every trusted on-disk workspace that has no session yet, so daemons
    /// come up on launch. Untrusted or missing folders still go through Trust or Forget.
    private func attachAll() async {
        guard let cli else { return }
        let targets = store.rows.filter {
            $0.trusted && isDirectory($0.path) && !stopped.contains($0.id) && sessions[$0.path] == nil
        }
        guard !targets.isEmpty else { return }
        let op = begin("Attaching daemons")
        defer { end(op) }
        let results = await withTaskGroup(of: (String, CommandResult).self) { group in
            for row in targets {
                group.addTask { (row.path, await cli.manager(root: row.path, "ensure")) }
            }
            var all: [(String, CommandResult)] = []
            for await item in group { all.append(item) }
            return all
        }
        var failed: [String] = []
        for (root, result) in results {
            if result.ok, let info = result.decode(DaemonInfo.self), let session = Session(info) {
                sessions[root] = session
            } else {
                failed.append(WorkspaceStore.folderName(root))
            }
        }
        syncRows()
        if !failed.isEmpty {
            notify("Auto-attach failed for \(ServiceBoard.joinNames(failed.sorted())).", .error)
        }
    }

    /// Every two seconds while the selected workspace is attached.
    private func tick() async {
        guard phase.isAttached, !stopped.contains(selectedId ?? "") else { return }
        await loadBoard()
    }

    // MARK: Rows

    func syncRows() {
        workspaces = store.rows.map { row in
            WorkspaceItem(
                id: row.id,
                name: WorkspaceStore.folderName(row.path),
                displayPath: WorkspaceStore.displayPath(row.path),
                path: row.path,
                trusted: row.trusted,
                missing: !isDirectory(row.path),
                stopped: stopped.contains(row.id),
                attached: sessions[row.path] != nil
            )
        }
    }

    func isDirectory(_ path: String) -> Bool {
        var isDir: ObjCBool = false
        return FileManager.default.fileExists(atPath: path, isDirectory: &isDir) && isDir.boolValue
    }

    // MARK: Selection

    func selectWorkspace(_ id: String?) {
        guard id != selectedId else { return }
        selectedId = id
        resetBoard()
        phase = id == nil ? .idle : .checking
        discoverTask?.cancel()
        // A short delay lets a scroll through several rows skip the `status` spawn per row.
        discoverTask = Task {
            try? await Task.sleep(for: .milliseconds(150))
            guard !Task.isCancelled else { return }
            await discover()
        }
    }

    func selectService(_ id: String) {
        guard id != selectedService else { if logOpen == false { logOpen = true }; return }
        selectedService = id
        log.reset()
        logEpoch += 1
        logLoading = false
        guard phase.isAttached else { return }
        Task { await loadLog() }
    }

    func resetBoard() {
        boardSeq += 1
        logEpoch += 1
        logLoading = false
        sections = []
        summary = ""
        urls = []
        groupCatalog = Catalog()
        catalogStamp = nil
        selectedService = nil
        log.reset()
    }

    // MARK: Feedback

    func begin(_ title: String) -> UUID {
        let op = Operation(title: title)
        operations.append(op)
        return op.id
    }

    func end(_ id: UUID) { operations.removeAll { $0.id == id } }

    func notify(_ message: String, _ style: Toast.Style = .info) {
        let t = Toast(message: message, style: style)
        toast = t
        toastTask?.cancel()
        toastTask = Task {
            try? await Task.sleep(for: .seconds(style == .error ? 8 : 4))
            if toast?.id == t.id { toast = nil }
        }
    }

    func dismissToast() { toast = nil }
}
