import Foundation

// MARK: - Daemon session

/// `hearth manager ensure|status|restart --json`. `token` is present only on a call that
/// attaches (ensure, restart).
public struct DaemonInfo: Decodable, Sendable, Equatable {
    public let port: Int
    public let token: String?
    public let protocolVersion: Int?
    public let instanceId: String?
}

/// What the app keeps in memory for one attached project daemon. Never written to disk.
public struct Session: Sendable, Equatable {
    public let token: String
    public let port: Int
    public let protocolVersion: Int?

    public init(token: String, port: Int, protocolVersion: Int?) {
        self.token = token
        self.port = port
        self.protocolVersion = protocolVersion
    }

    public init?(_ info: DaemonInfo) {
        guard let token = info.token, !token.isEmpty, info.port > 0 else { return nil }
        self.init(token: token, port: info.port, protocolVersion: info.protocolVersion)
    }
}

// MARK: - Live state (`GET /v1/services`)

public struct LiveService: Decodable, Sendable, Equatable {
    public let serviceId: String
    public let actualState: String
    public let generation: Int?
    public let error: String?

    public init(serviceId: String, actualState: String, generation: Int? = nil, error: String? = nil) {
        self.serviceId = serviceId
        self.actualState = actualState
        self.generation = generation
        self.error = error
    }
}

public struct ServicesResponse: Decodable, Sendable {
    public let services: [LiveService]
}

// MARK: - Catalog (`GET /v1/catalog`)

public struct CatalogEnvelope: Decodable, Sendable {
    public let catalog: Catalog
}

public struct Catalog: Decodable, Sendable, Equatable {
    public var services: [CatalogService]
    public var groups: [String: [String]]
    public var groupTree: [GroupNode]

    public init(services: [CatalogService] = [], groups: [String: [String]] = [:], groupTree: [GroupNode] = []) {
        self.services = services
        self.groups = groups
        self.groupTree = groupTree
    }

    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: Keys.self)
        services = try c.decodeIfPresent([CatalogService].self, forKey: .services) ?? []
        groups = try c.decodeIfPresent([String: [String]].self, forKey: .groups) ?? [:]
        groupTree = try c.decodeIfPresent([GroupNode].self, forKey: .groupTree) ?? []
    }

    private enum Keys: String, CodingKey { case services, groups, groupTree }
}

public struct GroupNode: Decodable, Sendable, Equatable {
    public let name: String
    public let members: [String]

    public init(name: String, members: [String]) {
        self.name = name
        self.members = members
    }
}

public struct CatalogService: Decodable, Sendable, Equatable {
    public let id: String
    public let label: String?
    public let kind: String?
    public let disabled: Bool
    public let readinessKind: String?
    public let commandVerified: Bool
    public let argv: [String]
    public let ports: [Int]

    public init(
        id: String, label: String? = nil, kind: String? = nil, disabled: Bool = false,
        readinessKind: String? = nil, commandVerified: Bool = true, argv: [String] = [], ports: [Int] = []
    ) {
        self.id = id
        self.label = label
        self.kind = kind
        self.disabled = disabled
        self.readinessKind = readinessKind
        self.commandVerified = commandVerified
        self.argv = argv
        self.ports = ports
    }

    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: Keys.self)
        id = try c.decode(String.self, forKey: .id)
        label = try c.decodeIfPresent(String.self, forKey: .label)
        kind = try c.decodeIfPresent(String.self, forKey: .kind)
        disabled = try c.decodeIfPresent(Bool.self, forKey: .disabled) ?? false
        let run = try c.decodeIfPresent(Profiles.self, forKey: .profiles)?.run
        readinessKind = run?.readiness?.kind
        commandVerified = run?.commandStatus == "verified"
        argv = run?.command?.command?.argv ?? []
        ports = try c.decodeIfPresent([PortValue].self, forKey: .ports)?.compactMap(\.value) ?? []
    }

    /// `hearth shared attach <name@version>` makes this row a project `shared:` service.
    public var sharedInstance: String? {
        guard commandVerified else { return nil }
        for index in argv.indices where index + 2 < argv.count {
            guard argv[index] == "shared", argv[index + 1] == "attach" else { continue }
            let id = argv[index + 2]
            return id.contains("@") && !id.hasPrefix("-") ? id : nil
        }
        return nil
    }

    private enum Keys: String, CodingKey { case id, label, kind, disabled, profiles, ports }
    private struct Profiles: Decodable { let run: Run? }
    private struct Run: Decodable {
        let commandStatus: String?
        let command: Wrapper?
        let readiness: Readiness?
    }
    private struct Wrapper: Decodable { let command: Spec? }
    private struct Spec: Decodable { let argv: [String]? }
    private struct Readiness: Decodable { let kind: String? }

    /// A port is `{ port, label }`, a bare number, or a string.
    private struct PortValue: Decodable {
        let value: Int?
        init(from decoder: Decoder) throws {
            if let object = try? decoder.container(keyedBy: PortKeys.self),
               let port = try? object.decode(Int.self, forKey: .port) {
                value = port
            } else if let single = try? decoder.singleValueContainer() {
                value = (try? single.decode(Int.self)) ?? (try? single.decode(String.self)).flatMap(Int.init)
            } else {
                value = nil
            }
        }
        private enum PortKeys: String, CodingKey { case port }
    }
}

