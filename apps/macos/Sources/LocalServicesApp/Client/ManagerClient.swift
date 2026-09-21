// Thin HTTP client for one daemon's loopback API (src/core/manager.ts's `handleRequest`). One
// instance per workspace; the connection (host/port/token/protocolVersion) comes from
// `DaemonConnection`/`lsd manager ensure --json`, not from anything this client discovers itself —
// mirrors how the CLI/TUI/MCP clients in the package itself never touch lock-file discovery directly
// either, they go through `localctl.ts`'s `ensure`.
//
// Polling, not SSE, for v1: `GET /v1/services` on a timer (see `WorkspaceController`). The daemon's
// `/v1/events/stream` (SSE) is the lower-latency path the TUI/CLI use, and is a documented fast
// follow here — polling is simpler to get right on the first pass and cheap enough for a handful of
// services on a local loopback connection.

import Foundation

enum ManagerClientError: Error, LocalizedError {
    case http(status: Int, code: String, message: String)
    case decoding(Error)
    case transport(Error)
    /// An operation the daemon accepted (202) and then settled as `failed`. Its message is the
    /// daemon's own — e.g. `Dependency startup failed: db (…)`.
    case operationFailed(String)

    var errorDescription: String? {
        switch self {
        case .http(_, let code, let message): return "\(code): \(message)"
        case .decoding(let error): return "malformed manager response: \(error)"
        case .transport(let error): return "manager unavailable: \(error.localizedDescription)"
        case .operationFailed(let message): return message
        }
    }
}

final class ManagerClient: ManagerAPI {
    private let connection: ManagerConnection
    private let session: URLSession

    init(connection: ManagerConnection, session: URLSession = .shared) {
        self.connection = connection
        self.session = session
    }

    func services() async throws -> [ServiceLifecycleState] {
        try await get("/v1/services", as: ServicesResponse.self).services
    }

    func urls() async throws -> [ResolvedServiceUrl] {
        try await get("/v1/urls", as: UrlsResponse.self).urls
    }

    func catalog() async throws -> ServiceCatalogSummary {
        try await get("/v1/catalog", as: CatalogResponse.self).catalog
    }

    func managerInfo() async throws -> ManagerInfo {
        try await get("/v1/manager", as: ManagerInfo.self)
    }

    /// `cursor`/`generation` mirror what `src/core/manager.ts`'s `/v1/logs/:serviceId` expects — pass
    /// back the previous slice's `nextCursor`/`generation` to continue tailing; omit both for the
    /// initial fetch. A `reset: true` slice (the log rotated or the caller's `generation` was stale)
    /// means the caller should replace its buffer, not append.
    func logs(serviceId: String, cursor: Int?, generation: Int?, limit: Int) async throws -> LogSlice {
        let escaped = serviceId.addingPercentEncoding(withAllowedCharacters: .urlPathAllowed) ?? serviceId
        var path = "/v1/logs/\(escaped)?limit=\(limit)"
        if let cursor { path += "&cursor=\(cursor)" }
        if let generation { path += "&generation=\(generation)" }
        return try await get(path, as: LogSlice.self)
    }

    @discardableResult
    func perform(_ action: ManagerAction, serviceId: String) async throws -> ManagerOperation {
        let body: [String: String] = ["requestId": UUID().uuidString, "serviceId": serviceId, "action": action.rawValue]
        return try await post("/v1/operations", body: body, as: OperationResponse.self).operation
    }

    /// `POST /v1/operations/bulk-start` — brings up `targets` in dependency order, stopping on the
    /// first failure (same policy the CLI's `start <group> --wait` uses). There's no bulk-stop
    /// endpoint on the daemon (see AGENTS.md); stopping several services is a client-side loop of
    /// individual `perform(.stop, ...)` calls instead — see `WorkspaceController.stopAll`.
    /// `GET /v1/operations/:id` — the daemon accepts an operation with `202` and runs it
    /// asynchronously, so its outcome is only ever visible by reading it back.
    func operation(id: String) async throws -> ManagerOperation {
        let escaped = id.addingPercentEncoding(withAllowedCharacters: .urlPathAllowed) ?? id
        return try await get("/v1/operations/\(escaped)", as: OperationResponse.self).operation
    }

    @discardableResult
    func bulkStart(targets: [String]) async throws -> ManagerOperation {
        var req = request("/v1/operations/bulk-start")
        req.httpMethod = "POST"
        req.setValue("application/json", forHTTPHeaderField: "content-type")
        req.httpBody = try? JSONSerialization.data(withJSONObject: ["requestId": UUID().uuidString, "targets": targets])
        return try await send(req, as: OperationResponse.self).operation
    }

    // MARK: - Transport

    // `appendingPathComponent` percent-encodes `?`/`&` as literal path characters instead of treating
    // them as a query delimiter — a `path` like `/v1/logs/kafka?limit=…` (see `logs()` above) would
    // turn into a request for the path `/v1/logs/kafka%3Flimit=…`, which the daemon's router (matching
    // on `url.pathname`) 404s as `service_not_found`. Splitting off the query and assigning it via
    // `percentEncodedQuery` (its values here are already query-safe — numbers and a percent-encoded
    // serviceId) keeps `path` callers passing a plain `"/foo?a=b"` string without needing URLComponents
    // at each call site.
    // `internal` (not `private`) so `ManagerClientTests` can assert on the built `URLRequest` directly
    // — this is the one place a `?query` string embedded in `path` (see `logs()` above) gets parsed.
    func request(_ path: String) -> URLRequest {
        var pathOnly = path
        var query: String?
        if let index = path.firstIndex(of: "?") {
            pathOnly = String(path[path.startIndex..<index])
            query = String(path[path.index(after: index)...])
        }
        var components = URLComponents(url: connection.baseURL.appendingPathComponent(pathOnly), resolvingAgainstBaseURL: false)!
        components.percentEncodedQuery = query
        var request = URLRequest(url: components.url!)
        request.setValue("Bearer \(connection.token)", forHTTPHeaderField: "authorization")
        request.setValue(String(connection.protocolVersion), forHTTPHeaderField: "x-local-services-protocol")
        return request
    }

    private func get<T: Decodable>(_ path: String, as type: T.Type) async throws -> T {
        try await send(request(path), as: type)
    }

    private func post<T: Decodable>(_ path: String, body: [String: String], as type: T.Type) async throws -> T {
        var req = request(path)
        req.httpMethod = "POST"
        req.setValue("application/json", forHTTPHeaderField: "content-type")
        req.httpBody = try? JSONSerialization.data(withJSONObject: body)
        return try await send(req, as: type)
    }

    private func send<T: Decodable>(_ request: URLRequest, as type: T.Type) async throws -> T {
        let data: Data
        let response: URLResponse
        do {
            (data, response) = try await session.data(for: request)
        } catch {
            throw ManagerClientError.transport(error)
        }
        guard let http = response as? HTTPURLResponse else {
            throw ManagerClientError.transport(URLError(.badServerResponse))
        }
        guard (200..<300).contains(http.statusCode) else {
            if let envelope = try? JSONDecoder().decode(ManagerErrorEnvelope.self, from: data) {
                throw ManagerClientError.http(status: http.statusCode, code: envelope.error.code, message: envelope.error.message)
            }
            throw ManagerClientError.http(status: http.statusCode, code: "request_failed", message: "HTTP \(http.statusCode)")
        }
        do {
            return try JSONDecoder().decode(T.self, from: data)
        } catch {
            throw ManagerClientError.decoding(error)
        }
    }
}
