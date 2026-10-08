import Foundation

public enum ManagerError: Error, LocalizedError, Sendable, Equatable {
    /// 401, or no session held: the window must re-attach with Start.
    case sessionEnded
    case transport(String)
    case http(status: Int, message: String)
    case timedOut

    public var errorDescription: String? {
        switch self {
        case .sessionEnded: "Daemon session ended. Start attaches this window."
        case .transport(let m): m
        case .http(let status, let m): m.isEmpty ? "daemon answered \(status)" : m
        case .timedOut: "operation timed out"
        }
    }
}

/// Bearer client for one project daemon. Holds the session for the life of the value;
/// the app drops the client when the daemon stops or the token is refused.
public struct ManagerClient: Sendable {
    public let session: Session
    private let urlSession: URLSession

    public init(session: Session, urlSession: URLSession = .shared) {
        self.session = session
        self.urlSession = urlSession
    }

    // MARK: Requests

    public func makeRequest(_ method: String, _ path: String, query: [String: String] = [:], body: Data? = nil) throws -> URLRequest {
        var components = URLComponents()
        components.scheme = "http"
        components.host = "127.0.0.1"
        components.port = session.port
        components.percentEncodedPath = path
        if !query.isEmpty {
            components.queryItems = query.sorted { $0.key < $1.key }.map { URLQueryItem(name: $0.key, value: $0.value) }
        }
        guard let url = components.url else { throw ManagerError.transport("bad url") }
        var request = URLRequest(url: url, timeoutInterval: 20)
        request.httpMethod = method
        request.setValue("Bearer \(session.token)", forHTTPHeaderField: "Authorization")
        request.setValue("application/json", forHTTPHeaderField: "Accept")
        // Authorized routes answer 426 unless this matches the daemon protocol.
        if let proto = session.protocolVersion {
            request.setValue(String(proto), forHTTPHeaderField: "x-hearth-protocol")
        }
        if let body {
            request.setValue("application/json", forHTTPHeaderField: "Content-Type")
            request.httpBody = body
        }
        return request
    }

    private func send<T: Decodable>(_ type: T.Type, _ method: String, _ path: String,
                                    query: [String: String] = [:], body: Data? = nil) async throws -> T {
        let request = try makeRequest(method, path, query: query, body: body)
        let data: Data, response: URLResponse
        do {
            (data, response) = try await urlSession.data(for: request)
        } catch is CancellationError {
            throw CancellationError()
        } catch {
            if (error as? URLError)?.code == .cancelled { throw CancellationError() }
            throw ManagerError.transport(error.localizedDescription)
        }
        let status = (response as? HTTPURLResponse)?.statusCode ?? 0
        if status == 401 { throw ManagerError.sessionEnded }
        guard (200..<300).contains(status) else {
            throw ManagerError.http(status: status, message: Self.errorMessage(data))
        }
        do {
            return try JSONDecoder().decode(T.self, from: data)
        } catch {
            throw ManagerError.transport("unexpected daemon response: \(error.localizedDescription)")
        }
    }

    /// `{ "error": { "message": … } }`.
    static func errorMessage(_ data: Data) -> String {
        struct Envelope: Decodable { struct E: Decodable { let message: String? }; let error: E? }
        return (try? JSONDecoder().decode(Envelope.self, from: data))?.error?.message ?? ""
    }

    static func escape(_ segment: String) -> String {
        segment.addingPercentEncoding(withAllowedCharacters: .urlPathAllowed.subtracting(CharacterSet(charactersIn: "/"))) ?? segment
    }

    // MARK: Typed calls

    public func services() async throws -> [LiveService] {
        try await send(ServicesResponse.self, "GET", "/v1/services").services
    }

    public func catalog() async throws -> Catalog {
        try await send(CatalogEnvelope.self, "GET", "/v1/catalog").catalog
    }

    public func urls() async throws -> [UrlRow] {
        try await send(UrlsResponse.self, "GET", "/v1/urls").urls
    }

    public func serviceLog(_ id: String, cursor: Int?, generation: Int?, limit: Int) async throws -> LogSlice {
        var query = ["limit": String(limit)]
        if let cursor { query["cursor"] = String(cursor) }
        if let generation { query["generation"] = String(generation) }
        return try await send(LogSlice.self, "GET", "/v1/logs/\(Self.escape(id))", query: query)
    }

    public func healthz() async -> Bool {
        guard let url = URL(string: "http://127.0.0.1:\(session.port)/healthz") else { return false }
        var request = URLRequest(url: url, timeoutInterval: 5)
        request.httpMethod = "GET"
        guard let (_, response) = try? await urlSession.data(for: request) else { return false }
        return (response as? HTTPURLResponse).map { (200..<400).contains($0.statusCode) } ?? false
    }

    // MARK: Operations

    /// The body of `POST /v1/operations`. `killUnowned` is only legal on `start` and must only
    /// ever be set after an explicit user confirmation.
    public static func operationBody(serviceId: String, action: String, killUnowned: Bool, requestId: String = UUID().uuidString) throws -> Data {
        var body: [String: Any] = ["requestId": requestId, "serviceId": serviceId, "action": action]
        if killUnowned {
            guard action == "start" else {
                throw ManagerError.transport("killUnowned only applies to a start action")
            }
            body["killUnowned"] = true
        }
        return try JSONSerialization.data(withJSONObject: body)
    }

    /// Submit one action and wait until it settles. Returns normally on success and throws
    /// the daemon's message on failure.
    public func perform(serviceId: String, action: String, killUnowned: Bool = false,
                        timeout: Duration = .seconds(180)) async throws {
        let body = try Self.operationBody(serviceId: serviceId, action: action, killUnowned: killUnowned)
        var operation = try await send(OperationEnvelope.self, "POST", "/v1/operations", body: body).operation
        let clock = ContinuousClock()
        let deadline = clock.now.advanced(by: timeout)
        while !operation.isSettled {
            if clock.now >= deadline { throw ManagerError.timedOut }
            try await Task.sleep(for: .milliseconds(250))
            operation = try await send(OperationEnvelope.self, "GET", "/v1/operations/\(Self.escape(operation.id))").operation
        }
        if operation.status == "failed" {
            let message = operation.error?.message ?? ""
            throw ManagerError.http(status: 0, message: message.isEmpty ? "\(action) failed" : message)
        }
    }
}
