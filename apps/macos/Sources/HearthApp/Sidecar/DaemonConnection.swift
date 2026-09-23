// Spawns `hearthd --root <workspace> manager ensure --json` — a one-shot subprocess call, not a
// long-lived one (the daemon it starts is itself detached; this app never holds a handle to the
// daemon process, only to the short-lived `hearthd manager ensure` call that finds-or-starts it and
// hands back a connection).

import Foundation

enum DaemonConnectionError: Error, LocalizedError {
    case sidecarUnavailable(Error)
    case processFailed(exitCode: Int32, stderr: String)
    case malformedOutput(String)
    case timedOut(seconds: Int)

    var errorDescription: String? {
        switch self {
        case .sidecarUnavailable(let error): return error.localizedDescription
        case .processFailed(let code, let stderr): return "hearthd exited \(code): \(stderr.trimmingCharacters(in: .whitespacesAndNewlines))"
        case .malformedOutput(let raw): return "hearthd printed unexpected output: \(raw)"
        case .timedOut(let seconds): return "hearthd did not respond within \(seconds)s"
        }
    }
}

enum DaemonConnection {
    /// Ensures a daemon is running for `root` (spawning one if needed) and returns everything a
    /// `ManagerClient` needs to talk to it directly.
    static func ensure(root: String) async throws -> ManagerConnection {
        let output = try await runHearthd(root: root, args: ["manager", "ensure", "--json"])
        guard let data = output.data(using: .utf8), let connection = try? JSONDecoder().decode(ManagerConnection.self, from: data) else {
            throw DaemonConnectionError.malformedOutput(output)
        }
        return connection
    }

    /// Stops the daemon for `root` (and every service it manages) — `hearthd manager stop`, which only
    /// returns once the daemon process has exited (up to its own 300s limit), so this outlasts it.
    static func stopManager(root: String) async throws {
        _ = try await runHearthd(root: root, args: ["manager", "stop", "--json"], timeout: .seconds(330))
    }

    /// Replaces the daemon for `root` with a freshly started one — `hearthd manager restart`, which
    /// shuts the old daemon down *without* stopping its services (they are detached and re-adopted
    /// by the new daemon), waits for it to exit, then ensures a new one and prints its connection.
    /// Like `stopManager`, this outlasts the daemon's own 300s shutdown limit.
    static func restart(root: String) async throws -> ManagerConnection {
        let output = try await runHearthd(root: root, args: ["manager", "restart", "--json"], timeout: .seconds(330))
        guard let data = output.data(using: .utf8), let connection = try? JSONDecoder().decode(ManagerConnection.self, from: data) else {
            throw DaemonConnectionError.malformedOutput(output)
        }
        return connection
    }

    /// Re-reads `root`'s config file and pushes it to the running daemon — `hearthd manager reload`
    /// (loads fresh from disk on every `hearthd` invocation). Used by `ConfigFileWatcher` to react to
    /// a `hearth.yaml` edit.
    static func reload(root: String) async throws {
        _ = try await runHearthd(root: root, args: ["manager", "reload", "--json"])
    }

    private static func runHearthd(root: String, args: [String], timeout: Duration = .seconds(60)) async throws -> String {
        let process = Process()
        guard let hearthdBinary = SidecarLocator.findHearthdBinary() else {
            throw DaemonConnectionError.sidecarUnavailable(SidecarLocatorError.hearthdBinaryNotFound)
        }
        process.executableURL = URL(fileURLWithPath: hearthdBinary)
        process.arguments = ["--root", root] + args
        let stdout = Pipe()
        let stderr = Pipe()
        process.standardOutput = stdout
        process.standardError = stderr

        // Both pipes are drained CONCURRENTLY with the process running, not from inside
        // `terminationHandler`. A child that writes more than the ~64KB pipe buffer (a Bun stack
        // trace from a broken catalog, a verbose validation error) blocks forever on `write` if
        // nothing is reading — so it never exits, `terminationHandler` never fires, and the
        // continuation is never resumed. The UI's only symptom would be "Starting daemon…" forever.
        let collector = OutputCollector()
        stdout.fileHandleForReading.readabilityHandler = { handle in
            let chunk = handle.availableData
            if chunk.isEmpty { handle.readabilityHandler = nil } else { collector.appendOut(chunk) }
        }
        stderr.fileHandleForReading.readabilityHandler = { handle in
            let chunk = handle.availableData
            if chunk.isEmpty { handle.readabilityHandler = nil } else { collector.appendErr(chunk) }
        }

        let finish: (Process) -> Void = { proc in
            stdout.fileHandleForReading.readabilityHandler = nil
            stderr.fileHandleForReading.readabilityHandler = nil
            // Whatever landed in the buffer after the last readability callback.
            collector.appendOut(stdout.fileHandleForReading.availableData)
            collector.appendErr(stderr.fileHandleForReading.availableData)
            _ = proc
        }

        // A hard ceiling so a wedged sidecar surfaces as an error the user can retry from, rather
        // than leaving the workspace stuck in `.connecting` with no reachable Retry button. The
        // realistic wedge is `loginShellWhich` running `$SHELL -ilc` against an rc file that blocks.
        return try await withThrowingTaskGroup(of: String.self) { group in
            group.addTask {
                try await withCheckedThrowingContinuation { continuation in
                    let resumed = ResumeOnce()
                    process.terminationHandler = { proc in
                        finish(proc)
                        guard resumed.claim() else { return }
                        let out = collector.outString()
                        let err = collector.errString()
                        if proc.terminationStatus == 0 {
                            continuation.resume(returning: out)
                        } else {
                            continuation.resume(throwing: DaemonConnectionError.processFailed(exitCode: proc.terminationStatus, stderr: err.isEmpty ? out : err))
                        }
                    }
                    do {
                        try process.run()
                    } catch {
                        guard resumed.claim() else { return }
                        continuation.resume(throwing: DaemonConnectionError.sidecarUnavailable(error))
                    }
                }
            }
            group.addTask {
                try await Task.sleep(for: timeout)
                throw DaemonConnectionError.timedOut(seconds: Int(timeout.components.seconds))
            }
            defer { group.cancelAll() }
            guard let first = try await group.next() else {
                throw DaemonConnectionError.timedOut(seconds: Int(timeout.components.seconds))
            }
            return first
        }
    }
}

/// Accumulates the child's output off the pipe's serial reader queue. `NSLock` rather than an actor
/// because `readabilityHandler` is a synchronous callback.
private final class OutputCollector: @unchecked Sendable {
    private let lock = NSLock()
    private var out = Data()
    private var err = Data()

    func appendOut(_ data: Data) {
        guard !data.isEmpty else { return }
        lock.lock(); out.append(data); lock.unlock()
    }
    func appendErr(_ data: Data) {
        guard !data.isEmpty else { return }
        lock.lock(); err.append(data); lock.unlock()
    }
    func outString() -> String {
        lock.lock(); defer { lock.unlock() }
        return String(data: out, encoding: .utf8) ?? ""
    }
    func errString() -> String {
        lock.lock(); defer { lock.unlock() }
        return String(data: err, encoding: .utf8) ?? ""
    }
}

/// `CheckedContinuation` traps on a second resume. `terminationHandler` and the `process.run()`
/// catch can both fire, so exactly one of them is allowed to win.
private final class ResumeOnce: @unchecked Sendable {
    private let lock = NSLock()
    private var done = false
    func claim() -> Bool {
        lock.lock(); defer { lock.unlock() }
        if done { return false }
        done = true
        return true
    }
}
