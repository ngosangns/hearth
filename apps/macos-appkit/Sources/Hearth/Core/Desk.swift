import Foundation

/// The whole desk model, ported from the Livewire `WorkspaceDesk` component.
/// Every action runs on one serial work queue — the same serialization Livewire
/// gives PHP per request — so state never needs a lock. Each mutation emits an
/// immutable snapshot to the main queue for the views.
final class DeskController {

    enum Pane: String {
        case workspaces, shared
    }

    struct WorkspaceRow {
        let id: String
        let name: String
        let path: String      // display path (~/)
        let fullPath: String
        let trusted: Bool
        let missing: Bool
        let stopped: Bool
        let hasToken: Bool
    }

    struct Snapshot {
        var pane: Pane = .workspaces
        var rows: [WorkspaceRow] = []
        var selected: WorkspaceRow?
        var notice: String?
        var loadError: String?
        var pendingKind: String?
        var pendingId: String?
        var sections: [ServiceBoard.ServiceSection] = []
        var summary = ""
        var urls: [ServiceBoard.VisibleURL] = []
        var selectedService = "$daemon"
        var selectedLine: ServiceBoard.ServiceLine?
        var logText = ""
        var logOpen = true
        var logHasMore = false
        var recipes: [ServiceBoard.Recipe] = []
        var instances: [ServiceBoard.Instance] = []
        var smpLive = false
        var busy = 0
        var binaryLine = ""
        var binaryExists = false
        var logSeq = 0
    }

    /// Called on the main queue with a fresh immutable snapshot after every change.
    var onChange: ((Snapshot) -> Void)?

    private let work = DispatchQueue(label: "hearth.desk", qos: .userInitiated)
    private let store: WorkspaceStore
    private let sessions = SessionStore.shared
    private let hearth = Hearth.shared

    private struct State {
        var pane: Pane = .workspaces
        var selectedId: String?
        var notice: String?
        var pendingKind: String?
        var pendingId: String?
        var sections: [ServiceBoard.ServiceSection] = []
        var summary = ""
        var urls: [ServiceBoard.VisibleURL] = []
        var catalogGroups: [String: [String]] = [:]
        var selectedService = "$daemon"
        var logText = ""
        var logCursor: Int?
        var logGeneration: Int?
        var logOpen = true
        var logLimit = 16384
        var logHasMore = false
        var serviceGeneration: Int?
        var catalogMtime: Int?
        var recipes: [ServiceBoard.Recipe] = []
        var instances: [ServiceBoard.Instance] = []
        var smpLive = false
        var busy = 0
        var binaryLine = ""
        var binaryExists = false
        var logSeq = 0
        var boardSeq = 0
    }

    private var state = State()

    init(store: WorkspaceStore = .shared) {
        self.store = store
    }

    // MARK: - state plumbing (work queue only)

    /// Mutate, then emit a snapshot on the main queue.
    private func mutate(_ body: (inout State) -> Void) {
        body(&state)
        emit()
    }

    private func emit() {
        let snapshot = snapshot()
        DispatchQueue.main.async { [weak self] in self?.onChange?(snapshot) }
    }

    private func snapshot() -> Snapshot {
        var snap = Snapshot()
        snap.pane = state.pane
        snap.notice = state.notice
        snap.loadError = store.loadError
        snap.pendingKind = state.pendingKind
        snap.pendingId = state.pendingId
        snap.sections = state.sections
        snap.summary = state.summary
        snap.urls = state.urls
        snap.selectedService = state.selectedService
        snap.logText = state.logText
        snap.logOpen = state.logOpen
        snap.logHasMore = state.logHasMore
        snap.recipes = state.recipes
        snap.instances = state.instances
        snap.smpLive = state.smpLive
        snap.busy = state.busy
        snap.binaryLine = state.binaryLine
        snap.binaryExists = state.binaryExists
        snap.logSeq = state.logSeq
        snap.rows = store.rows.map { present($0) }
        if let id = state.selectedId, let row = store.get(id) {
            snap.selected = present(row)
        }
        snap.selectedLine = line(state.selectedService)
        return snap
    }

    /// A user action: bumps `busy` while it runs.
    private func run(_ body: @escaping () -> Void) {
        work.async { [self] in
            mutate { $0.busy += 1 }
            body()
            mutate { $0.busy -= 1 }
        }
    }

    /// A background poll: serialized on the same queue, without the busy flag.
    private func poll(_ body: @escaping () -> Void) {
        work.async(execute: body)
    }

