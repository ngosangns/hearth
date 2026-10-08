import Foundation

/// Pure service-board rules: grouping, state wording, bulk targets, URL visibility, shared
/// impact notices. Ported from the old `hearth tui` desk so the window matches it.
public enum ServiceBoard {

    public struct Line: Sendable, Equatable, Identifiable {
        public let id: String
        public let label: String
        public let ports: String
        public let state: String
        public let disabled: Bool
        public let finite: Bool
        public let infra: Bool
        public let sharedInstance: String?
        public let error: String?

        public var display: String { ServiceBoard.displayState(state) }
        public var up: Bool { ServiceBoard.isUp(state) }
        public var shared: Bool { sharedInstance != nil }
    }

    public struct Section: Sendable, Equatable, Identifiable {
        public let name: String?
        public var services: [Line]
        public var id: String { name ?? "$other" }
    }

    public struct VisibleURL: Sendable, Equatable, Identifiable {
        public let serviceId: String
        public let label: String
        public let url: String
        public var id: String { "\(serviceId)|\(url)" }
    }

    // MARK: State wording

    /// `running-unready` is the only state whose label differs from the wire string.
    public static func displayState(_ wire: String) -> String {
        wire == "running-unready" ? "degraded" : wire
    }

    public static func isUp(_ state: String) -> Bool {
        ["ready", "running", "running-unready"].contains(state)
    }

    /// Stop is the primary action while a start is still in flight. `succeeded` is not up.
    public static func showsStop(_ state: String) -> Bool {
        isUp(state) || ["queued-start", "starting", "preparing", "stopping"].contains(state)
    }

    /// Short wire label used for shared instances.
    public static func compactWire(_ state: String) -> String {
        switch state {
        case "running-unready": "running"
        case "preparing": "starting"
        case "queued-start": "queued"
        case "externally-owned": "external"
        default: state
        }
    }

    // MARK: Sections

    /// Rows follow catalog order. With a `groupTree` a service sits under the first group that
    /// names it; the rest go to a trailing unnamed section. Empty groups are dropped.
    public static func sections(catalog: Catalog, live: [LiveService]) -> [Section] {
        var byId: [String: LiveService] = [:]
        for row in live where !row.serviceId.isEmpty { byId[row.serviceId] = row }

        var lines: [Line] = []
        var seen: Set<String> = []
        for service in catalog.services where !service.id.isEmpty {
            lines.append(line(service, live: byId[service.id]))
            seen.insert(service.id)
        }
        // A live row the catalog no longer names (mid-reload) still shows.
        for id in byId.keys.sorted() where !seen.contains(id) {
            lines.append(line(CatalogService(id: id), live: byId[id]))
        }

        guard !catalog.groupTree.isEmpty else {
            return [Section(name: nil, services: lines)]
        }

        var firstGroup: [String: String] = [:]
        var built: [Section] = []
        for group in catalog.groupTree {
            built.append(Section(name: group.name, services: []))
            for member in group.members where firstGroup[member] == nil { firstGroup[member] = group.name }
        }
        var rest: [Line] = []
        for line in lines {
            if let name = firstGroup[line.id], let index = built.firstIndex(where: { $0.name == name }) {
                built[index].services.append(line)
            } else {
                rest.append(line)
            }
        }
        built.removeAll { $0.services.isEmpty }
        if !rest.isEmpty { built.append(Section(name: nil, services: rest)) }
        return built.isEmpty ? [Section(name: nil, services: [])] : built
    }

    private static func line(_ service: CatalogService, live: LiveService?) -> Line {
        let label = service.label.flatMap { $0.isEmpty ? nil : $0 } ?? service.id
        let error = live?.error.flatMap { $0.isEmpty ? nil : $0 }
        return Line(
            id: service.id,
            label: label,
            ports: service.ports.map(String.init).joined(separator: ", "),
            state: live?.actualState ?? "stopped",
            disabled: service.disabled,
            finite: service.readinessKind == "exit",
            infra: service.kind == "infrastructure",
            sharedInstance: service.sharedInstance,
            error: error
        )
    }

    public static func lines(_ sections: [Section]) -> [Line] { sections.flatMap(\.services) }

    // MARK: Summary and targets

    /// Finite services stay out of the totals unless they failed.
    public static func summary(_ sections: [Section]) -> String {
        var ready = 0, failed = 0, total = 0
        for service in lines(sections) {
            if service.finite && service.state != "failed" { continue }
            total += 1
            if service.up { ready += 1 } else if service.state == "failed" { failed += 1 }
        }
        if total == 0 { return "" }
        return failed > 0 ? "\(ready)/\(total) ready  \(failed) failed" : "\(ready)/\(total) ready"
    }

    public static func stopAllTargets(_ sections: [Section]) -> [String] {
        lines(sections).compactMap { line in
            line.disabled || ["stopped", "succeeded"].contains(line.state) ? nil : line.id
        }
    }

    public static func groupTargets(_ sections: [Section], name: String) -> [String] {
        sections.filter { $0.name == name }.flatMap(\.services).filter { !$0.disabled }.map(\.id)
    }

    public static func groupIsUp(_ sections: [Section], name: String) -> Bool {
        let longLived = sections.filter { $0.name == name }.flatMap(\.services).filter { !$0.disabled && !$0.finite }
        return !longLived.isEmpty && longLived.allSatisfy(\.up)
    }

