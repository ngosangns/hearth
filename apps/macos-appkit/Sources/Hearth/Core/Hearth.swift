import Foundation

/// Runs the `hearth` binary with an argv array — never a shell string.
/// Binary order: bundled `Contents/extras/hearth`, then `~/.local/bin/hearth`.
final class Hearth {
    static let shared = Hearth()

    private init() {}

    var binaryPath: String {
        let bundled = Bundle.main.bundleURL
            .appendingPathComponent("Contents/extras/hearth").path
        if FileManager.default.isExecutableFile(atPath: bundled) {
            return bundled
        }
        let home = FileManager.default.homeDirectoryForCurrentUser.path
        return "\(home)/.local/bin/hearth"
    }

    var binaryExists: Bool {
        FileManager.default.isExecutableFile(atPath: binaryPath)
    }

    /// `hearth --root <root> manager <subcommand> --json`.
    /// Call from a background queue — `run` blocks until exit.
    func manager(root: String, _ subcommand: String) -> CommandResult {
        let timeout: TimeInterval = switch subcommand {
        case "ensure": 90
        case "stop", "restart": 300
        case "reload": 60
        default: 20
        }
        return run(["--root", root, "manager", subcommand, "--json"], cwd: root, timeout: timeout)
    }

    /// `hearth <args>` in `cwd`. A nil timeout waits without a bound
    /// (shared install and start), matching the PHP runner.
    func run(_ args: [String], cwd: String, timeout: TimeInterval? = 20) -> CommandResult {
        let path = binaryPath
        guard FileManager.default.isExecutableFile(atPath: path) else {
            return .missingBinary()
        }
        let process = Process()
        process.executableURL = URL(fileURLWithPath: path)
        process.arguments = args
        process.currentDirectoryURL = URL(fileURLWithPath: cwd)
        let outPipe = Pipe()
        let errPipe = Pipe()
        process.standardOutput = outPipe
        process.standardError = errPipe

        var timedOut = false
        var watchdog: DispatchWorkItem?
        if let timeout {
            let item = DispatchWorkItem { [weak process] in
                timedOut = true
                process?.terminate()
            }
            watchdog = item
            DispatchQueue.global().asyncAfter(deadline: .now() + timeout, execute: item)
        }

        do {
            try process.run()
        } catch {
            watchdog?.cancel()
            return CommandResult(ok: false, exit: nil, stdout: "", stderr: error.localizedDescription, json: nil)
        }
        // Drain both pipes while the child runs — a full pipe buffer would otherwise
        // deadlock a chatty child waiting on write before exit.
        var outData = Data()
        var errData = Data()
        let drains = DispatchGroup()
        drains.enter()
        DispatchQueue.global().async { outData = outPipe.fileHandleForReading.readDataToEndOfFile(); drains.leave() }
        drains.enter()
        DispatchQueue.global().async { errData = errPipe.fileHandleForReading.readDataToEndOfFile(); drains.leave() }
        process.waitUntilExit()
        drains.wait()
        watchdog?.cancel()

        let out = String(data: outData, encoding: .utf8) ?? ""
        let err = String(data: errData, encoding: .utf8) ?? ""
        let stderr = timedOut && err.isEmpty ? "command timed out" : err
        return CommandResult(
            ok: !timedOut && process.terminationStatus == 0,
            exit: process.terminationStatus,
            stdout: out,
            stderr: stderr,
            json: CommandResult.lastJson(out)
        )
    }
}