    private func present(_ row: WorkspaceRecord) -> WorkspaceRow {
        var isDir: ObjCBool = false
        let exists = FileManager.default.fileExists(atPath: row.path, isDirectory: &isDir)
        return WorkspaceRow(
            id: row.id,
            name: WorkspaceStore.folderName(row.path),
            path: WorkspaceStore.displayPath(row.path),
            fullPath: row.path,
            trusted: row.trusted,
            missing: !(exists && isDir.boolValue),
            stopped: sessions.isStopped(row.id),
            hasToken: sessions.hasToken(row.path)
        )
    }

    private func selectedRow() -> WorkspaceRecord? {
        guard let id = state.selectedId else { return nil }
        return store.get(id)
    }

    private func line(_ id: String) -> ServiceBoard.ServiceLine? {
        for section in state.sections {
            for service in section.services where service.id == id {
                return service
            }
        }
        return nil
    }

    private func isDirectory(_ path: String) -> Bool {
        var isDir: ObjCBool = false
        return FileManager.default.fileExists(atPath: path, isDirectory: &isDir) && isDir.boolValue
    }

    // MARK: - mount / navigation

    /// First open: report the binary, select the first workspace, attach every
    /// trusted on-disk workspace, then discover the selection.
    func start() {
        poll { [self] in
            let exists = hearth.binaryExists
            var line = "bundled hearth is missing or not executable"
            if exists {
                let result = hearth.run(["--version"], cwd: "/", timeout: 20)
                let text = (result.stdout + result.stderr).trimmingCharacters(in: .whitespacesAndNewlines)
                if !text.isEmpty { line = text }
            }
            mutate {
                $0.binaryLine = line
                $0.binaryExists = exists
                if $0.selectedId == nil {
                    $0.selectedId = store.rows.first?.id
                }
            }
            mutate { $0.busy += 1 }
            let attachFailed = attachAll()
            mutate { $0.busy -= 1 }
            discoverSelected()
            // discoverSelected always sets the notice — append launch failures
            // after it so they are not silently overwritten.
            if !attachFailed.isEmpty {
                mutate {
                    $0.notice = ($0.notice.map { $0 + " " } ?? "")
                        + "Auto-attach failed for \(ServiceBoard.joinNames(attachFailed))."
                }
            }
        }
    }

    /// `manager ensure` for every trusted, on-disk workspace that has no
    /// session yet — the daemons come up on launch without a click. Untrusted
    /// or missing folders still go through their explicit Trust/Forget flow;
    /// `discoverSelected` reports them normally afterward. Returns the display
    /// names that failed to attach.
    private func attachAll() -> [String] {
        var failed: [String] = []
        for row in store.rows {
            guard row.trusted, isDirectory(row.path),
                  !sessions.isStopped(row.id), !sessions.hasToken(row.path) else { continue }
            let result = hearth.manager(root: row.path, "ensure")
            guard result.ok, let payload = result.json,
                  payload["token"] != nil, payload["port"] != nil else {
                failed.append(WorkspaceStore.folderName(row.path))
                continue
            }
            sessions.put(root: row.path, payload: payload)
        }
        return failed.sorted()
    }

    /// Every two seconds while a workspace daemon is attached.
    func tick() {
        poll { [self] in
            guard let row = selectedRow(),
                  !sessions.isStopped(row.id),
                  sessions.hasToken(row.path) else { return }
            loadBoard(row.path)
        }
    }

    func addFolder(_ input: String) {
        run { [self] in
            mutate { $0.pendingKind = nil; $0.pendingId = nil }
            let added: (record: WorkspaceRecord, created: Bool)
            do {
                added = try store.add(input.trimmingCharacters(in: .whitespacesAndNewlines))
            } catch {
                mutate { $0.notice = error.localizedDescription }
                return
            }
            mutate { $0.selectedId = added.record.id }
            resetBoard()
            discoverSelected()
        }
    }

    func select(_ id: String) {
        poll { [self] in
            if state.pendingId != id {
                mutate { $0.pendingKind = nil; $0.pendingId = nil }
            }
            let changed = state.selectedId != id
            mutate { $0.selectedId = id }
            if changed { resetBoard() }
            discoverSelected()
        }
    }

    /// Re-read the list and discover the selection. Never calls `manager ensure`.
    func refreshList() {
        run { [self] in
            mutate { $0.pendingKind = nil; $0.pendingId = nil }
            if let error = store.reload() {
                mutate { $0.notice = error }
            }
            discoverSelected()
        }
    }

    func showPane(_ pane: Pane) {
        run { [self] in
            mutate {
                $0.pane = pane
                $0.pendingKind = nil
                $0.pendingId = nil
            }
            if pane == .shared {
                mutate { $0.notice = nil }
                loadShared()
            }
        }
    }

