// Swift mirrors of the wire shapes `hearth-core` (rust/crates/hearth-core/src/state.rs,
// rust/crates/hearth-core/src/manager/) sends over HTTP+SSE. Kept intentionally permissive (lots of
// optional fields, no strict enums for `identity`'s POSIX/Docker union) rather than a byte-for-byte
// port of the TS types — this client only needs enough to render status and drive start/stop/restart,
// and a permissive decode degrades gracefully instead of breaking the whole app on a field this
// client doesn't know about yet.

import Foundation

/// What `hearthd manager ensure --json` prints (and `manager restart --json`, which prints the same
/// shape for the daemon it just started) — see src/cli/localctl.ts's `manager ensure` handler.
/// This is the entire connection contract a generic (non-Bun) client needs.
struct ManagerConnection: Codable, Equatable {
    let instanceId: String
    let port: Int
    let token: String
    let protocolVersion: Int
    let runtimeDirectory: String
    let root: String

    var baseURL: URL { URL(string: "http://127.0.0.1:\(port)")! }
}

struct ProcessIdentity: Codable, Equatable {
    let pid: Int?
    let pgid: Int?
    let containerId: String?
    let containerName: String?
}

struct ServiceLifecycleState: Codable, Equatable, Identifiable {
    let serviceId: String
    let desiredState: String
    let actualState: String
    let readiness: String
    let generation: Int
    let identity: ProcessIdentity?
    let readinessKind: String?
    let readinessDetail: String?
    let createdAt: String
    let updatedAt: String
    let exitedAt: String?
    let exitCode: Int?
    let error: String?
    let currentOperationId: String?

    var id: String { serviceId }

    /// Fields the UI actually renders. Daemon timestamps (`updatedAt`) change on every poll even
    /// when nothing visible has moved, so comparing the whole struct would republish the service
    /// list (and rebuild the log panel) twice a second.
    func isVisuallyEqual(to other: ServiceLifecycleState) -> Bool {
        serviceId == other.serviceId
            && actualState == other.actualState
            && readiness == other.readiness
            && readinessDetail == other.readinessDetail
            && identity == other.identity
            && error == other.error
            && currentOperationId == other.currentOperationId
            && exitCode == other.exitCode
    }

    /// A small, display-oriented collapse of `actualState` — mirrors `localctl.ts`'s own `textState`.
    var displayState: String {
        switch actualState {
        case "ready": return "ready"
        case "queued-start": return "queued"
        case "running", "running-unready", "starting", "preparing": return "starting"
        case "stopping": return "stopping"
        case "failed": return "failed"
        case "orphaned": return "orphaned"
        case "externally-owned": return "external"
        default: return "stopped"
        }
    }
}

struct ServicesResponse: Codable {
    let services: [ServiceLifecycleState]
}

struct CatalogService: Codable, Equatable, Identifiable {
    let id: String
    let label: String?
    let kind: String?
    let ownership: String?
    var disabled: Bool? = nil

    var displayName: String { label ?? id }
    var isDisabled: Bool { disabled ?? false }
}

/// One `groups:` entry as declared — `members` may name services or other groups. The flattened
/// view (`groups`) is for resolving targets; this one is for grouped display, in declaration
/// order.
struct CatalogGroupDecl: Codable, Equatable, Identifiable {
    let name: String
    let members: [String]

    var id: String { name }
}

struct ServiceCatalogSummary: Codable, Equatable {
    let services: [CatalogService]
    let groups: [String: [String]]
    var groupTree: [CatalogGroupDecl]? = nil

    /// Ordered sections for the service list: each service lands in the first group that lists it
    /// directly (in `groups:` declaration order); the rest fall into a trailing `nil` section.
    /// Group names that list other groups only contribute members transitively, so they never
    /// become display sections themselves (e.g. `all`).
    func serviceSections(serviceOrder: [String]) -> [(name: String?, serviceIds: [String])] {
        let tree = groupTree ?? []
        guard !tree.isEmpty else { return [(nil, serviceOrder)] }
        var firstGroup: [String: String] = [:]
        for group in tree {
            for member in group.members where firstGroup[member] == nil {
                firstGroup[member] = group.name
            }
        }
        var sections: [(name: String?, serviceIds: [String])] = tree.map { ($0.name as String?, []) }
        var rest: [String] = []
        for id in serviceOrder {
            if let group = firstGroup[id], let index = sections.firstIndex(where: { $0.name == group }) {
                sections[index].serviceIds.append(id)
            } else {
                rest.append(id)
            }
        }
        sections = sections.filter { !$0.serviceIds.isEmpty }
        if !rest.isEmpty {
            sections.append((nil, rest))
        }
        return sections
    }
}

