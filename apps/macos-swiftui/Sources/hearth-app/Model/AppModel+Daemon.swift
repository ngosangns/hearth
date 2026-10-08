import SwiftUI
import HearthKit

/// Workspace and daemon lifecycle: discover, trust, start, stop, restart, forget, and the board
/// refresh that follows each of them.
extension AppModel {

    // MARK: Discover

    /// Re-read the daemon status for the selected workspace. Never runs `manager ensure`.
    func discover() async {
        guard let cli, let row = selectedRecord else { phase = .idle; return }
        guard isDirectory(row.path) else {
            resetBoard()
            phase = .missingFolder
            return
        }
        if !phase.isAttached { phase = .checking }
        let result = await cli.manager(root: row.path, "status")
        guard selectedId == row.id, !Task.isCancelled else { return }
        if !result.ok { sessions[row.path] = nil }
        syncRows()

        if stopped.contains(row.id) {
            resetBoard()
            phase = .stopped
            return
        }
        guard result.ok, let info = result.decode(DaemonInfo.self) else {
            resetBoard()
            let message = result.visibleMessage
            phase = .down(message: message.isEmpty || message == "hearth manager is unavailable" ? nil : message)
            return
        }
        guard sessions[row.path] != nil else {
            resetBoard()
            phase = .detached(port: info.port, proto: info.protocolVersion)
            return
        }
        phase = .attached(port: info.port, proto: info.protocolVersion)
        await loadBoard()
    }

    // MARK: Board

    func loadBoard() async {
        guard let row = selectedRecord, let session = sessions[row.path] else { return }
        let id = row.id
        await reloadCatalogIfChanged(root: row.path)
        boardSeq += 1
        let seq = boardSeq
        let client = ManagerClient(session: session)
        do {
            async let live = client.services()
            async let catalog = Self.soft { try await client.catalog() }
            async let urls = Self.soft { try await client.urls() }
            let (liveRows, catalogDoc, urlRows) = try await (live, catalog, urls)
            guard seq == boardSeq, selectedId == id else { return }
            let doc = catalogDoc ?? Catalog()
            groupCatalog = doc
            let built = ServiceBoard.sections(catalog: doc, live: liveRows)
            sections = built
            summary = ServiceBoard.summary(built)
            self.urls = ServiceBoard.visibleUrls(urlRows ?? [], sections: built)
            if let selected = selectedService, line(selected) == nil {
                selectedService = nil
                log.reset()
                logEpoch += 1
                logLoading = false
            }
            log.observe(liveGeneration: liveRows.first { $0.serviceId == selectedService }?.generation)
            if logOpen { await loadLog() }
        } catch ManagerError.sessionEnded {
            endSession(root: row.path)
        } catch is CancellationError {
        } catch {
            // A transient failure keeps the last board; the next tick retries.
        }
    }

    /// Network failures on optional reads degrade to nil; a refused token still surfaces.
    nonisolated static func soft<T: Sendable>(_ body: @Sendable () async throws -> T) async throws -> T? {
        do { return try await body() }
        catch ManagerError.sessionEnded { throw ManagerError.sessionEnded }
        catch is CancellationError { throw CancellationError() }
        catch { return nil }
    }

    func endSession(root: String) {
        sessions[root] = nil
        syncRows()
        if selectedRecord?.path == root {
            resetBoard()
            phase = .sessionEnded
        }
    }

    private func reloadCatalogIfChanged(root: String) async {
        let next = ServiceBoard.catalogStamp(root: root)
        if ServiceBoard.shouldReloadCatalog(previous: catalogStamp, next: next), let cli {
            let result = await cli.manager(root: root, "reload")
            if !result.ok {
                let message = result.visibleMessage
                notify(message.isEmpty ? "Catalog reload failed. The running catalog stays." : message, .error)
            }
        }
        if next != nil { catalogStamp = next }
    }

    // MARK: Log

    func loadLog() async {
        await fetchLog(earlier: false)
    }

    /// One page before the bytes already on screen. The tail stays put; the pane shifts its scroll
    /// anchor by `log.prepended`. A second call while a fetch is in flight is a no-op.
    func loadEarlier() {
        guard !logLoading, log.hasMore, log.nextWindowLimit() != nil else { return }
        logLoading = true
        Task { await fetchLog(earlier: true, held: true) }
    }

    private func fetchLog(earlier: Bool, held: Bool = false) async {
        guard let service = selectedService, let row = selectedRecord, let session = sessions[row.path] else {
            if held { logLoading = false }
            return
        }
        let limit: Int
        let cursor: Int?
        if earlier {
            guard let next = log.nextWindowLimit() else {
                if held { logLoading = false }
                return
            }
            limit = next
            cursor = nil
        } else {
            guard !logLoading else { return }
            limit = log.limit
            cursor = log.cursor
            logLoading = true
        }
        let epoch = logEpoch
        let generation = log.generation
        defer { if epoch == logEpoch { logLoading = false } }
        let client = ManagerClient(session: session)
        do {
            let slice = try await client.serviceLog(service, cursor: cursor, generation: generation, limit: limit)
            guard epoch == logEpoch else { return }
            if earlier || cursor == nil {
                log.install(slice)
            } else {
                log.apply(slice)
            }
        } catch ManagerError.sessionEnded {
            endSession(root: row.path)
        } catch {
            // Keep the text already shown.
        }
    }

    func toggleLog() {
        logOpen.toggle()
        if logOpen { Task { await loadLog() } }
    }

    // MARK: Workspace list

    func addFolder(_ input: String) {
        do {
            let added = try store.add(input)
            syncRows()
            if added.created { notify("Added \(WorkspaceStore.folderName(added.record.path)). Trust it to start its daemon.", .info) }
            pane = .workspaces
            selectedId = nil
            selectWorkspace(added.record.id)
        } catch {
            notify(error.localizedDescription, .error)
        }
    }