    func refreshShared() {
        run { [self] in
            mutate { $0.pendingKind = nil; $0.pendingId = nil; $0.notice = nil }
            loadShared()
        }
    }

    // MARK: - workspace actions

    func trust() {
        run { [self] in
            guard let row = selectedRow() else { return }
            guard isDirectory(row.path) else {
                mutate { $0.notice = missingNotice(row) }
                return
            }
            if row.trusted {
                startDaemon()
                return
            }
            guard armed("trust", row.id) else {
                mutate {
                    $0.pendingKind = "trust"
                    $0.pendingId = row.id
                    let name = WorkspaceStore.folderName(row.path)
                    let path = WorkspaceStore.displayPath(row.path)
                    $0.notice = "Press again to trust \(name) (\(path)) and start its daemon."
                }
                return
            }
            mutate { $0.pendingKind = nil; $0.pendingId = nil }
            _ = store.reload()
            let fresh: WorkspaceRecord
            do {
                fresh = try store.trust(row.id)
            } catch {
                mutate { $0.notice = error.localizedDescription }
                return
            }
            sessions.clearStopped(fresh.id)
            ensure(fresh)
        }
    }

    func startDaemon() {
        run { [self] in
            guard let row = selectedRow() else { return }
            guard isDirectory(row.path) else {
                mutate { $0.notice = missingNotice(row) }
                return
            }
            guard row.trusted else {
                mutate { $0.notice = "Trust the folder before starting its daemon." }
                return
            }
            mutate { $0.pendingKind = nil; $0.pendingId = nil }
            sessions.clearStopped(row.id)
            ensure(row)
        }
    }

    func stopDaemon() {
        run { [self] in
            guard let row = selectedRow(), isDirectory(row.path), row.trusted else { return }
            guard armed("stop", row.id) else {
                mutate {
                    $0.pendingKind = "stop"
                    $0.pendingId = row.id
                    $0.notice = "Press again to stop the daemon for \(WorkspaceStore.folderName(row.path)). Its services stop."
                }
                return
            }
            mutate { $0.pendingKind = nil; $0.pendingId = nil }
            sessions.markStopped(row.id)
            sessions.forgetRoot(row.path)
            resetBoard()
            let result = hearth.manager(root: row.path, "stop")
            mutate {
                $0.notice = result.ok
                    ? "Daemon stopped. Start runs it again."
                    : (result.visibleMessage.isEmpty ? "stop daemon failed" : result.visibleMessage)
            }
        }
    }

    func restartDaemon() {
        run { [self] in
            guard let row = selectedRow(), isDirectory(row.path), row.trusted,
                  !sessions.isStopped(row.id) else { return }
            guard armed("restart-daemon", row.id) else {
                mutate {
                    $0.pendingKind = "restart-daemon"
                    $0.pendingId = row.id
                    $0.notice = "Press again to restart the daemon for \(WorkspaceStore.folderName(row.path)). Its services keep running."
                }
                return
            }
            mutate { $0.pendingKind = nil; $0.pendingId = nil }
            let result = hearth.manager(root: row.path, "restart")
            guard result.ok, let payload = result.json,
                  payload["token"] != nil, payload["port"] != nil else {
                mutate { $0.notice = result.visibleMessage.isEmpty ? "restart daemon failed" : result.visibleMessage }
                return
            }
            sessions.put(root: row.path, payload: payload)
            sessions.clearStopped(row.id)
            mutate {
                $0.logCursor = nil
                $0.logGeneration = nil
                let port = payload["port"] ?? ""
                let proto = payload["protocolVersion"]
                $0.notice = "Daemon is up on port \(port)\(proto != nil ? ", protocol \(proto!)" : "")."
            }
            loadBoard(row.path)
        }
    }

    func forget() {
        run { [self] in
            guard let row = selectedRow() else { return }
            guard armed("forget", row.id) else {
                mutate {
                    $0.pendingKind = "forget"
                    $0.pendingId = row.id
                    $0.notice = "Press again to forget \(WorkspaceStore.folderName(row.path)). Its services keep running."
                }
                return
            }
            mutate { $0.pendingKind = nil; $0.pendingId = nil }
            _ = store.reload()
            do {
                try store.remove(row.id)
            } catch {
                mutate { $0.notice = error.localizedDescription }
                return
            }
            sessions.forgetId(row.id)
            sessions.forgetRoot(row.path)
            resetBoard()
            let next = store.rows.first?.id
            mutate { $0.selectedId = next }
            if next != nil { discoverSelected() }
            mutate { $0.notice = "Forgot the workspace. Its services keep running." }
        }
    }

    // MARK: - service selection & log