    public static func startAllTargets(groups: [String: [String]], sections: [Section]) -> [String] {
        if let all = groups["all"], !all.isEmpty { return all }
        return lines(sections).filter { !$0.disabled }.map(\.id)
    }

    // MARK: URLs

    /// A finished `readiness: exit` row has no process by design, so its link stays visible.
    public static func urlVisible(requiresRunning: Bool, state: String) -> Bool {
        !requiresRunning || isUp(state) || state == "succeeded"
    }

    public static func visibleUrls(_ urls: [UrlRow], sections: [Section]) -> [VisibleURL] {
        var states: [String: String] = [:]
        for line in lines(sections) { states[line.id] = line.state }
        return urls.compactMap { row in
            guard urlVisible(requiresRunning: row.requiresRunning ?? true, state: states[row.serviceId] ?? "stopped") else { return nil }
            let label = row.label.flatMap { $0.isEmpty ? nil : $0 } ?? row.serviceId
            return VisibleURL(serviceId: row.serviceId, label: label, url: row.url)
        }
    }

    // MARK: Catalog reload

    /// The first observation only records the stamp; a change on a later one reloads.
    public static func shouldReloadCatalog(previous: Int?, next: Int?) -> Bool {
        previous != nil && next != nil && previous != next
    }

    public static func catalogStamp(root: String) -> Int? {
        ["hearth.yaml", "hearth.yml", "hearth.json"].compactMap { name in
            (try? FileManager.default.attributesOfItem(atPath: root + "/" + name))?[.modificationDate] as? Date
        }.map { Int($0.timeIntervalSince1970) }.max()
    }

    // MARK: Log

    /// `limit` counts characters, not bytes.
    public static func boundedTail(_ text: String, limit: Int) -> String {
        guard limit >= 1 else { return "" }
        return text.count <= limit ? text : String(text.suffix(limit))
    }

    // MARK: Shared-service impact notices

    public struct WorkspaceLabel: Sendable, Equatable {
        public let root: String
        public let name: String
        public init(root: String, name: String) {
            self.root = root
            self.name = name
        }
    }

    public struct Touch: Sendable, Equatable {
        public let instance: String
        public let others: [String]
        public init(instance: String, others: [String]) {
            self.instance = instance
            self.others = others
        }
    }

    /// `all` = every attachment label; `others` = attachments outside `current`.
    public static func classifyAttachments(_ roots: [String], current: String?, known: [WorkspaceLabel]) -> (all: [String], others: [String]) {
        let others = roots.filter { root in current.map { !sameRoot(root, $0) } ?? true }
        return (labels(roots, known: known), labels(others, known: known))
    }

    public static func instanceSharedNotice(_ verb: String, instance: String, affected: [String]) -> String {
        let who = joinNames(affected)
        let (doing, consequence): (String, String) = switch verb {
        case "restart": ("Restarting", "takes the shared service down for \(who)")
        case "remove": ("Removing", "deletes its data and takes it down for \(who)")
        default: ("Stopping", "takes the shared service down for \(who)")
        }
        return "\(doing) \(instance) \(consequence)."
    }

    /// Notice shown before a confirmation. Stopping from a project only detaches that workspace.
    public static func projectSharedNotice(_ verb: String, current: String, touches: [Touch], unknown: [String]) -> String {
        var sentences: [String] = []
        if !touches.isEmpty {
            let listed = joinNames(touches.map { "\($0.instance) (\(joinNames($0.others)))" })
            let be = touches.count == 1 ? "is" : "are"
            let them = touches.count == 1 ? "it" : "them"
            let doing = verb == "restart" ? "Restarting" : "Stopping"
            let effect = verb == "restart" ? "only detaches and reattaches \(current)" : "only detaches \(current)"
            sentences.append("\(listed) \(be) also used by other workspaces. \(doing) \(them) here \(effect). Those workspaces keep \(them)")
        }
        if !unknown.isEmpty {
            sentences.append("Could not check which workspaces use \(joinNames(unknown))")
        }
        return sentences.joined(separator: ". ") + "."
    }

    public static func uncheckedSharedNotice(instance: String) -> String {
        "Could not check which workspaces use \(instance)."
    }

    public static func removeNotice(_ id: String, affected: [String], unchecked: Bool) -> String {
        if unchecked { return uncheckedSharedNotice(instance: id) + " Removing it deletes its data." }
        if affected.isEmpty { return "Removing \(id) deletes its data." }
        return instanceSharedNotice("remove", instance: id, affected: affected)
    }

    public static func joinNames(_ names: [String]) -> String {
        switch names.count {
        case 0: ""
        case 1: names[0]
        case 2: "\(names[0]) and \(names[1])"
        default: names.dropLast().joined(separator: ", ") + ", and \(names[names.count - 1])"
        }
    }

    public static func sameRoot(_ left: String, _ right: String) -> Bool {
        (left as NSString).resolvingSymlinksInPath == (right as NSString).resolvingSymlinksInPath
    }

    private static func labels(_ roots: [String], known: [WorkspaceLabel]) -> [String] {
        roots.map { root in
            known.first(where: { sameRoot($0.root, root) })?.name ?? WorkspaceStore.folderName(root)
        }
    }
}
