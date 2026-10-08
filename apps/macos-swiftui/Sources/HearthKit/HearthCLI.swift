import Foundation

/// Result of one `hearth` invocation.
public struct CommandResult: Sendable {
    public let ok: Bool
    public let exit: Int32?
    public let stdout: String
    public let stderr: String

    public init(ok: Bool, exit: Int32?, stdout: String, stderr: String) {
        self.ok = ok
        self.exit = exit
        self.stdout = stdout
        self.stderr = stderr
    }

    /// stderr when present, else stdout. Tokens are redacted.
    public var visibleMessage: String {
        let err = stderr.trimmingCharacters(in: .whitespacesAndNewlines)
        let text = err.isEmpty ? stdout.trimmingCharacters(in: .whitespacesAndNewlines) : err
        return Self.redact(text)
    }

    /// Decodes the last JSON object in stdout.
    public func decode<T: Decodable>(_ type: T.Type) -> T? {
        Self.lastJSONData(stdout).flatMap { try? JSONDecoder().decode(T.self, from: $0) }
    }

    /// The whole document when stdout is one JSON object (pretty or compact); otherwise the
    /// last line that is a standalone object. A nested one-line object inside a pretty
    /// document must not win over the document.
    public static func lastJSONData(_ stdout: String) -> Data? {
        let trimmed = stdout.trimmingCharacters(in: .whitespacesAndNewlines)
        if trimmed.hasPrefix("{"), let data = trimmed.data(using: .utf8), isObject(data) {
            return data
        }
        var found: Data?
        for line in stdout.split(whereSeparator: \.isNewline) {
            let row = line.trimmingCharacters(in: .whitespaces)
            guard row.hasPrefix("{"), let data = row.data(using: .utf8), isObject(data) else { continue }
            found = data
        }
        return found
    }

    public static func redact(_ text: String) -> String {
        text.replacingOccurrences(
            of: "(\"token\"\\s*:\\s*\")[^\"]+",
            with: "$1[redacted]",
            options: .regularExpression
        )
    }

    private static func isObject(_ data: Data) -> Bool {
        (try? JSONSerialization.jsonObject(with: data)) is [String: Any]
    }
}

/// Thread-safe accumulator for pipe output.
private final class Buffer: @unchecked Sendable {
    private let lock = NSLock()
    private var data = Data()
    func append(_ d: Data) { lock.lock(); data.append(d); lock.unlock() }
    var string: String { lock.lock(); defer { lock.unlock() }; return String(decoding: data, as: UTF8.self) }
}

/// Lets cancel / timeout / exit race safely and records why the child was killed.
private final class ProcessBox: @unchecked Sendable {
    let process = Process()
    private let lock = NSLock()
    private var _timedOut = false
    var timedOut: Bool { lock.lock(); defer { lock.unlock() }; return _timedOut }
    func terminate(timedOut: Bool = false) {
        lock.lock(); defer { lock.unlock() }
        guard process.isRunning else { return }
        if timedOut { _timedOut = true }
        process.terminate()
    }
}

/// Async wrapper around the `hearth` binary. The Rust CLI stays the single source of
/// truth; this type only spawns it, with an argv array and never a shell string.
public struct HearthCLI: Sendable {
    public let executable: URL

    public init(executable: URL) { self.executable = executable }

    /// `HEARTH_BIN` -> bundled `Contents/extras/hearth` -> `~/.local/bin/hearth`.
    public static func locate(
        bundle: Bundle = .main,
        environment: [String: String] = ProcessInfo.processInfo.environment,
        home: String = NSHomeDirectory()
    ) -> HearthCLI? {
        let fm = FileManager.default
        var candidates: [String] = []
        if let env = environment["HEARTH_BIN"], !env.isEmpty { candidates.append(env) }
        candidates.append(bundle.bundleURL.appendingPathComponent("Contents/extras/hearth").path)
        candidates.append("\(home)/.local/bin/hearth")
        return candidates
            .first(where: { fm.isExecutableFile(atPath: $0) })
            .map { HearthCLI(executable: URL(fileURLWithPath: $0)) }
    }