    func selectService(_ id: String) {
        poll { [self] in
            mutate {
                $0.selectedService = id
                $0.logText = ""
                $0.logCursor = nil
                $0.logGeneration = nil
                $0.serviceGeneration = nil
                $0.logOpen = true
                $0.logLimit = 16384
                $0.logHasMore = false
                $0.logSeq += 1
            }
            guard let row = selectedRow(),
                  !sessions.isStopped(row.id),
                  sessions.hasToken(row.path),
                  let api = ManagerAPI.open(root: row.path) else { return }
            loadLog(api)
        }
    }

    func toggleLog() {
        poll { [self] in
            let open = !state.logOpen
            mutate { $0.logOpen = open }
            guard open else { return }
            guard let row = selectedRow(),
                  !sessions.isStopped(row.id),
                  sessions.hasToken(row.path),
                  let api = ManagerAPI.open(root: row.path) else { return }
            loadLog(api)
        }
    }

    func expandLog() {
        run { [self] in
            guard state.logOpen, state.logHasMore else { return }
            mutate {
                $0.logLimit = min($0.logLimit * 4, 262144)
                $0.logText = ""
                $0.logCursor = nil
                $0.logGeneration = nil
                $0.logHasMore = false
            }
            guard let row = selectedRow(),
                  !sessions.isStopped(row.id),
                  sessions.hasToken(row.path),
                  let api = ManagerAPI.open(root: row.path) else { return }
            loadLog(api)
        }
    }

    // MARK: - service actions

    func startService(_ id: String) {
        run { [self] in
            guard let row = line(id), !row.disabled else { return }
            mutate { $0.pendingKind = nil; $0.pendingId = nil }
            finishOne(id, "start", killUnowned: false)
        }
    }

    func stopService(_ id: String) {
        run { [self] in
            guard let row = line(id), !row.disabled else { return }
            keepArmed("project-stop", id)
            guard allowProjectShared("project-stop", "stop", id, [id]) else { return }
            finishOne(id, "stop", killUnowned: false)
        }
    }

    func restartService(_ id: String) {
        run { [self] in
            guard let row = line(id), !row.disabled else { return }
            keepArmed("project-restart", id)
            guard allowProjectShared("project-restart", "restart", id, [id]) else { return }
            finishOne(id, "restart", killUnowned: false)
        }
    }

    func reclaimPort(_ id: String) {
        run { [self] in
            guard let row = line(id), !row.disabled, row.state == "externally-owned" else { return }
            keepArmed("kill", id)
            guard armed("kill", id) else {
                mutate {
                    $0.pendingKind = "kill"
                    $0.pendingId = id
                    $0.notice = "Press again to reclaim the port for \(id) and start it."
                }
                return
            }
            mutate { $0.pendingKind = nil; $0.pendingId = nil }
            finishOne(id, "start", killUnowned: true)
        }
    }

    func startAll() {
        run { [self] in
            mutate { $0.pendingKind = nil; $0.pendingId = nil }
            var ids = ServiceBoard.startAllTargets(groups: state.catalogGroups, sections: state.sections)
            ids = ids.filter { line($0)?.disabled != true }
            runMany(ids, "start", done: "Started.")
        }
    }

    func stopAll() {
        run { [self] in
            keepArmed("stop-all", "all")
            let ids = ServiceBoard.stopAllTargets(state.sections)
            guard allowProjectShared("stop-all", "stop", "all", ids) else { return }
            runMany(ids, "stop", done: "Stopped.")
        }
    }

    func startGroup(_ name: String) {
        run { [self] in
            mutate { $0.pendingKind = nil; $0.pendingId = nil }
            runMany(ServiceBoard.groupTargets(state.sections, name: name), "start", done: "Started \(name).")
        }
    }

    func stopGroup(_ name: String) {
        run { [self] in
            keepArmed("stop-group", name)
            let ids = ServiceBoard.groupTargets(state.sections, name: name).filter {
                guard let row = line($0) else { return false }
                return !["stopped", "succeeded"].contains(row.state)
            }
            guard allowProjectShared("stop-group", "stop", name, ids) else { return }
            runMany(ids, "stop", done: "Stopped \(name).")
        }
    }

    func restartGroup(_ name: String) {
        run { [self] in
            keepArmed("restart-group", name)
            let ids = ServiceBoard.groupTargets(state.sections, name: name)
            guard allowProjectShared("restart-group", "restart", name, ids) else { return }
            runMany(ids, "restart", done: "Restarted \(name).")
        }
    }

    // MARK: - shared pane actions

    func installRecipe(_ id: String) {
        run { [self] in
            mutate { $0.pendingKind = nil; $0.pendingId = nil }
            let result = hearth.run(["shared", "install", id, "--json"], cwd: "/", timeout: nil)
            mutate { $0.notice = result.ok ? "Installed \(id)." : (result.visibleMessage.isEmpty ? "install failed" : result.visibleMessage) }
            loadShared()
        }
    }