    func chooseFolder() {
        let panel = NSOpenPanel()
        panel.canChooseDirectories = true
        panel.canChooseFiles = false
        panel.allowsMultipleSelection = false
        panel.prompt = "Add Workspace"
        panel.message = "Choose a project folder that has a hearth.yaml."
        if panel.runModal() == .OK, let url = panel.url { addFolder(url.path) }
    }

    /// Re-read the list file and discover the selection. Never runs `manager ensure`.
    func refresh() {
        if let error = store.reload() { notify(error, .error) }
        if let id = selectedId, store.get(id) == nil { selectedId = store.rows.first?.id }
        syncRows()
        discoverTask?.cancel()
        discoverTask = Task {
            let op = begin("Refreshing")
            defer { end(op) }
            await discover()
            if pane == .shared { await loadSharedNow() }
        }
    }

    // MARK: Daemon actions

    func requestTrust() {
        guard let item = selectedItem, !item.missing else { return }
        if item.trusted { startDaemon(); return }
        confirmation = Confirmation(
            title: "Trust \(item.name)?",
            message: "\(item.displayPath) will start a daemon that runs the commands in its hearth.yaml. Only trust a folder you wrote or reviewed.",
            confirmTitle: "Trust and Start", destructive: false
        ) { [weak self] in self?.trust() }
    }

    private func trust() {
        guard let row = selectedRecord else { return }
        _ = store.reload()
        do { try store.trust(row.id) } catch { notify(error.localizedDescription, .error); return }
        stopped.remove(row.id)
        syncRows()
        Task { await ensure(row.id) }
    }

    func startDaemon() {
        guard let item = selectedItem, !item.missing else { return }
        guard item.trusted else { notify("Trust the folder before starting its daemon.", .error); return }
        stopped.remove(item.id)
        syncRows()
        Task { await ensure(item.id) }
    }

    private func ensure(_ id: String) async {
        guard let cli, let row = store.get(id) else { return }
        let op = begin("Starting daemon · \(WorkspaceStore.folderName(row.path))")
        defer { end(op) }
        phase = .checking
        let result = await cli.manager(root: row.path, "ensure")
        guard let info = result.decode(DaemonInfo.self), result.ok, let session = Session(info) else {
            let message = result.visibleMessage
            notify(message.isEmpty ? "manager ensure failed" : message, .error)
            if selectedId == id { phase = .down(message: message.isEmpty ? nil : message) }
            return
        }
        sessions[row.path] = session
        syncRows()
        let healthy = await ManagerClient(session: session).healthz()
        notify("Daemon is up on port \(session.port)." + (healthy ? "" : " healthz did not answer."), healthy ? .success : .error)
        guard selectedId == id else { return }
        phase = .attached(port: session.port, proto: session.protocolVersion)
        await loadBoard()
    }

    func requestStopDaemon() {
        guard let item = selectedItem else { return }
        confirmation = Confirmation(
            title: "Stop the daemon for \(item.name)?",
            message: "Its services stop. Start runs the daemon again.",
            confirmTitle: "Stop Daemon", destructive: true
        ) { [weak self] in self?.stopDaemon() }
    }

    private func stopDaemon() {
        guard let cli, let row = selectedRecord else { return }
        stopped.insert(row.id)
        sessions[row.path] = nil
        syncRows()
        resetBoard()
        phase = .stopped
        Task {
            let op = begin("Stopping daemon · \(WorkspaceStore.folderName(row.path))")
            defer { end(op) }
            let result = await cli.manager(root: row.path, "stop")
            if result.ok {
                notify("Daemon stopped. Start runs it again.", .success)
            } else {
                let message = result.visibleMessage
                notify(message.isEmpty ? "stop daemon failed" : message, .error)
            }
        }
    }

    func requestRestartDaemon() {
        guard let item = selectedItem else { return }
        confirmation = Confirmation(
            title: "Restart the daemon for \(item.name)?",
            message: "Its services keep running.",
            confirmTitle: "Restart Daemon", destructive: false
        ) { [weak self] in self?.restartDaemon() }
    }

    private func restartDaemon() {
        guard let cli, let row = selectedRecord else { return }
        let id = row.id
        Task {
            let op = begin("Restarting daemon · \(WorkspaceStore.folderName(row.path))")
            defer { end(op) }
            let result = await cli.manager(root: row.path, "restart")
            guard result.ok, let info = result.decode(DaemonInfo.self), let session = Session(info) else {
                let message = result.visibleMessage
                notify(message.isEmpty ? "restart daemon failed" : message, .error)
                return
            }
            sessions[row.path] = session
            stopped.remove(id)
            syncRows()
            notify("Daemon is up on port \(session.port).", .success)
            guard selectedId == id else { return }
            log.reset()
            logEpoch += 1
            logLoading = false
            phase = .attached(port: session.port, proto: session.protocolVersion)
            await loadBoard()
        }
    }

    func requestForget() {
        guard let item = selectedItem else { return }
        confirmation = Confirmation(
            title: "Forget \(item.name)?",
            message: "The folder leaves this list. Its services keep running.",
            confirmTitle: "Forget", destructive: true
        ) { [weak self] in self?.forget() }
    }

    private func forget() {
        guard let row = selectedRecord else { return }
        _ = store.reload()
        do { try store.remove(row.id) } catch { notify(error.localizedDescription, .error); return }
        stopped.remove(row.id)
        sessions[row.path] = nil
        syncRows()
        resetBoard()
        selectedId = nil
        selectWorkspace(store.rows.first?.id)
        notify("Forgot \(WorkspaceStore.folderName(row.path)). Its services keep running.", .info)
    }
}
