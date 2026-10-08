import Foundation

/// Pure desk rules ported from the PHP `ServiceBoard` (itself ported from the TUI).
/// Wire strings stay wire strings. `running-unready` is the only state whose
/// label changes (`degraded`).
enum ServiceBoard {

    struct ServiceLine {
        let id: String
        let label: String
        let ports: String
        let state: String
        let display: String
        let disabled: Bool
        let finite: Bool
        let infra: Bool
        let shared: Bool
        let sharedInstance: String?
        let error: String?
        let up: Bool
    }

    struct ServiceSection {
        let name: String?
        var services: [ServiceLine]
    }

    struct Recipe: Hashable {
        let id: String
        let name: String
        let version: String
    }

    struct Instance {
        let id: String
        let port: Any?
        let installState: String
        let state: String
        let display: String
        let attachments: Int
    }

    struct VisibleURL {
        let serviceId: String
        let label: String
        let url: String
    }

    struct WorkspaceLabel {
        let root: String
        let name: String
        let path: String
    }

    static func displayState(_ wire: String) -> String {
        wire == "running-unready" ? "degraded" : wire
    }

    static func isUp(_ state: String) -> Bool {
        ["ready", "running", "running-unready"].contains(state)
    }

    /// Stop is the primary action while a start is still in flight. Succeeded is not up.
    static func showsStop(_ state: String) -> Bool {
        isUp(state) || ["queued-start", "starting", "preparing", "stopping"].contains(state)
    }

    static func boundedTail(_ text: String, limit: Int) -> String {
        guard limit >= 1 else { return "" }
        return text.count <= limit ? text : String(text.suffix(limit))
    }

    static func sharedInstanceOf(_ service: [String: Any]) -> String? {
        guard let profile = service["profiles"] as? [String: Any],
              let run = profile["run"] as? [String: Any],
              run["commandStatus"] as? String == "verified",
              let command = (run["command"] as? [String: Any])?["command"] as? [String: Any],
              let argv = command["argv"] as? [Any]
        else { return nil }
        for index in 0..<argv.count where index + 1 < argv.count {
            guard argv[index] as? String == "shared", argv[index + 1] as? String == "attach" else { continue }
            guard let id = argv[index + 2] as? String else { return nil }
            if id.contains("@") && !id.hasPrefix("-") { return id }
            return nil
        }
        return nil
    }

    static func isFinite(_ service: [String: Any]) -> Bool {
        let run = (service["profiles"] as? [String: Any])?["run"] as? [String: Any]
        let kind = (run?["readiness"] as? [String: Any])?["kind"] as? String
        return kind == "exit"
    }

    static func sections(catalog: [String: Any], live: [[String: Any]]) -> [ServiceSection] {
        var byId: [String: [String: Any]] = [:]
        for row in live {
            if let id = row["serviceId"] as? String, !id.isEmpty {
                byId[id] = row
            }
        }

        var metas: [[String: Any]] = []
        var seen: Set<String> = []
        for item in catalog["services"] as? [[String: Any]] ?? [] {
            guard let id = item["id"] as? String, !id.isEmpty else { continue }
            metas.append(meta(item))
            seen.insert(id)
        }
        for (id, _) in byId where !seen.contains(id) {
            metas.append([
                "id": id, "label": id, "ports": "",
                "disabled": false, "finite": false, "infra": false,
                "shared": false, "sharedInstance": NSNull(),
            ])
        }

        let lineFor: ([String: Any]) -> ServiceLine = { meta in
            let id = meta["id"] as? String ?? ""
            let found = byId[id]
            let state = found?["actualState"] as? String ?? "stopped"
            let err = found?["error"] as? String
            let label = meta["label"] as? String ?? ""
            let sharedInstance = meta["sharedInstance"] as? String
            return ServiceLine(
                id: id,
                label: label.isEmpty ? id : label,
                ports: meta["ports"] as? String ?? "",
                state: state,
                display: displayState(state),
                disabled: meta["disabled"] as? Bool ?? false,
                finite: meta["finite"] as? Bool ?? false,
                infra: meta["infra"] as? Bool ?? false,
                shared: meta["shared"] as? Bool ?? false,
                sharedInstance: sharedInstance,
                error: (err?.isEmpty == false) ? err : nil,
                up: isUp(state)
            )
        }

        guard let tree = catalog["groupTree"] as? [[String: Any]], !tree.isEmpty else {
            return [ServiceSection(name: nil, services: metas.map(lineFor))]
        }

        var first: [String: String] = [:]
        var built: [ServiceSection] = []
        for group in tree {
            guard let name = group["name"] as? String else { continue }
            built.append(ServiceSection(name: name, services: []))
            for member in group["members"] as? [String] ?? [] where first[member] == nil {
                first[member] = name
            }
        }

        var rest: [ServiceLine] = []
        for meta in metas {
            let service = lineFor(meta)
            guard let id = meta["id"] as? String, let name = first[id] else {
                rest.append(service)
                continue
            }
            if let index = built.firstIndex(where: { $0.name == name }) {
                built[index].services.append(service)
            } else {
                rest.append(service)
            }
        }
        built.removeAll { $0.services.isEmpty }
        if !rest.isEmpty {
            built.append(ServiceSection(name: nil, services: rest))
        }
        return built.isEmpty ? [ServiceSection(name: nil, services: [])] : built
    }