    func startInstance(_ id: String) {
        run { [self] in
            mutate { $0.pendingKind = nil; $0.pendingId = nil }
            let result = hearth.run(["shared", "start", id, "--json"], cwd: "/", timeout: nil)
            mutate { $0.notice = result.ok ? "Started \(id)." : (result.visibleMessage.isEmpty ? "start failed" : result.visibleMessage) }
            loadShared()
        }
    }

    func stopInstance(_ id: String) {
        run { [self] in
            keepArmed("instance-stop", id)
            guard allowInstance(id, "stop") else { return }
            let result = hearth.run(["shared", "stop", id, "--json"], cwd: "/", timeout: 120)
            mutate { $0.notice = result.ok ? "Stopped \(id)." : (result.visibleMessage.isEmpty ? "stop failed" : result.visibleMessage) }
            loadShared()
        }
    }

    func restartInstance(_ id: String) {
        run { [self] in
            keepArmed("instance-restart", id)
            guard allowInstance(id, "restart") else { return }
            let stop = hearth.run(["shared", "stop", id, "--json"], cwd: "/", timeout: 120)
            guard stop.ok else {
                mutate { $0.notice = stop.visibleMessage.isEmpty ? "stop failed" : stop.visibleMessage }
                loadShared()
                return
            }
            let start = hearth.run(["shared", "start", id, "--json"], cwd: "/", timeout: nil)
            mutate { $0.notice = start.ok ? "Restarted \(id)." : (start.visibleMessage.isEmpty ? "start failed" : start.visibleMessage) }
            loadShared()
        }
    }

    func removeInstance(_ id: String) {
        run { [self] in
            keepArmed("shared-remove", id)
            let (unchecked, affected, force) = removeImpact(id)
            guard armed("shared-remove", id) else {
                mutate {
                    $0.pendingKind = "shared-remove"
                    $0.pendingId = id
                    $0.notice = ServiceBoard.removeNotice(id, affected: affected, unchecked: unchecked)
                }
                return
            }
            mutate { $0.pendingKind = nil; $0.pendingId = nil }
            var args = ["shared", "remove", id, "--json"]
            if force { args.append("--force") }
            let result = hearth.run(args, cwd: "/", timeout: 120)
            mutate { $0.notice = result.ok ? "Removed \(id)." : (result.visibleMessage.isEmpty ? "remove failed" : result.visibleMessage) }
            loadShared()
        }
    }

    // MARK: - internals (work queue only)

    /// Re-read the daemon status for the selected workspace. Never ensures.
    private func discoverSelected() {
        guard let row = selectedRow() else { return }
        guard isDirectory(row.path) else {
            resetBoard()
            mutate { $0.notice = missingNotice(row) }
            return
        }
        let result = hearth.manager(root: row.path, "status")
        let stopped = sessions.isStopped(row.id)
        if !result.ok { sessions.forgetRoot(row.path) }
        if stopped {
            resetBoard()
            mutate {
                $0.notice = result.ok
                    ? "Daemon is stopping. Start runs it again."
                    : "Daemon stopped. Start runs it again."
            }
            return
        }
        if result.ok {
            let port = result.json?["port"]
            let proto = result.json?["protocolVersion"]
            if !sessions.hasToken(row.path) {
                resetBoard()
                mutate {
                    $0.notice = "Daemon is up on port \(port ?? "")\(proto != nil ? ", protocol \(proto!)" : ""). Start attaches this window."
                }
                return
            }
            mutate {
                $0.notice = "Daemon is up on port \(port ?? "")\(proto != nil ? ", protocol \(proto!)" : "")."
            }
            loadBoard(row.path)
            return
        }
        resetBoard()
        mutate {
            let message = result.visibleMessage
            let name = WorkspaceStore.folderName(row.path)
            let path = WorkspaceStore.displayPath(row.path)
            if message.isEmpty || message == "hearth manager is unavailable" {
                $0.notice = row.trusted
                    ? "\(name) (\(path)). Start runs the daemon."
                    : "\(name) (\(path)) is untrusted. Trust starts the daemon."
            } else {
                $0.notice = message
            }
        }
    }

