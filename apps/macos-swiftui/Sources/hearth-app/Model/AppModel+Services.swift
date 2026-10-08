import SwiftUI
import HearthKit

/// Service, group and bulk actions against the attached daemon.
extension AppModel {

    enum Verb: String {
        case start, stop, restart
        var past: String { switch self { case .start: "Started"; case .stop: "Stopped"; case .restart: "Restarted" } }
        var gerund: String { switch self { case .start: "Starting"; case .stop: "Stopping"; case .restart: "Restarting" } }
    }

    // MARK: Single service

    func start(_ id: String) { requestService(.start, [id], label: id) }
    func stop(_ id: String) { requestService(.stop, [id], label: id) }
    func restart(_ id: String) { requestService(.restart, [id], label: id) }

    func requestReclaim(_ id: String) {
        guard line(id)?.state == "externally-owned" else { return }
        confirmation = Confirmation(
            title: "Reclaim the port for \(id)?",
            message: "Another process holds this service's port. Reclaiming signals that process (SIGTERM, then SIGKILL) and starts \(id).",
            confirmTitle: "Kill and Start", destructive: true
        ) { [weak self] in self?.run(.start, [id], label: id, killUnowned: true) }
    }

    // MARK: Bulk and groups

    func startAll() {
        let ids = ServiceBoard.startAllTargets(groups: groupCatalog.groups, sections: sections)
            .filter { line($0)?.disabled != true }
        requestService(.start, ids, label: "all services")
    }

    func stopAll() {
        requestService(.stop, ServiceBoard.stopAllTargets(sections), label: "all services")
    }

    func startGroup(_ name: String) {
        requestService(.start, ServiceBoard.groupTargets(sections, name: name), label: name)
    }

    func stopGroup(_ name: String) {
        let ids = ServiceBoard.groupTargets(sections, name: name).filter {
            guard let row = line($0) else { return false }
            return !["stopped", "succeeded"].contains(row.state)
        }
        requestService(.stop, ids, label: name)
    }

    func restartGroup(_ name: String) {
        requestService(.restart, ServiceBoard.groupTargets(sections, name: name), label: name)
    }

    // MARK: Flow

    /// A stop or restart that reaches a shared service another workspace uses asks first;
    /// everything else runs at once.
    private func requestService(_ verb: Verb, _ ids: [String], label: String) {
        guard phase.isAttached else { return }
        guard !ids.isEmpty else {
            notify(verb == .stop ? "Nothing to stop." : "Nothing to start.", .info)
            return
        }
        guard verb != .start else { run(verb, ids, label: label); return }
        Task {
            if let notice = await sharedImpactNotice(verb, ids) {
                confirmation = Confirmation(
                    title: "\(verb.gerund) \(label)?", message: notice,
                    confirmTitle: verb == .stop ? "Stop" : "Restart", destructive: verb == .stop
                ) { [weak self] in self?.run(verb, ids, label: label) }
            } else {
                run(verb, ids, label: label)
            }
        }
    }

    private func run(_ verb: Verb, _ ids: [String], label: String, killUnowned: Bool = false) {
        guard let row = selectedRecord, let session = sessions[row.path] else { return }
        let client = ManagerClient(session: session)
        let root = row.path
        Task {
            let op = begin("\(verb.gerund) \(label)")
            defer { end(op) }
            var failures: [(id: String, message: String)] = []
            var ended = false
            await withTaskGroup(of: (String, Error?).self) { group in
                for id in ids {
                    group.addTask {
                        do { try await client.perform(serviceId: id, action: verb.rawValue, killUnowned: killUnowned); return (id, nil) }
                        catch { return (id, error) }
                    }
                }
                for await (id, error) in group {
                    guard let error else { continue }
                    if case ManagerError.sessionEnded = error { ended = true } else {
                        failures.append((id, error.localizedDescription))
                    }
                }
            }
            if ended { endSession(root: root); return }
            if selectedRecord?.path == root { await loadBoard() }
            if failures.isEmpty {
                notify("\(verb.past) \(label).", .success)
            } else if failures.count == 1, ids.count == 1 {
                notify("\(failures[0].id): \(failures[0].message)", .error)
            } else {
                let names = ServiceBoard.joinNames(failures.map(\.id).sorted())
                notify("Failed: \(names). \(failures[0].message)", .error)
            }
        }
    }

    // MARK: Shared impact

    /// nil = nothing shared is affected; otherwise the sentence for the confirmation.
    private func sharedImpactNotice(_ verb: Verb, _ ids: [String]) async -> String? {
        let instanceIds = Set(ids.compactMap { line($0)?.sharedInstance })
        guard !instanceIds.isEmpty else { return nil }
        let currentRoot = selectedRecord?.path
        let currentName = currentRoot.map(WorkspaceStore.folderName) ?? "this workspace"
        guard let known = await installedInstances() else {
            return ServiceBoard.uncheckedSharedNotice(instance: ServiceBoard.joinNames(instanceIds.sorted()))
        }
        let byId = Dictionary(known.map { ($0.id, $0) }, uniquingKeysWith: { $1 })
        var touches: [ServiceBoard.Touch] = []
        var unknown: [String] = []
        for id in instanceIds.sorted() {
            guard let instance = byId[id] else { unknown.append(id); continue }
            let report = ServiceBoard.classifyAttachments(instance.attachmentRoots, current: currentRoot, known: knownLabels())
            if !report.others.isEmpty { touches.append(.init(instance: id, others: report.others)) }
        }
        if touches.isEmpty && unknown.isEmpty { return nil }
        if touches.isEmpty { return ServiceBoard.uncheckedSharedNotice(instance: ServiceBoard.joinNames(unknown)) }
        return ServiceBoard.projectSharedNotice(verb.rawValue, current: currentName, touches: touches, unknown: unknown)
    }

    func knownLabels() -> [ServiceBoard.WorkspaceLabel] {
        store.rows.map { .init(root: $0.path, name: WorkspaceStore.folderName($0.path)) }
    }

    /// `hearth shared installed --json`: the local registry, readable without smp running.
    func installedInstances() async -> [SharedInstance]? {
        guard let cli else { return nil }
        let result = await cli.run(["shared", "installed", "--json"], cwd: "/", timeout: .seconds(20))
        guard result.ok else { return nil }
        return result.decode(SharedInstances.self)?.instances
    }
}