/// One registered service URL with its placeholders already resolved by the daemon
/// (`GET /v1/urls`). `requiresRunning` is `false` only when the catalog said the URL works while the
/// service is stopped.
struct ResolvedServiceUrl: Codable, Equatable, Hashable {
    let serviceId: String
    let label: String?
    let url: String
    let requiresRunning: Bool

    /// What a link shows: the catalog's label, else the URL's host and port.
    var displayName: String {
        if let label, !label.isEmpty { return label }
        guard let components = URLComponents(string: url), let host = components.host else { return url }
        return components.port.map { "\(host):\($0)" } ?? host
    }
}

struct UrlsResponse: Codable {
    let urls: [ResolvedServiceUrl]
}

struct CatalogResponse: Codable {
    let catalog: ServiceCatalogSummary
}

struct OperationTraceEntry: Codable, Equatable {
    let at: String
    let message: String
}

struct OperationError: Codable, Equatable {
    let code: String
    let message: String
}

struct ManagerOperation: Codable, Equatable, Identifiable {
    let id: String
    let requestId: String
    let kind: String
    let serviceId: String?
    let action: String?
    let status: String
    let createdAt: String
    let updatedAt: String
    let trace: [OperationTraceEntry]
    let error: OperationError?
}

struct OperationResponse: Codable {
    let operation: ManagerOperation
}

struct ManagerInfo: Codable {
    let protocolVersion: Int
    let instanceId: String
    let pid: Int
    let port: Int
    let startedAt: String
    let runtimeDirectory: String
}

struct LogSlice: Codable {
    let serviceId: String
    let generation: Int
    let cursor: Int
    let nextCursor: Int
    let data: String
    let reset: Bool
    let truncated: Bool
}

struct ManagerErrorEnvelope: Codable {
    struct Body: Codable { let code: String; let message: String }
    let error: Body
}

enum ManagerAction: String {
    case start, stop, restart
}

// MARK: - Shared services (`smp` daemon, `GET /v1/shared*`)

/// A project attached to a shared instance. `connection` is the rendered per-project connection
/// block from the service recipe (`{ url, env }`) — present once the instance provisioned it.
struct SharedAttachment: Codable, Equatable {
    let projectId: String
    let projectRoot: String
    let provisioned: Bool
    let connection: SharedConnection?
}

struct SharedConnection: Codable, Equatable {
    let url: String?
    let env: [String: String]?
}

struct SharedInstanceState: Codable, Equatable {
    let actualState: String?
    let readiness: String?
}

/// One `name@version` singleton under smp (`GET /v1/shared`).
struct SharedInstance: Codable, Equatable, Identifiable {
    let id: String
    let name: String
    let version: String
    let port: Int
    let installState: String
    let installError: String?
    let state: SharedInstanceState?
    let attachments: [SharedAttachment]

    /// Same display collapse as `ServiceLifecycleState.displayState` — the wire states are the
    /// same supervisor states, just nested under `state` here.
    var displayState: String {
        switch state?.actualState {
        case "ready": return "ready"
        case "queued-start": return "queued"
        case "running", "running-unready", "starting", "preparing": return "starting"
        case "stopping": return "stopping"
        case "failed": return "failed"
        case "orphaned": return "orphaned"
        case "externally-owned": return "external"
        default: return "stopped"
        }
    }
}

struct SharedInstancesResponse: Codable {
    let instances: [SharedInstance]
}

/// A recipe as the catalog UI needs it — the version key is the datum; the body is only decoded
/// for an optional human description. Permissive on purpose (see this file's header comment).
struct SharedRecipeSummary: Codable, Equatable {
    let description: String?
}

struct SharedFamily: Codable, Equatable {
    let versions: [String: SharedRecipeSummary]
}

/// The remote shared-services registry (`GET /v1/shared/catalog`).
struct SharedCatalogDocument: Codable, Equatable {
    let version: Int?
    let services: [String: SharedFamily]
}

struct SharedCatalogResponse: Codable {
    let catalog: SharedCatalogDocument
}

/// The `POST /v1/shared/install|remove` reply — fields optional because a client shouldn't break on
/// additive response keys.
struct SharedMutationResponse: Codable {
    let service: String?
    let port: Int?
    let installState: String?
}