    private func ensure(_ row: WorkspaceRecord) {
        let result = hearth.manager(root: row.path, "ensure")
        guard result.ok, let payload = result.json,
              payload["token"] != nil, payload["port"] != nil else {
            mutate { $0.notice = result.visibleMessage.isEmpty ? "manager ensure failed" : result.visibleMessage }
            return
        }
        sessions.put(root: row.path, payload: payload)
        guard sessions.hasToken(row.path) else {
            mutate { $0.notice = "manager ensure failed" }
            return
        }
        let port = (payload["port"] as? Int) ?? Int(payload["port"] as? String ?? "") ?? 0
        let proto = payload["protocolVersion"]
        let healthy = healthz(port)
        mutate {
            $0.notice = "Daemon is up on port \(port)\(proto != nil ? ", protocol \(proto!)" : "")."
                + (healthy ? "" : " healthz did not answer.")
        }
        loadBoard(row.path)
    }

    private func healthz(_ port: Int) -> Bool {
        guard let url = URL(string: "http://127.0.0.1:\(port)/healthz") else { return false }
        var request = URLRequest(url: url, timeoutInterval: 5)
        request.httpMethod = "GET"
        var response: URLResponse?
        let semaphore = DispatchSemaphore(value: 0)
        URLSession.shared.dataTask(with: request) { _, resp, _ in
            response = resp
            semaphore.signal()
        }.resume()
        semaphore.wait()
        return (response as? HTTPURLResponse).map { (200..<400).contains($0.statusCode) } ?? false
    }

    private func loadBoard(_ root: String) {
        maybeReload(root)
        state.boardSeq += 1
        let seq = state.boardSeq
        guard let api = ManagerAPI.open(root: root) else {
            resetBoard()
            mutate { $0.notice = "Daemon session ended. Start attaches this window." }
            return
        }
        let services = api.get("/v1/services")
        if services.unauthorized {
            resetBoard()
            mutate { $0.notice = "Daemon session ended. Start attaches this window." }
            return
        }
        guard services.ok else { return }
        let catalog = api.get("/v1/catalog")
        var body: [String: Any] = ["services": [], "groups": [:], "groupTree": []]
        if catalog.unauthorized {
            resetBoard()
            mutate { $0.notice = "Daemon session ended. Start attaches this window." }
            return
        }
        if catalog.ok, let doc = catalog.json?["catalog"] as? [String: Any] {
            body = doc
        }
        let live = services.json?["services"] as? [[String: Any]] ?? []
        let urls = api.get("/v1/urls")
        if urls.unauthorized {
            resetBoard()
            mutate { $0.notice = "Daemon session ended. Start attaches this window." }
            return
        }
        let urlRows = urls.ok ? (urls.json?["urls"] as? [[String: Any]] ?? []) : []
        let sections = ServiceBoard.sections(catalog: body, live: live)
        let groups = body["groups"] as? [String: [String]] ?? [:]
        let summary = ServiceBoard.summary(sections)
        let visibleUrls = ServiceBoard.visibleUrls(urlRows, sections: sections)
        mutate {
            if $0.boardSeq == seq {
                $0.catalogGroups = groups
                $0.sections = sections
                $0.summary = summary
                $0.urls = visibleUrls
            }
        }
        syncLifecycle(live)
        if state.logOpen {
            loadLog(api)
        }
    }

    private func syncLifecycle(_ live: [[String: Any]]) {
        mutate {
            guard $0.selectedService != "$daemon" else { return }
            var next: Int?
            for row in live where (row["serviceId"] as? String) == $0.selectedService {
                if let gen = row["generation"] as? Int { next = gen }
            }
            if $0.serviceGeneration != nil && next != $0.serviceGeneration {
                $0.logCursor = nil
                $0.logGeneration = nil
            }
            $0.serviceGeneration = next
        }
    }

    private func loadLog(_ api: ManagerAPI) {
        let service = state.selectedService
        let cursor = state.logCursor
        let generation = state.logGeneration
        let limit = state.logLimit
        let result = service == "$daemon"
            ? api.daemonLog(bytes: limit)
            : api.serviceLog(service, cursor: cursor, generation: generation, limit: limit)
        if result.unauthorized {
            resetBoard()
            mutate { $0.notice = "Daemon session ended. Start attaches this window." }
            return
        }
        guard result.ok, let slice = result.json else { return }
        applyLog(slice, limit: limit)
    }

    private func applyLog(_ slice: [String: Any], limit: Int) {
        let data = slice["data"] as? String ?? ""
        let reset = slice["reset"] as? Bool ?? false
        mutate {
            if reset {
                $0.logHasMore = data.count >= limit - 16 && limit < 262144
            }
            $0.logText = ServiceBoard.boundedTail((reset || $0.logText.isEmpty) ? data : $0.logText + data, limit: 262144)
            if let cursor = slice["nextCursor"] as? Int { $0.logCursor = cursor }
            if let generation = slice["generation"] as? Int { $0.logGeneration = generation }
        }
    }

