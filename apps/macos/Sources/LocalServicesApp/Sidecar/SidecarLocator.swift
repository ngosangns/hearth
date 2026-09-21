// Finds the compiled `lsd` binary `DaemonConnection` spawns. Order: env override, a packaged
// `.app`'s bundled copy (`Contents/Resources/lsd/bin/lsd` — see `scripts/build-app.sh`), the
// installed `/Applications` copy, other known install locations, this checkout's `cargo build`
// output, then a login-shell `which lsd`.
//
// A GUI-launched app inherits launchd's bare `PATH`, so known absolute paths are checked before
// `command -v`. There is no TypeScript/`bun` fallback.

import Foundation

enum SidecarLocatorError: Error, LocalizedError {
    case lsdBinaryNotFound

    var errorDescription: String? {
        "Could not find a compiled `lsd` binary. Package the app (`task macos:install`), run `task rust:install`, or `cargo build --release -p lsd` from rust/ in this checkout."
    }
}

enum SidecarLocator {
    /// Installed `lsd` lives inside Local Services.app (`task rust:install` / `macos:install`).
    private static let knownLsdBinaryPaths = [
        "/Applications/Local Services.app/Contents/Resources/lsd/bin/lsd",
        "/usr/local/bin/lsd",
        "\(NSHomeDirectory())/.cargo/bin/lsd",
    ]

    /// Set `LSD_DEBUG=1` to log which `lsd` this resolved to on stderr.
    private static var debugLoggingEnabled: Bool { ProcessInfo.processInfo.environment["LSD_DEBUG"] == "1" }
    private static func debugLog(_ message: @autoclosure () -> String) {
        guard debugLoggingEnabled else { return }
        FileHandle.standardError.write(Data("[SidecarLocator] \(message())\n".utf8))
    }

    static func findLsdBinary() -> String? {
        if let override = ProcessInfo.processInfo.environment["LOCAL_SERVICES_LSD_BIN_PATH"], FileManager.default.isExecutableFile(atPath: override) {
            debugLog("lsd (binary): \(override) (LOCAL_SERVICES_LSD_BIN_PATH override)")
            return override
        }
        if let bundled = Bundle.main.resourceURL?.appendingPathComponent("lsd/bin/lsd").path, FileManager.default.isExecutableFile(atPath: bundled) {
            debugLog("lsd (binary): \(bundled) (bundled app resource)")
            return bundled
        }
        for path in knownLsdBinaryPaths where FileManager.default.isExecutableFile(atPath: path) {
            debugLog("lsd (binary): \(path) (known install location)")
            return path
        }
        if let dev = devRepoLsdBinary() {
            debugLog("lsd (binary): \(dev) (dev repo `cargo build` output)")
            return dev
        }
        let found = loginShellWhich("lsd")
        debugLog("lsd (binary): \(found ?? "not found") (login shell `command -v`)")
        return found
    }

    /// Resolves `rust/target/{release,debug}/lsd` relative to this source file's own on-disk path —
    /// only reached when there's no bundled/installed binary. Release preferred over debug.
    private static func devRepoLsdBinary() -> String? {
        var url = URL(fileURLWithPath: #filePath) // .../apps/macos/Sources/LocalServicesApp/Sidecar/SidecarLocator.swift
        for _ in 0..<6 { url.deleteLastPathComponent() } // -> repo root
        for profile in ["release", "debug"] {
            let candidate = url.appendingPathComponent("rust/target/\(profile)/lsd").path
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
