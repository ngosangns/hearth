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
    case incompatibleProtocol(daemon: Int, app: Int)

    var errorDescription: String? {
        switch self {
        case .sidecarUnavailable(let error): return error.localizedDescription
        case .processFailed(let code, let stderr): return "hearthd exited \(code): \(stderr.trimmingCharacters(in: .whitespacesAndNewlines))"
        case .malformedOutput(let raw): return "hearthd printed unexpected output: \(raw)"
        case .timedOut(let seconds): return "hearthd did not respond within \(seconds)s"
        case .incompatibleProtocol(let daemon, let app):
            return "The hearthd daemon speaks protocol \(daemon), but this app speaks protocol \(app). Update Hearth.app and hearthd to the same release, then use Restart Daemon."
        }
    }
}

enum DaemonConnection {
    /// Ensures a daemon is running for `root` (spawning one if needed) and returns everything a
    /// `ManagerClient` needs to talk to it directly.
    static func ensure(root: String) async throws -> ManagerConnection {
        try decodeConnection(try await runHearthd(root: root, args: ["manager", "ensure", "--json"]))
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
        try decodeConnection(try await runHearthd(root: root, args: ["manager", "restart", "--json"], timeout: .seconds(330)))
    }

    /// Re-reads `root`'s config file and pushes it to the running daemon — `hearthd manager reload`
    /// (loads fresh from disk on every `hearthd` invocation). Used by `ConfigFileWatcher` to react to
    /// a `hearth.yaml` edit.
    static func reload(root: String) async throws {
        _ = try await runHearthd(root: root, args: ["manager", "reload", "--json"])
    }

    /// Ensures the machine-global smp daemon (`hearthd smp`) is running and returns the same
    /// `ManagerConnection` shape — `hearthd shared ensure --json`. smp owns no project root, so the
    /// subprocess runs without `--root`.
    static func ensureShared() async throws -> ManagerConnection {
        try decodeConnection(try await runHearthd(root: nil, args: ["shared", "ensure", "--json"]))
    }

    /// Decodes an `ensure`/`restart` connection and refuses one this build cannot talk to: a
    /// `PROTOCOL_VERSION` bump is breaking for every client, and an old daemon would otherwise only
    /// surface as a `426` on the first request or as scattered decode errors.
    static func decodeConnection(_ output: String) throws -> ManagerConnection {
        guard let data = output.data(using: .utf8), let connection = try? JSONDecoder().decode(ManagerConnection.self, from: data) else {
            throw DaemonConnectionError.malformedOutput(output)
        }
        guard connection.protocolVersion == ManagerClient.supportedProtocolVersion else {
            throw DaemonConnectionError.incompatibleProtocol(daemon: connection.protocolVersion, app: ManagerClient.supportedProtocolVersion)
        }
        return connection
    }

    /// A hard ceiling so a wedged sidecar surfaces as an error the user can retry from, rather than
    /// leaving the workspace stuck in `.connecting` with no reachable Retry button. The binary
    /// lookup is inside the timed region: its last resort runs `$SHELL -ilc`, and an rc file that
    /// blocks is the realistic wedge.
    private static func runHearthd(root: String?, args: [String], timeout: Duration = .seconds(60)) async throws -> String {
        try await Subprocess.withTimeout(timeout) {
            guard let hearthdBinary = await SidecarLocator.findHearthdBinary() else {
                throw DaemonConnectionError.sidecarUnavailable(SidecarLocatorError.hearthdBinaryNotFound)
            }
            let result = try await Subprocess.run(
                URL(fileURLWithPath: hearthdBinary),
                arguments: (root.map { ["--root", $0] } ?? []) + args
            )
            guard result.status == 0 else {
                throw DaemonConnectionError.processFailed(exitCode: result.status, stderr: result.stderr.isEmpty ? result.stdout : result.stderr)
            }
            return result.stdout
        }
    }
}

/// One-shot child processes whose wait honours task cancellation — which is what makes
/// `withTimeout` able to return at all. A continuation that only resumes on exit kept the task
/// group (and so the caller) waiting on a hung child forever, however the timeout fired.
enum Subprocess {
    struct Output {
        let status: Int32
        let stdout: String
        let stderr: String
    }

    /// How long a cancelled child gets after SIGTERM before SIGKILL — an interactive login shell
    /// ignores SIGTERM outright.
    static let killGrace: TimeInterval = 2

    /// How long to keep collecting output after the child exits. A grandchild that inherited the
    /// pipes (a daemonized helper, a background job in an rc file) can hold them open forever, so
    /// EOF is not a precondition for returning.
    static let drainGrace: TimeInterval = 1