    private func maybeReload(_ root: String) {
        let next = ServiceBoard.catalogStamp(root: root)
        if ServiceBoard.shouldReloadCatalog(previous: state.catalogMtime, next: next) {
            let result = hearth.manager(root: root, "reload")
            if !result.ok {
                mutate {
                    $0.notice = result.visibleMessage.isEmpty
                        ? "Catalog reload failed. The running catalog stays."
                        : result.visibleMessage
                }
            }
        }
        if next != nil {
            mutate { $0.catalogMtime = next }
        }
    }

    private func finishOne(_ id: String, _ action: String, killUnowned: Bool) {
        let error = runAction(id, action, killUnowned)
        afterActions(error == nil ? "\(doneWord(action)) \(id)." : error)
    }

    private func runMany(_ ids: [String], _ action: String, done: String) {
        if ids.isEmpty {
            afterActions(action == "stop" ? "Nothing to stop." : "Nothing to start.")
            return
        }
        var failed: [String] = []
        for id in ids {
            let error = runAction(id, action, false)
            if error == "session" {
                afterActions("session")
                return
            }
            if error != nil { failed.append(id) }
        }
        afterActions(failed.isEmpty ? done : "Failed: \(failed.joined(separator: ", ")).")
    }

    private func afterActions(_ notice: String?) {
        if notice == "session" {
            resetBoard()
            mutate { $0.notice = "Daemon session ended. Start attaches this window." }
            return
        }
        if let row = selectedRow(),
           sessions.hasToken(row.path),
           !sessions.isStopped(row.id) {
            loadBoard(row.path)
        }
        mutate { $0.notice = notice }
    }

    private func runAction(_ serviceId: String, _ action: String, _ killUnowned: Bool) -> String? {
        guard let row = selectedRow() else { return "No workspace selected." }
        guard let api = ManagerAPI.open(root: row.path) else { return "session" }
        let posted = api.submit(serviceId: serviceId, action: action, killUnowned: killUnowned)
        if posted.unauthorized { return "session" }
        guard posted.ok else {
            return posted.message.isEmpty ? "\(action) failed" : posted.message
        }
        let operation = posted.json?["operation"] as? [String: Any]
        let opId = operation?["id"] as? String
        let status = operation?["status"] as? String
        guard let opId, !opId.isEmpty else { return "\(action) failed" }
        if status == "failed" { return operationMessage(operation) ?? "\(action) failed" }
        if status == "succeeded" { return nil }
        let waited = api.wait(operationId: opId)
        if waited.unauthorized { return "session" }
        guard waited.ok else {
            return waited.message.isEmpty ? "\(action) failed" : waited.message
        }
        let settled = waited.json?["operation"] as? [String: Any] ?? [:]
        if settled["status"] as? String == "succeeded" { return nil }
        return operationMessage(settled) ?? "\(action) failed"
    }

    private func operationMessage(_ operation: [String: Any]?) -> String? {
        guard let error = operation?["error"] as? [String: Any],
              let message = error["message"] as? String, !message.isEmpty else { return nil }
        return message
    }

    // MARK: - shared impact checks

    private func allowProjectShared(_ kind: String, _ verb: String, _ key: String, _ serviceIds: [String]) -> Bool {
        var instances: Set<String> = []
        for id in serviceIds {
            if let instance = line(id)?.sharedInstance, !instance.isEmpty {
                instances.insert(instance)
            }
        }
        if instances.isEmpty { return true }
        if armed(kind, key) {
            mutate { $0.pendingKind = nil; $0.pendingId = nil }
            return true
        }
        let row = selectedRow()
        let currentName = row.map { WorkspaceStore.folderName($0.path) } ?? "this workspace"
        let currentRoot = row?.path
        guard let payload = installedPayload() else {
            mutate {
                $0.pendingKind = kind
                $0.pendingId = key
                $0.notice = ServiceBoard.uncheckedSharedNotice(verb, instance: ServiceBoard.joinNames(instances.sorted()))
            }
            return false
        }
        let byId = instancesById(payload)
        var touches: [(instance: String, others: [String])] = []
        var unknown: [String] = []
        for instanceId in instances.sorted() {
            guard let instance = byId[instanceId] else {
                unknown.append(instanceId)
                continue
            }
            let report = ServiceBoard.classifyAttachments(
                ServiceBoard.attachmentRoots(instance), current: currentRoot, known: knownRoots())
            if !report.others.isEmpty {
                touches.append((instanceId, report.others))
            }
        }
        if touches.isEmpty && unknown.isEmpty { return true }
        touches.sort { $0.instance < $1.instance }
        unknown.sort()
        mutate {
            $0.pendingKind = kind
            $0.pendingId = key
            $0.notice = touches.isEmpty
                ? ServiceBoard.uncheckedSharedNotice(verb, instance: ServiceBoard.joinNames(unknown))
                : ServiceBoard.projectSharedNotice(verb, current: currentName, touches: touches, unknown: unknown)
        }
        return false
    }

