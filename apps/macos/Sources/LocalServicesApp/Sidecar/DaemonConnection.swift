// Spawns `bun run <lsd.ts> --root <workspace> manager ensure --json` — a one-shot subprocess call,
// not a long-lived one (the daemon it starts is itself detached, per `runDaemon`'s
// `Bun.spawn(..., detached: true)`; this app never holds a handle to the daemon process, only to the
// short-lived `lsd manager ensure` call that finds-or-starts it and hands back a connection). Mirrors
// exactly what `lsd manager ensure` does for a terminal user — see src/cli/localctl.ts's `ensure`.

import Foundation

enum DaemonConnectionError: Error, LocalizedError {
    case sidecarUnavailable(Error)
    case processFailed(exitCode: Int32, stderr: String)
    case malformedOutput(String)

    var errorDescription: String? {
        switch self {
        case .sidecarUnavailable(let error): return error.localizedDescription
        case .processFailed(let code, let stderr): return "lsd exited \(code): \(stderr.trimmingCharacters(in: .whitespacesAndNewlines))"
        case .malformedOutput(let raw): return "lsd printed unexpected output: \(raw)"
        }
    }
}

enum DaemonConnection {
    /// Ensures a daemon is running for `root` (spawning one if needed) and returns everything a
    /// `ManagerClient` needs to talk to it directly.
    static func ensure(root: String) async throws -> ManagerConnection {
        let output = try await runLsd(root: root, args: ["manager", "ensure", "--json"])
        guard let data = output.data(using: .utf8), let connection = try? JSONDecoder().decode(ManagerConnection.self, from: data) else {
            throw DaemonConnectionError.malformedOutput(output)
        }
        return connection
    }

    /// Stops the daemon for `root` (and every service it manages) — `lsd manager stop`.
    static func stopManager(root: String) async throws {
        _ = try await runLsd(root: root, args: ["manager", "stop", "--json"])
    }

    private static func runLsd(root: String, args: [String]) async throws -> String {
        guard let bun = SidecarLocator.findBun() else { throw DaemonConnectionError.sidecarUnavailable(SidecarLocatorError.bunNotFound) }
        guard let lsdEntry = SidecarLocator.findLsdEntry() else { throw DaemonConnectionError.sidecarUnavailable(SidecarLocatorError.lsdEntryNotFound("<unresolved>")) }

        let process = Process()
        process.executableURL = URL(fileURLWithPath: bun)
        process.arguments = ["run", lsdEntry, "--root", root] + args
        let stdout = Pipe()
        let stderr = Pipe()
        process.standardOutput = stdout
        process.standardError = stderr

        return try await withCheckedThrowingContinuation { continuation in
            process.terminationHandler = { proc in
                let outData = stdout.fileHandleForReading.readDataToEndOfFile()
                let errData = stderr.fileHandleForReading.readDataToEndOfFile()
                let out = String(data: outData, encoding: .utf8) ?? ""
                let err = String(data: errData, encoding: .utf8) ?? ""
                if proc.terminationStatus == 0 {
                    continuation.resume(returning: out)
                } else {
                    continuation.resume(throwing: DaemonConnectionError.processFailed(exitCode: proc.terminationStatus, stderr: err.isEmpty ? out : err))
                }
            }
            do {
                try process.run()
            } catch {
                continuation.resume(throwing: DaemonConnectionError.sidecarUnavailable(error))
            }
        }
    }
}