    /// Runs `work`, throwing `timedOut` if it has not finished within `timeout`. `work` is
    /// cancelled on timeout; it must honour cancellation for this to return promptly.
    static func withTimeout<T: Sendable>(_ timeout: Duration, _ work: @escaping @Sendable () async throws -> T) async throws -> T {
        try await withThrowingTaskGroup(of: T.self) { group in
            group.addTask(operation: work)
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

    /// Runs `executable` to completion. Cancelling the calling task terminates the child (SIGTERM,
    /// then SIGKILL after `killGrace`) and throws `CancellationError`.
    static func run(_ executable: URL, arguments: [String]) async throws -> Output {
        let process = Process()
        process.executableURL = executable
        process.arguments = arguments
        process.standardInput = FileHandle.nullDevice
        let stdout = Pipe()
        let stderr = Pipe()
        process.standardOutput = stdout
        process.standardError = stderr
        let handle = ProcessHandle(process)

        let output: Output = try await withTaskCancellationHandler {
            try await withCheckedThrowingContinuation { continuation in
                // Both pipes are drained CONCURRENTLY with the process running, not from inside
                // `terminationHandler`. A child that writes more than the ~64KB pipe buffer (a long
                // validation error) blocks forever on `write` if nothing is reading — so it never
                // exits and the continuation is never resumed.
                let collector = OutputCollector()
                let resumed = ResumeOnce()
                let settled = DispatchGroup()
                settled.enter() // stdout EOF
                settled.enter() // stderr EOF
                settled.enter() // exit
                let finish: @Sendable () -> Void = {
                    guard resumed.claim() else { return }
                    stdout.fileHandleForReading.readabilityHandler = nil
                    stderr.fileHandleForReading.readabilityHandler = nil
                    continuation.resume(returning: Output(status: process.terminationStatus, stdout: collector.outString(), stderr: collector.errString()))
                }
                stdout.fileHandleForReading.readabilityHandler = { file in
                    let chunk = file.availableData
                    if chunk.isEmpty {
                        file.readabilityHandler = nil
                        if collector.markEOF(.out) { settled.leave() }
                    } else {
                        collector.append(chunk, to: .out)
                    }
                }
                stderr.fileHandleForReading.readabilityHandler = { file in
                    let chunk = file.availableData
                    if chunk.isEmpty {
                        file.readabilityHandler = nil
                        if collector.markEOF(.err) { settled.leave() }
                    } else {
                        collector.append(chunk, to: .err)
                    }
                }
                process.terminationHandler = { _ in
                    settled.leave()
                    DispatchQueue.global().asyncAfter(deadline: .now() + drainGrace, execute: finish)
                }
                settled.notify(queue: .global(), execute: finish)
                do {
                    try handle.launch()
                } catch {
                    guard resumed.claim() else { return }
                    stdout.fileHandleForReading.readabilityHandler = nil
                    stderr.fileHandleForReading.readabilityHandler = nil
                    continuation.resume(throwing: error is CancellationError ? error : DaemonConnectionError.sidecarUnavailable(error))
                }
            }
        } onCancel: {
            handle.cancel()
        }
        if handle.wasCancelled { throw CancellationError() }
        return output
    }
}

/// Serializes launch against cancellation: `onCancel` can run before the process is launched (the
/// task was already cancelled), and `Process.terminate()` on an unlaunched process raises.
private final class ProcessHandle: @unchecked Sendable {
    private let lock = NSLock()
    private let process: Process
    private var launched = false
    private var cancelled = false

    init(_ process: Process) { self.process = process }

    var wasCancelled: Bool {
        lock.lock(); defer { lock.unlock() }
        return cancelled
    }

    func launch() throws {
        lock.lock(); defer { lock.unlock() }
        if cancelled { throw CancellationError() }
        try process.run()
        launched = true
    }

    func cancel() {
        lock.lock()
        cancelled = true
        let running = launched && process.isRunning
        lock.unlock()
        guard running else { return }
        process.terminate()
        let pid = process.processIdentifier
        DispatchQueue.global().asyncAfter(deadline: .now() + Subprocess.killGrace) { [process] in
            if process.isRunning { kill(pid, SIGKILL) }
        }
    }
}

/// Accumulates the child's output off the pipe's serial reader queue. `NSLock` rather than an actor
/// because `readabilityHandler` is a synchronous callback.
private final class OutputCollector: @unchecked Sendable {
    enum Stream { case out, err }

    private let lock = NSLock()
    private var out = Data()
    private var err = Data()
    private var closed: Set<Stream> = []

    func append(_ data: Data, to stream: Stream) {
        lock.lock(); defer { lock.unlock() }
        switch stream {
        case .out: out.append(data)
        case .err: err.append(data)
        }
    }
    /// True only for the first EOF on `stream` — a `DispatchGroup` traps on an unbalanced leave.
    func markEOF(_ stream: Stream) -> Bool {
        lock.lock(); defer { lock.unlock() }
        return closed.insert(stream).inserted
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

/// `CheckedContinuation` traps on a second resume. EOF-plus-exit, the post-exit drain deadline and
/// the `launch()` catch can each fire, so exactly one of them is allowed to win.
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
