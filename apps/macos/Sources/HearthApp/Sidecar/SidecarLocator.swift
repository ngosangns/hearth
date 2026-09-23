// Finds the compiled `hearthd` binary `DaemonConnection` spawns. Order: env override, a packaged
// `.app`'s bundled copy (`Contents/Resources/hearthd/bin/hearthd` — see `scripts/build-app.sh`), the
// installed `/Applications` copy, other known install locations, this checkout's `cargo build`
// output, then a login-shell `which hearthd`.
//
// A GUI-launched app inherits launchd's bare `PATH`, so known absolute paths are checked before
// `command -v`. There is no TypeScript/`bun` fallback.

import Foundation

enum SidecarLocatorError: Error, LocalizedError {
    case hearthdBinaryNotFound

    var errorDescription: String? {
        "Could not find a compiled `hearthd` binary. Package the app (`task macos:install`), run `task rust:install`, or `cargo build --release -p hearthd` from rust/ in this checkout."
    }
}

enum SidecarLocator {
    /// Installed `hearthd` lives inside Hearth.app (`task rust:install` / `macos:install`).
    private static let knownHearthdBinaryPaths = [
        "/Applications/Hearth.app/Contents/Resources/hearthd/bin/hearthd",
        "/usr/local/bin/hearthd",
        "\(NSHomeDirectory())/.cargo/bin/hearthd",
    ]

    /// Set `HEARTHD_DEBUG=1` to log which `hearthd` this resolved to on stderr.
    private static var debugLoggingEnabled: Bool { ProcessInfo.processInfo.environment["HEARTHD_DEBUG"] == "1" }
    private static func debugLog(_ message: @autoclosure () -> String) {
        guard debugLoggingEnabled else { return }
        FileHandle.standardError.write(Data("[SidecarLocator] \(message())\n".utf8))
    }

    static func findHearthdBinary() -> String? {
        if let override = ProcessInfo.processInfo.environment["HEARTH_BIN_PATH"], FileManager.default.isExecutableFile(atPath: override) {
            debugLog("hearthd (binary): \(override) (HEARTH_BIN_PATH override)")
            return override
        }
        if let bundled = Bundle.main.resourceURL?.appendingPathComponent("hearthd/bin/hearthd").path, FileManager.default.isExecutableFile(atPath: bundled) {
            debugLog("hearthd (binary): \(bundled) (bundled app resource)")
            return bundled
        }
        for path in knownHearthdBinaryPaths where FileManager.default.isExecutableFile(atPath: path) {
            debugLog("hearthd (binary): \(path) (known install location)")
            return path
        }
        if let dev = devRepoHearthdBinary() {
            debugLog("hearthd (binary): \(dev) (dev repo `cargo build` output)")
            return dev
        }
        let found = loginShellWhich("hearthd")
        debugLog("hearthd (binary): \(found ?? "not found") (login shell `command -v`)")
        return found
    }

    /// Resolves `rust/target/{release,debug}/hearthd` relative to this source file's own on-disk path —
    /// only reached when there's no bundled/installed binary. Release preferred over debug.
    private static func devRepoHearthdBinary() -> String? {
        var url = URL(fileURLWithPath: #filePath) // .../apps/macos/Sources/HearthApp/Sidecar/SidecarLocator.swift
        for _ in 0..<6 { url.deleteLastPathComponent() } // -> repo root
        for profile in ["release", "debug"] {
            let candidate = url.appendingPathComponent("rust/target/\(profile)/hearthd").path
            if FileManager.default.isExecutableFile(atPath: candidate) { return candidate }
        }
        return nil
    }

    private static func loginShellWhich(_ name: String) -> String? {
        let shell = ProcessInfo.processInfo.environment["SHELL"] ?? "/bin/zsh"
        let process = Process()
        process.executableURL = URL(fileURLWithPath: shell)
        process.arguments = ["-ilc", "command -v \(name)"]
        let pipe = Pipe()
        process.standardOutput = pipe
        process.standardError = FileHandle.nullDevice
        do {
            try process.run()
            process.waitUntilExit()
            let output = String(data: pipe.fileHandleForReading.readDataToEndOfFile(), encoding: .utf8) ?? ""
            let trimmed = output.trimmingCharacters(in: .whitespacesAndNewlines)
            return trimmed.isEmpty ? nil : trimmed
        } catch {
            return nil
        }
    }
}