    static func summary(_ sections: [ServiceSection]) -> String {
        var ready = 0, failed = 0, total = 0
        for service in lines(sections) {
            if service.finite && service.state != "failed" { continue }
            total += 1
            if isUp(service.state) { ready += 1 }
            else if service.state == "failed" { failed += 1 }
        }
        if total == 0 { return "" }
        if failed > 0 { return "\(ready)/\(total) ready  \(failed) failed" }
        return "\(ready)/\(total) ready"
    }

    static func stopAllTargets(_ sections: [ServiceSection]) -> [String] {
        lines(sections).compactMap { service in
            guard !service.disabled, !["stopped", "succeeded"].contains(service.state) else { return nil }
            return service.id
        }
    }

    static func groupTargets(_ sections: [ServiceSection], name: String) -> [String] {
        for section in sections where section.name == name {
            return section.services.filter { !$0.disabled }.map(\.id)
        }
        return []
    }

    static func groupIsUp(_ sections: [ServiceSection], name: String) -> Bool {
        var longLived: [ServiceLine] = []
        for section in sections where section.name == name {
            longLived += section.services.filter { !$0.disabled && !$0.finite }
        }
        guard !longLived.isEmpty else { return false }
        return longLived.allSatisfy { isUp($0.state) }
    }

    static func startAllTargets(groups: [String: [String]], sections: [ServiceSection]) -> [String] {
        if let all = groups["all"], !all.isEmpty { return all }
        return lines(sections).filter { !$0.disabled }.map(\.id)
    }

    static func urlVisible(_ requiresRunning: Bool, state: String) -> Bool {
        !requiresRunning || isUp(state) || state == "succeeded"
    }

    static func visibleUrls(_ urls: [[String: Any]], sections: [ServiceSection]) -> [VisibleURL] {
        var states: [String: String] = [:]
        for service in lines(sections) { states[service.id] = service.state }
        var visible: [VisibleURL] = []
        for url in urls {
            guard let serviceId = url["serviceId"] as? String,
                  let link = url["url"] as? String else { continue }
            let requires = url["requiresRunning"] as? Bool ?? true
            guard urlVisible(requires, state: states[serviceId] ?? "stopped") else { continue }
            let label = (url["label"] as? String).flatMap { $0.isEmpty ? nil : $0 } ?? serviceId
            visible.append(VisibleURL(serviceId: serviceId, label: label, url: link))
        }
        return visible
    }

    static func shouldReloadCatalog(previous: Int?, next: Int?) -> Bool {
        previous != nil && next != nil && previous != next
    }

    static func catalogStamp(root: String) -> Int? {
        var latest: Int?
        for name in ["hearth.yaml", "hearth.yml", "hearth.json"] {
            let path = root + "/" + name
            guard let attrs = try? FileManager.default.attributesOfItem(atPath: path),
                  let mtime = attrs[.modificationDate] as? Date else { continue }
            let stamp = Int(mtime.timeIntervalSince1970)
            if latest == nil || stamp > latest! { latest = stamp }
        }
        return latest
    }

    static func attachmentRoots(_ instance: [String: Any]) -> [String] {
        (instance["attachments"] as? [[String: Any]] ?? []).compactMap { row in
            let root = row["projectRoot"] as? String
            return (root?.isEmpty == false) ? root : nil
        }
    }

    static func instanceId(_ instance: [String: Any]) -> String {
        if let id = instance["id"] as? String, !id.isEmpty { return id }
        let name = instance["name"] as? String ?? ""
        let version = instance["version"] as? String ?? ""
        return "\(name)@\(version)"
    }

    static func recipesFrom(_ document: [String: Any]) -> [Recipe] {
        guard let services = document["services"] as? [String: [String: Any]] else { return [] }
        var rows: [Recipe] = []
        for (name, family) in services {
            let versions = ((family["versions"] as? [String: Any])?.keys ?? [:].keys).sorted()
            for version in versions {
                rows.append(Recipe(id: "\(name)@\(version)", name: name, version: version))
            }
        }
        return rows
    }

