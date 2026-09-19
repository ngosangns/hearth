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

    var errorDescription: String? {
        switch self {
        case .http(_, let code, let message): return "\(code): \(message)"
        case .decoding(let error): return "malformed manager response: \(error)"
        case .transport(let error): return "manager unavailable: \(error.localizedDescription)"
        }
    }
}

final class ManagerClient: Sendable {
    private let connection: ManagerConnection
    private let session: URLSession

    init(connection: ManagerConnection, session: URLSession = .shared) {
        self.connection = connection
        self.session = session
    }

    func services() async throws -> [ServiceLifecycleState] {
        try await get("/v1/services", as: ServicesResponse.self).services
    }

    func catalog() async throws -> ServiceCatalogSummary {
        try await get("/v1/catalog", as: CatalogResponse.self).catalog
    }

    func managerInfo() async throws -> ManagerInfo {
        try await get("/v1/manager", as: ManagerInfo.self)
    }

    @discardableResult
    func perform(_ action: ManagerAction, serviceId: String) async throws -> Operation {
        let body: [String: String] = ["requestId": UUID().uuidString, "serviceId": serviceId, "action": action.rawValue]
        return try await post("/v1/operations", body: body, as: OperationResponse.self).operation
    }

    // MARK: - Transport

    private func request(_ path: String) -> URLRequest {
        var request = URLRequest(url: connection.baseURL.appendingPathComponent(path))
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
