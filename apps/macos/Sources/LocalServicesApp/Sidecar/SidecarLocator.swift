// Finds the two things `DaemonConnection` needs to spawn `lsd` as a subprocess: the `bun` runtime
// and this package's `src/bin/lsd.ts` entry.
//
// This app currently runs `bun run <lsd.ts>` rather than a `bun build --compile` standalone binary.
// That was tried first (see git history / PR discussion) and rejected: a freshly-compiled, ad-hoc
// (not Developer-ID) signed Bun executable gets SIGKILLed on launch by this machine's endpoint
// security — even a trivial "hello world" compile reproduces it, while the long-installed system
// `bun` (also only ad-hoc signed) runs fine, which points at "freshly written unsigned executable"
// heuristics rather than a signature-validity check `bun build --compile` could fix on its own.
// Shipping a *signed* compiled sidecar (a real Developer ID cert + notarization) is real future work,
// not something to route around here. `bun run` against the plain TS source sidesteps the whole
// problem for now, at the cost of requiring a `bun` install on the machine running this app — no
// different from this package's own existing "Bun-only" requirement.
//
// `lsd.ts` is located one of two ways: a packaged `.app` (see scripts/build-app.sh) carries a copy of
// `src/` under `Contents/Resources/lsd/src` — `Bundle.main.resourceURL` finds that first. A `swift
// run`/`swift build` dev binary has no such bundle, so it falls back to resolving `src/bin/lsd.ts`
// from *this source file's own* on-disk path (`#filePath`), which only works built in place inside
// the `local-services` monorepo (this is fine for dev; the packaged path is what makes the app
// self-contained). The bundled copy is a plain directory copy — relative imports resolve the same
// regardless of where the directory tree sits, verified end to end before wiring this in — but it's
// only `core`/`cli`, not `tui`/`mcp`'s dependencies (`@oh-my-pi/pi-tui`, `@modelcontextprotocol/sdk`),
// so `lsd tui` doesn't work from a packaged app; this app never calls it, only `manager
// ensure/reload/stop`, `start`/`stop`/`restart`, `status`, `logs`.

import Foundation

enum SidecarLocatorError: Error, LocalizedError {
    case bunNotFound
    case lsdEntryNotFound(String)

    var errorDescription: String? {
        switch self {
        case .bunNotFound:
            return "Could not find a `bun` executable. Install it from https://bun.sh and relaunch."
        case .lsdEntryNotFound:
            return "Could not find lsd's src/bin/lsd.ts — this app must either be packaged (scripts/build-app.sh, which bundles it as a resource) or built in place inside the local-services repo."
        }
    }
}

enum SidecarLocator {
    /// Common install locations, checked before falling back to an interactive-login-shell `which`
    /// (see `core/env.ts`'s `resolveLoginShellEnv` for the same trick on the daemon side) — a GUI app
    /// launched from Finder/Dock inherits a bare `PATH` with none of these.
    private static let knownBunPaths = [
        "/opt/homebrew/bin/bun",
        "/usr/local/bin/bun",
        "\(NSHomeDirectory())/.bun/bin/bun",
    ]

    /// Set `LSD_DEBUG=1` in the environment to log which `bun`/`lsd.ts` this resolved to on stderr —
    /// the first thing to check for a "manager unavailable"/"bun not found" report, and what confirms
    /// a packaged `.app` is actually using its bundled `src/` copy rather than falling back to a dev
    /// checkout that happens to still be on the same machine.
    private static var debugLoggingEnabled: Bool { ProcessInfo.processInfo.environment["LSD_DEBUG"] == "1" }
    private static func debugLog(_ message: @autoclosure () -> String) {
        guard debugLoggingEnabled else { return }
        FileHandle.standardError.write(Data("[SidecarLocator] \(message())\n".utf8))
    }

    static func findBun() -> String? {
        for path in knownBunPaths where FileManager.default.isExecutableFile(atPath: path) {
            debugLog("bun: \(path) (known install location)")
            return path
        }
        let found = loginShellWhich("bun")
        debugLog("bun: \(found ?? "not found") (login shell `command -v`)")
        return found
    }

    static func findLsdEntry() -> String? {
        if let override = ProcessInfo.processInfo.environment["LOCAL_SERVICES_LSD_PATH"], FileManager.default.fileExists(atPath: override) {
            debugLog("lsd.ts: \(override) (LOCAL_SERVICES_LSD_PATH override)")
            return override
        }
        if let bundled = Bundle.main.resourceURL?.appendingPathComponent("lsd/src/bin/lsd.ts").path, FileManager.default.fileExists(atPath: bundled) {
            debugLog("lsd.ts: \(bundled) (bundled app resource)")
            return bundled
        }
        let dev = devRepoLsdEntry()
        debugLog("lsd.ts: \(dev ?? "not found") (dev repo checkout, no bundled resource present)")
        return dev
    }

    /// Resolves `src/bin/lsd.ts` relative to this source file's own on-disk path at compile time —
    /// only reached when there's no bundled copy (an unpackaged `swift run`/`swift build` dev binary).
    private static func devRepoLsdEntry() -> String? {
        var url = URL(fileURLWithPath: #filePath) // .../apps/macos/Sources/LocalServicesApp/Sidecar/SidecarLocator.swift
        for _ in 0..<6 { url.deleteLastPathComponent() } // -> repo root
        let candidate = url.appendingPathComponent("src/bin/lsd.ts").path
        return FileManager.default.fileExists(atPath: candidate) ? candidate : nil
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
