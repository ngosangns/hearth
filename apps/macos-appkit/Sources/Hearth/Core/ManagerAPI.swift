import Foundation

/// Bearer client for one project daemon. The token is read from the session store
/// at each call and is never stored on this object after the request returns.
/// Call from a background queue — `send` blocks on a semaphore.
final class ManagerAPI {
    let root: String

    init(root: String) { self.root = root }

    static func open(root: String) -> ManagerAPI? {
        SessionStore.shared.session(root) == nil ? nil : ManagerAPI(root: root)
    }

    func get(_ path: String, query: [String: String] = [:]) -> ApiResult {
        send("GET", path, query: query, body: nil)
    }

    func submit(serviceId: String, action: String, killUnowned: Bool = false) -> ApiResult {
        var body: [String: Any] = [
            "requestId": UUID().uuidString,
            "serviceId": serviceId,
            "action": action,
        ]
        if killUnowned {
            guard action == "start" else {
                return ApiResult(ok: false, status: nil, json: nil, unauthorized: false,
                                 message: "killUnowned only applies to a start action")
            }
            body["killUnowned"] = true
        }
        return send("POST", "/v1/operations", query: [:], body: body)
    }

    /// Poll until the operation is succeeded or failed. Interval is about 250ms.
    func wait(operationId: String, timeoutSeconds: TimeInterval = 180) -> ApiResult {
        let deadline = Date().addingTimeInterval(timeoutSeconds)
        var last: ApiResult?
        while Date() < deadline {
            let result = get("/v1/operations/\(operationId.addingPercentEncoding(withAllowedCharacters: .urlPathAllowed) ?? operationId)")
            if !result.ok || result.unauthorized { return result }
            last = result
            let status = (result.json?["operation"] as? [String: Any])?["status"] as? String
            if status == "succeeded" || status == "failed" { return result }
            Thread.sleep(forTimeInterval: 0.25)
        }
        return ApiResult(ok: false, status: last?.status, json: last?.json, unauthorized: false,
                         message: "operation timed out")
    }

    func daemonLog(bytes: Int = 16384) -> ApiResult {
        get("/v1/daemon/log", query: ["bytes": String(bytes)])
    }

    func serviceLog(_ serviceId: String, cursor: Int?, generation: Int?, limit: Int = 16384) -> ApiResult {
        var query = ["limit": String(limit)]
        if let cursor { query["cursor"] = String(cursor) }
        if let generation { query["generation"] = String(generation) }
        let escaped = serviceId.addingPercentEncoding(withAllowedCharacters: .urlPathAllowed) ?? serviceId
        return get("/v1/logs/\(escaped)", query: query)
    }

    private func send(_ method: String, _ path: String, query: [String: String], body: [String: Any]?) -> ApiResult {
        guard let session = SessionStore.shared.session(root) else {
            return .session
        }
        var components = URLComponents()
        components.scheme = "http"
        components.host = "127.0.0.1"
        components.port = session.port
        components.path = path
        if !query.isEmpty {
            components.queryItems = query.map { URLQueryItem(name: $0.key, value: $0.value) }
        }
        guard let url = components.url else {
            return .transport("bad url")
        }
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
            request.httpBody = try? JSONSerialization.data(withJSONObject: body)
        }

        var resultData: Data?
        var resultResponse: URLResponse?
        var resultError: Error?
        let semaphore = DispatchSemaphore(value: 0)
        URLSession.shared.dataTask(with: request) { data, response, error in
            resultData = data
            resultResponse = response
            resultError = error
            semaphore.signal()
        }.resume()
        semaphore.wait()

        if let resultError {
            return .transport(resultError.localizedDescription)
        }
        let http = resultResponse as? HTTPURLResponse
        let status = http?.statusCode
        if status == 401 {
            SessionStore.shared.forgetRoot(root)
            return .session
        }
        let decoded = resultData.flatMap { try? JSONSerialization.jsonObject(with: $0) } as? [String: Any]
        var message = ""
        if let error = decoded?["error"] as? [String: Any], let text = error["message"] as? String {
            message = text
        }
        return ApiResult(ok: (200..<300).contains(status ?? 0), status: status, json: decoded,
                         unauthorized: false, message: message)
    }
}
