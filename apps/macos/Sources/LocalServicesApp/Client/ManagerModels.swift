// Swift mirrors of the wire shapes `@gnasdev/local-services/core` (src/core/state.ts, src/core/
// catalog.ts, src/core/manager.ts) sends over HTTP+SSE. Kept intentionally permissive (lots of
// optional fields, no strict enums for `identity`'s POSIX/Docker union) rather than a byte-for-byte
// port of the TS types — this client only needs enough to render status and drive start/stop/restart,
// and a permissive decode degrades gracefully instead of breaking the whole app on a field this
// client doesn't know about yet.

import Foundation

/// What `lsd manager ensure --json` prints — see src/cli/localctl.ts's `manager ensure` handler.
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
    let dependencies: [String]?

    var displayName: String { label ?? id }
}

struct ServiceCatalogSummary: Codable, Equatable {
    let services: [CatalogService]
    let groups: [String: [String]]
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