// MARK: - URLs, logs, operations

public struct UrlsResponse: Decodable, Sendable {
    public let urls: [UrlRow]
}

public struct UrlRow: Decodable, Sendable, Equatable {
    public let serviceId: String
    public let url: String
    public let label: String?
    public let requiresRunning: Bool?

    public init(serviceId: String, url: String, label: String? = nil, requiresRunning: Bool? = nil) {
        self.serviceId = serviceId
        self.url = url
        self.label = label
        self.requiresRunning = requiresRunning
    }
}

public struct LogSlice: Decodable, Sendable, Equatable {
    public let data: String
    public let cursor: Int?
    public let nextCursor: Int?
    public let generation: Int?
    public let reset: Bool?
    /// The window starts after byte 0, so an earlier page exists in this file.
    public let truncated: Bool?

    public init(data: String, cursor: Int? = nil, nextCursor: Int? = nil, generation: Int? = nil, reset: Bool? = nil, truncated: Bool? = nil) {
        self.data = data
        self.cursor = cursor
        self.nextCursor = nextCursor
        self.generation = generation
        self.reset = reset
        self.truncated = truncated
    }
}

public struct OperationEnvelope: Decodable, Sendable {
    public let operation: Operation

    public struct Operation: Decodable, Sendable {
        public let id: String
        public let status: String
        public let error: Failure?
        public var isSettled: Bool { status == "succeeded" || status == "failed" }
    }

    public struct Failure: Decodable, Sendable {
        public let message: String?
    }
}

// MARK: - Shared services (smp)

/// `hearth shared list --json`: `{ services: { <name>: { versions: { <ver>: … } } } }`.
public struct SharedCatalog: Decodable, Sendable {
    public let recipes: [Recipe]

    public struct Recipe: Sendable, Equatable, Identifiable {
        public let name: String
        public let version: String
        public var id: String { "\(name)@\(version)" }
    }

    private struct Family: Decodable { let versions: [String: Ignored]? }
    private struct Ignored: Decodable { init(from decoder: Decoder) throws {} }
    private enum Keys: String, CodingKey { case services }

    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: Keys.self)
        let families = try c.decodeIfPresent([String: Family].self, forKey: .services) ?? [:]
        recipes = families.keys.sorted().flatMap { name in
            (families[name]?.versions ?? [:]).keys.sorted().map { Recipe(name: name, version: $0) }
        }
    }
}

/// `hearth shared status|installed --json`: `{ instances: [ … ] }`.
public struct SharedInstances: Decodable, Sendable {
    public let instances: [SharedInstance]
}

public struct SharedInstance: Decodable, Sendable, Equatable, Identifiable {
    public let name: String
    public let version: String
    public let port: Int?
    public let installState: String
    public let actualState: String?
    /// Absolute project roots attached to this instance.
    public let attachmentRoots: [String]

    public var id: String { "\(name)@\(version)" }
    public var isUp: Bool { ServiceBoard.isUp(actualState ?? "") }

    public init(
        name: String, version: String, port: Int? = nil, installState: String = "installed",
        actualState: String? = nil, attachmentRoots: [String] = []
    ) {
        self.name = name
        self.version = version
        self.port = port
        self.installState = installState
        self.actualState = actualState
        self.attachmentRoots = attachmentRoots
    }

    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: Keys.self)
        name = try c.decodeIfPresent(String.self, forKey: .name) ?? ""
        version = try c.decodeIfPresent(String.self, forKey: .version) ?? ""
        port = try c.decodeIfPresent(Int.self, forKey: .port)
        installState = try c.decodeIfPresent(String.self, forKey: .installState) ?? ""
        actualState = try c.decodeIfPresent(State.self, forKey: .state)?.actualState
        // The registry stores attachments as a map keyed by project id; accept a list too.
        if let map = try? c.decode([String: Attachment].self, forKey: .attachments) {
            attachmentRoots = map.keys.sorted().compactMap { map[$0]?.projectRoot }.filter { !$0.isEmpty }
        } else if let list = try? c.decode([Attachment].self, forKey: .attachments) {
            attachmentRoots = list.compactMap(\.projectRoot).filter { !$0.isEmpty }
        } else {
            attachmentRoots = []
        }
    }

    private enum Keys: String, CodingKey { case name, version, port, installState, state, attachments }
    private struct State: Decodable { let actualState: String? }
    private struct Attachment: Decodable { let projectRoot: String? }
}
