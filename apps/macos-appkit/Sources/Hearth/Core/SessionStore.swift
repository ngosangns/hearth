import Foundation

/// In-memory daemon sessions, keyed by workspace root. The AppKit process owns its
/// whole lifetime, so tokens never touch disk — the equivalent of `DaemonMemory`'s
/// SysV shared-memory backend. A `manager ensure` (or `restart`) re-attaches.
final class SessionStore {
    static let shared = SessionStore()

    struct Session {
        let token: String
        let port: Int
        let protocolVersion: Int?
    }

    private var sessions: [String: Session] = [:]
    private var stopped: Set<String> = []

    /// Keep the `manager ensure` payload: token, port, protocolVersion.
    func put(root: String, payload: [String: Any]) {
        guard let token = payload["token"] as? String, !token.isEmpty else { return }
        let port = (payload["port"] as? Int) ?? Int((payload["port"] as? String) ?? "") ?? 0
        guard port > 0 else { return }
        let proto = (payload["protocolVersion"] as? Int) ?? Int((payload["protocolVersion"] as? String) ?? "")
        sessions[root] = Session(token: token, port: port, protocolVersion: proto)
    }

    func session(_ root: String) -> Session? { sessions[root] }
    func token(_ root: String) -> String? { sessions[root]?.token }
    func hasToken(_ root: String) -> Bool { sessions[root] != nil }
    func forgetRoot(_ root: String) { sessions.removeValue(forKey: root) }

    func markStopped(_ id: String) { stopped.insert(id) }
    func isStopped(_ id: String) -> Bool { stopped.contains(id) }
    func clearStopped(_ id: String) { stopped.remove(id) }
    func forgetId(_ id: String) { stopped.remove(id) }
}