    private func allowInstance(_ id: String, _ verb: String) -> Bool {
        if armed("instance-\(verb)", id) {
            mutate { $0.pendingKind = nil; $0.pendingId = nil }
            return true
        }
        guard let payload = installedPayload() else {
            armInstance(verb, id, ServiceBoard.uncheckedSharedNotice(verb, instance: id))
            return false
        }
        guard let instance = instancesById(payload)[id] else {
            armInstance(verb, id, ServiceBoard.uncheckedSharedNotice(verb, instance: id))
            return false
        }
        let current = selectedRow()?.path
        let report = ServiceBoard.classifyAttachments(
            ServiceBoard.attachmentRoots(instance), current: current, known: knownRoots())
        if report.others.isEmpty { return true }
        armInstance(verb, id, ServiceBoard.instanceSharedNotice(verb, instance: id, affected: report.all))
        return false
    }

    private func armInstance(_ verb: String, _ id: String, _ notice: String) {
        mutate {
            $0.pendingKind = "instance-\(verb)"
            $0.pendingId = id
            $0.notice = notice
        }
    }

    private func removeImpact(_ id: String) -> (unchecked: Bool, affected: [String], force: Bool) {
        guard let payload = installedPayload(),
              let instance = instancesById(payload)[id] else {
            return (true, [], true)
        }
        let report = ServiceBoard.classifyAttachments(
            ServiceBoard.attachmentRoots(instance), current: nil, known: knownRoots())
        return (false, report.all, !report.all.isEmpty)
    }

    private func installedPayload() -> [String: Any]? {
        let result = hearth.run(["shared", "installed", "--json"], cwd: "/", timeout: 20)
        return result.ok ? result.json : nil
    }

    private func loadShared() {
        let list = hearth.run(["shared", "list", "--json"], cwd: "/", timeout: 20)
        let status = hearth.run(["shared", "status", "--json"], cwd: "/", timeout: 20)
        if status.ok, let json = status.json {
            mutate {
                $0.smpLive = true
                $0.instances = ServiceBoard.instancesFrom(json)
            }
        } else {
            let installed = hearth.run(["shared", "installed", "--json"], cwd: "/", timeout: 20)
            mutate {
                $0.smpLive = false
                $0.instances = installed.ok && installed.json != nil
                    ? ServiceBoard.instancesFrom(installed.json!)
                    : []
                if $0.notice == nil || $0.notice == "" {
                    $0.notice = status.visibleMessage.isEmpty
                        ? "smp is not running. Showing the local registry."
                        : status.visibleMessage
                }
            }
        }
        mutate {
            $0.recipes = list.json.map { ServiceBoard.recipesFrom($0) } ?? []
        }
    }

    private func instancesById(_ payload: [String: Any]) -> [String: [String: Any]] {
        var byId: [String: [String: Any]] = [:]
        for instance in payload["instances"] as? [[String: Any]] ?? [] {
            byId[ServiceBoard.instanceId(instance)] = instance
        }
        return byId
    }

    private func knownRoots() -> [ServiceBoard.WorkspaceLabel] {
        store.rows.map {
            ServiceBoard.WorkspaceLabel(
                root: $0.path,
                name: WorkspaceStore.folderName($0.path),
                path: WorkspaceStore.displayPath($0.path)
            )
        }
    }

    // MARK: - small helpers

    private func missingNotice(_ row: WorkspaceRecord) -> String {
        "\(WorkspaceStore.folderName(row.path)) is missing on disk. Forget removes it."
    }

    private func armed(_ kind: String, _ id: String) -> Bool {
        state.pendingKind == kind && state.pendingId == id
    }

    private func keepArmed(_ kind: String, _ id: String) {
        if !armed(kind, id) {
            mutate { $0.pendingKind = nil; $0.pendingId = nil }
        }
    }

    private func resetBoard() {
        mutate {
            $0.sections = []
            $0.summary = ""
            $0.urls = []
            $0.catalogGroups = [:]
            $0.logText = ""
            $0.logCursor = nil
            $0.logGeneration = nil
            $0.serviceGeneration = nil
            $0.logLimit = 16384
            $0.logHasMore = false
            $0.catalogMtime = nil
            $0.selectedService = "$daemon"
        }
    }

    private func doneWord(_ action: String) -> String {
        switch action {
        case "stop": return "Stopped"
        case "restart": return "Restarted"
        default: return "Started"
        }
    }
}