    /// A GUI app inherits a bare PATH. `hearth` resolves its own tool directories for the
    /// daemon; this only keeps the CLI's own helpers (git, docker probes) reachable.
    private static var childEnvironment: [String: String] {
        var env = ProcessInfo.processInfo.environment
        let extra = ["\(NSHomeDirectory())/.local/bin", "/opt/homebrew/bin", "/usr/local/bin", "/usr/bin", "/bin"]
        let current = (env["PATH"] ?? "").split(separator: ":").map(String.init)
        env["PATH"] = (current + extra.filter { !current.contains($0) }).joined(separator: ":")
        return env
    }

    /// Default timeout per `manager` subcommand.
    public static func managerTimeout(_ subcommand: String) -> Duration {
        switch subcommand {
        case "ensure": .seconds(90)
        case "stop", "restart": .seconds(300)
        case "reload": .seconds(60)
        default: .seconds(20)
        }
    }

    /// `hearth --root <root> manager <subcommand> --json`.
    public func manager(root: String, _ subcommand: String) async -> CommandResult {
        await run(["--root", root, "manager", subcommand, "--json"], cwd: root,
                  timeout: Self.managerTimeout(subcommand))
    }

    /// Runs `hearth <args>` in `cwd`. A nil timeout waits without bound (shared install and
    /// start). Honors Task cancellation.
    public func run(_ args: [String], cwd: String, timeout: Duration? = .seconds(20)) async -> CommandResult {
        let box = ProcessBox()
        let p = box.process
        p.executableURL = executable
        p.arguments = args
        p.environment = Self.childEnvironment
        p.currentDirectoryURL = URL(fileURLWithPath: cwd)
        let out = Pipe(), err = Pipe()
        p.standardOutput = out
        p.standardError = err
        p.standardInput = FileHandle.nullDevice
        let outBuf = Buffer(), errBuf = Buffer()
        out.fileHandleForReading.readabilityHandler = { outBuf.append($0.availableData) }
        err.fileHandleForReading.readabilityHandler = { errBuf.append($0.availableData) }

        let timeoutTask: Task<Void, Never>? = timeout.map { limit in
            Task {
                try? await Task.sleep(for: limit)
                if !Task.isCancelled { box.terminate(timedOut: true) }
            }
        }
        defer { timeoutTask?.cancel() }

        let status: Int32? = await withTaskCancellationHandler {
            await withCheckedContinuation { (cont: CheckedContinuation<Int32?, Never>) in
                p.terminationHandler = { proc in
                    out.fileHandleForReading.readabilityHandler = nil
                    err.fileHandleForReading.readabilityHandler = nil
                    if let rest = try? out.fileHandleForReading.readToEnd() { outBuf.append(rest) }
                    if let rest = try? err.fileHandleForReading.readToEnd() { errBuf.append(rest) }
                    cont.resume(returning: proc.terminationStatus)
                }
                do { try p.run() } catch {
                    out.fileHandleForReading.readabilityHandler = nil
                    err.fileHandleForReading.readabilityHandler = nil
                    errBuf.append(Data(error.localizedDescription.utf8))
                    cont.resume(returning: nil)
                }
            }
        } onCancel: { box.terminate() }

        let stderr = errBuf.string
        if box.timedOut {
            return CommandResult(ok: false, exit: status, stdout: outBuf.string,
                                 stderr: stderr.isEmpty ? "command timed out" : stderr)
        }
        return CommandResult(ok: status == 0, exit: status, stdout: outBuf.string, stderr: stderr)
    }

    public func version() async -> String? {
        let result = await run(["--version"], cwd: "/", timeout: .seconds(5))
        let text = (result.stdout + result.stderr).trimmingCharacters(in: .whitespacesAndNewlines)
        return result.ok && !text.isEmpty ? text : nil
    }
}
