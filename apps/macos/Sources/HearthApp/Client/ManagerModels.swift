// Swift mirrors of the wire shapes `hearth-core` (rust/crates/hearth-core/src/state.rs,
// rust/crates/hearth-core/src/manager/) sends over HTTP+SSE. Kept intentionally permissive (lots of
// optional fields, no strict enums for `identity`'s POSIX/Docker union) rather than a byte-for-byte
// port of the Rust types — this client only needs enough to render status and drive
// start/stop/restart, and a permissive decode degrades gracefully instead of breaking the whole app
// on a field this client doesn't know about yet. Fields nothing reads are deliberately not declared:
// a non-optional field the daemon stops sending would fail the whole decode for no benefit.

import Foundation

/// What `hearthd manager ensure --json` prints (and `manager restart --json`, which prints the same
/// shape for the daemon it just started) — see `rust/crates/hearth-cli/src/lib.rs`. The subset of
/// that output this client uses; `instanceId`/`runtimeDirectory`/`root` are printed too but unread.
struct ManagerConnection: Codable, Equatable {
    let port: Int
    let token: String
    let protocolVersion: Int

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

    var displayState: String { Self.displayState(for: actualState) }

    /// Counted as "up" in every ready/total summary — `ready`, plus `running` (a `kind: process`
    /// service sits in `running` forever and must not read as a stuck boot).
    var isUp: Bool { ["ready", "running"].contains(displayState) }

    /// A small, display-oriented collapse of `actualState` — the app's analogue of the CLI's
    /// `text_state` (which collapses every in-flight state into `running`).
    /// `running` covers both `running` and `running-unready`: the process is alive but readiness
    /// either isn't probed yet or isn't probeable at all (`kind: process` services sit here
    /// permanently — showing them as "starting" forever read as a stuck boot). Shared instances
    /// carry the same supervisor states, so `SharedInstance.displayState` goes through here too.
    static func displayState(for actualState: String?) -> String {
        switch actualState {
        case "ready": return "ready"
        case "queued-start": return "queued"
        case "running", "running-unready": return "running"
        case "starting", "preparing": return "starting"
        case "stopping": return "stopping"
        case "succeeded": return "succeeded"
        case "failed": return "failed"
        case "orphaned": return "orphaned"
        case "externally-owned": return "external"
        default: return "stopped"
        }
    }
}

/// The "ready/total · failed" numbers every summary shows (sidebar row, status strip, menu bar).
struct ServiceCounts: Equatable {
    var ready = 0
    var failed = 0
    var total = 0

    init() {}

    init(_ services: [ServiceLifecycleState]) {
        for service in services { add(service) }
    }

    /// `finite` is a `readiness: exit` command. It is a job, not a server, so it stays out of
    /// ready/total unless it failed — a never-run build must not turn the summary into `12/13`,
    /// and a failed one still raises the red badge.
    mutating func add(_ service: ServiceLifecycleState, finite: Bool = false) {
        if finite && service.displayState != "failed" {
            return
        }
        total += 1
        if service.isUp {
            ready += 1
        } else if service.displayState == "failed" {
            failed += 1
        }
    }
}

struct ServicesResponse: Codable {
    let services: [ServiceLifecycleState]
}

/// Enough of `profiles.run.readiness` to tell a finite command (`kind: exit`) from a server
/// before the first start. The lifecycle row has no `readinessKind` until then.
struct CatalogReadiness: Codable, Equatable {
    let kind: String
}

struct CatalogRunProfile: Codable, Equatable {
    var readiness: CatalogReadiness? = nil
}

struct CatalogProfiles: Codable, Equatable {
    let run: CatalogRunProfile
}

struct CatalogService: Codable, Equatable, Identifiable {
    let id: String
    let label: String?
    let kind: String?
    let ownership: String?
    var disabled: Bool? = nil
    var profiles: CatalogProfiles? = nil

    var displayName: String { label ?? id }
    var isDisabled: Bool { disabled ?? false }
    /// `readiness: { kind: exit }` — the run command is a job that exits.
    var isFinite: Bool { profiles?.run.readiness?.kind == "exit" }
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
    let error: OperationError?
}

struct OperationResponse: Codable {
    let operation: ManagerOperation
}

struct LogSlice: Codable {
    let serviceId: String
    let generation: Int
    let nextCursor: Int
    let data: String
    let reset: Bool
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
    var displayState: String { ServiceLifecycleState.displayState(for: state?.actualState) }
}

struct SharedInstancesResponse: Codable {
    let instances: [SharedInstance]
}

/// A recipe as the catalog UI needs it — the version key is the datum; the body is decoded only
/// for an optional description and `readiness.kind` (so a finite recipe can say Run before its
/// first start). Permissive on purpose (see this file's header comment).
struct SharedRecipeSummary: Codable, Equatable {
    let description: String?
    var readiness: CatalogReadiness? = nil

    var isFinite: Bool { readiness?.kind == "exit" }
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
