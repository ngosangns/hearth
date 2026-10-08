import SwiftUI
import HearthKit

/// The Shared pane: recipes from the remote catalog and instances from a live smp, or from the
/// local registry when smp is down. Drawing the pane never starts smp.
extension AppModel {

    func loadShared() {
        Task {
            let op = begin("Loading shared services")
            defer { end(op) }
            await loadSharedNow()
        }
    }

    func loadSharedNow() async {
        guard let cli else { return }
        async let list = cli.run(["shared", "list", "--json"], cwd: "/", timeout: .seconds(20))
        async let status = cli.run(["shared", "status", "--json"], cwd: "/", timeout: .seconds(20))
        let (listResult, statusResult) = await (list, status)
        if statusResult.ok, let live = statusResult.decode(SharedInstances.self) {
            smpLive = true
            instances = live.instances
        } else {
            smpLive = false
            instances = await installedInstances() ?? []
        }
        recipes = listResult.decode(SharedCatalog.self)?.recipes ?? []
        sharedLoaded = true
        for instance in instances { sharedInfo[instance.id] = instance }
        keepSharedSelectionValid()
    }

    private func keepSharedSelectionValid() {
        switch sharedSelection {
        case .instance(let id) where instances.contains { $0.id == id }: return
        case .recipe(let id) where recipes.contains { $0.id == id }: return
        default: break
        }
        sharedSelection = instances.first.map { .instance($0.id) } ?? recipes.first.map { .recipe($0.id) }
    }

    /// Loads the local registry so a popover can list who uses a shared instance.
    func loadSharedInfo() async {
        guard let rows = await installedInstances() else { sharedInfoFailed = true; return }
        sharedInfoFailed = false
        for row in rows { sharedInfo[row.id] = row }
    }

    // MARK: Actions

    func installRecipe(_ id: String) {
        sharedCommand("Installing \(id)", ["shared", "install", id, "--json"], timeout: nil, success: "Installed \(id).")
    }

    func startInstance(_ id: String) {
        sharedCommand("Starting \(id)", ["shared", "start", id, "--json"], timeout: nil, success: "Started \(id).")
    }

    func requestStopInstance(_ id: String) { requestInstance(.stop, id) }
    func requestRestartInstance(_ id: String) { requestInstance(.restart, id) }

    func requestRemoveInstance(_ id: String) {
        Task {
            let known = await installedInstances()
            let instance = known?.first { $0.id == id }
            let report = instance.map {
                ServiceBoard.classifyAttachments($0.attachmentRoots, current: nil, known: knownLabels())
            }
            let notice = ServiceBoard.removeNotice(id, affected: report?.all ?? [], unchecked: instance == nil)
            // An unreadable registry is treated like an attached instance: force is the explicit override.
            let force = instance == nil || !(report?.all.isEmpty ?? true)
            confirmation = Confirmation(
                title: "Remove \(id)?", message: notice, confirmTitle: "Remove", destructive: true
            ) { [weak self] in
                self?.sharedCommand("Removing \(id)", ["shared", "remove", id, "--json"] + (force ? ["--force"] : []),
                                    timeout: .seconds(120), success: "Removed \(id).")
            }
        }
    }

    private func requestInstance(_ verb: Verb, _ id: String) {
        Task {
            let known = await installedInstances()
            let notice: String?
            if let instance = known?.first(where: { $0.id == id }) {
                let report = ServiceBoard.classifyAttachments(
                    instance.attachmentRoots, current: selectedRecord?.path, known: knownLabels())
                notice = report.others.isEmpty ? nil
                    : ServiceBoard.instanceSharedNotice(verb.rawValue, instance: id, affected: report.all)
            } else {
                notice = ServiceBoard.uncheckedSharedNotice(instance: id)
            }
            if let notice {
                confirmation = Confirmation(
                    title: "\(verb.gerund) \(id)?", message: notice,
                    confirmTitle: verb == .stop ? "Stop" : "Restart", destructive: verb == .stop
                ) { [weak self] in self?.runInstance(verb, id) }
            } else {
                runInstance(verb, id)
            }
        }
    }

    private func runInstance(_ verb: Verb, _ id: String) {
        guard let cli else { return }
        Task {
            let op = begin("\(verb.gerund) \(id)")
            defer { end(op) }
            let stop = await cli.run(["shared", "stop", id, "--json"], cwd: "/", timeout: .seconds(120))
            guard stop.ok else { failure(stop, "stop failed"); await loadSharedNow(); return }
            if verb == .restart {
                let start = await cli.run(["shared", "start", id, "--json"], cwd: "/", timeout: nil)
                guard start.ok else { failure(start, "start failed"); await loadSharedNow(); return }
            }
            notify("\(verb.past) \(id).", .success)
            await loadSharedNow()
        }
    }

    private func sharedCommand(_ title: String, _ args: [String], timeout: Duration?, success: String) {
        guard let cli else { return }
        Task {
            let op = begin(title)
            defer { end(op) }
            let result = await cli.run(args, cwd: "/", timeout: timeout)
            if result.ok { notify(success, .success) } else { failure(result, "\(args.dropFirst().first ?? "command") failed") }
            await loadSharedNow()
        }
    }

    private func failure(_ result: CommandResult, _ fallback: String) {
        let message = result.visibleMessage
        notify(message.isEmpty ? fallback : message, .error)
    }
}