    static func instancesFrom(_ payload: [String: Any]) -> [Instance] {
        (payload["instances"] as? [[String: Any]] ?? []).map { instance in
            let state = ((instance["state"] as? [String: Any])?["actualState"] as? String) ?? ""
            return Instance(
                id: instanceId(instance),
                port: instance["port"],
                installState: (instance["installState"] as? String) ?? "",
                state: state,
                display: state.isEmpty ? "" : compactWire(state),
                attachments: attachmentRoots(instance).count
            )
        }
    }

    /// `all` = every attachment label, `others` = attachments outside `current`.
    static func classifyAttachments(_ roots: [String], current: String?, known: [WorkspaceLabel]) -> (all: [String], others: [String]) {
        let others = roots.filter { current == nil || !sameRoot($0, current!) }
        return (labelsFor(roots, known: known), labelsFor(others, known: known))
    }

    static func instanceSharedNotice(_ verb: String, instance: String, affected: [String]) -> String {
        let who = joinNames(affected)
        let (doing, consequence): (String, String) = switch verb {
        case "restart": ("Restarting", "takes the shared service down for \(who)")
        case "remove": ("Removing", "deletes its data and takes it down for \(who)")
        default: ("Stopping", "takes the shared service down for \(who)")
        }
        return "\(doing) \(instance) \(consequence). Press again to \(verb)."
    }

    /// Stopping here only detaches this workspace.
    static func projectSharedNotice(_ verb: String, current: String, touches: [(instance: String, others: [String])], unknown: [String]) -> String {
        var sentences: [String] = []
        if !touches.isEmpty {
            let parts = touches.map { "\($0.instance) (\(joinNames($0.others)))" }
            let listed = joinNames(parts)
            let be = touches.count == 1 ? "is" : "are"
            let them = touches.count == 1 ? "it" : "them"
            let doing = verb == "restart" ? "Restarting" : "Stopping"
            let effect = verb == "restart"
                ? "only detaches and reattaches \(current)"
                : "only detaches \(current)"
            sentences.append("\(listed) \(be) also used by other workspaces. \(doing) \(them) here \(effect). Those workspaces keep \(them)")
        }
        if !unknown.isEmpty {
            sentences.append("Could not check which workspaces use \(joinNames(unknown))")
        }
        return sentences.joined(separator: ". ") + ". Press again to \(verb)."
    }

    static func uncheckedSharedNotice(_ verb: String, instance: String) -> String {
        "Could not check which workspaces use \(instance). Press again to \(verb) anyway."
    }

    static func removeNotice(_ id: String, affected: [String], unchecked: Bool) -> String {
        if unchecked { return uncheckedSharedNotice("remove", instance: id) }
        if affected.isEmpty { return "Press again to remove \(id)." }
        return instanceSharedNotice("remove", instance: id, affected: affected)
    }

    static func joinNames(_ names: [String]) -> String {
        switch names.count {
        case 0: return ""
        case 1: return names[0]
        case 2: return "\(names[0]) and \(names[1])"
        default: return names.dropLast().joined(separator: ", ") + ", and \(names.last!)"
        }
    }

    static func sameRoot(_ left: String, _ right: String) -> Bool {
        (left as NSString).resolvingSymlinksInPath == (right as NSString).resolvingSymlinksInPath
    }

    static func compactWire(_ state: String) -> String {
        switch state {
        case "running-unready": return "running"
        case "preparing": return "starting"
        case "queued-start": return "queued"
        case "externally-owned": return "external"
        default: return state
        }
    }

    private static func meta(_ service: [String: Any]) -> [String: Any] {
        var ports: [String] = []
        for port in service["ports"] as? [Any] ?? [] {
            if let row = port as? [String: Any], let value = row["port"] {
                ports.append("\(value)")
            } else if let number = port as? Int {
                ports.append(String(number))
            } else if let text = port as? String, !text.isEmpty {
                ports.append(text)
            }
        }
        let shared = sharedInstanceOf(service)
        let label = service["label"] as? String
        return [
            "id": service["id"] as? String ?? "",
            "label": (label?.isEmpty == false) ? label! : (service["id"] as? String ?? ""),
            "ports": ports.joined(separator: ", "),
            "disabled": service["disabled"] as? Bool ?? false,
            "finite": isFinite(service),
            "infra": service["kind"] as? String == "infrastructure",
            "shared": shared != nil,
            "sharedInstance": shared ?? NSNull(),
        ]
    }

    private static func labelsFor(_ roots: [String], known: [WorkspaceLabel]) -> [String] {
        roots.map { root in
            if let match = known.first(where: { sameRoot($0.root, root) }) {
                return match.name
            }
            return folderBaseName(root)
        }
    }

    private static func folderBaseName(_ path: String) -> String {
        let name = (path as NSString).lastPathComponent
        return name.isEmpty || name == "/" ? path : name
    }

    private static func lines(_ sections: [ServiceSection]) -> [ServiceLine] {
        sections.flatMap(\.services)
    }
}
