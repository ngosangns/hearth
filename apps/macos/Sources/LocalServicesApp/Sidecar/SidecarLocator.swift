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
// `lsd.ts` is located via the *source* path of this very file (`#filePath`), which only works when
// this app is built in place inside the `local-services` monorepo (this milestone's only supported
// setup — see apps/macos/README.md). Packaging `lsd.ts` (plus its `src/` imports) as a bundled app
// resource, so the app works from an installed .app with no adjacent checkout, is documented there
// as the next step, not attempted yet.

import Foundation

enum SidecarLocatorError: Error, LocalizedError {
    case bunNotFound
    case lsdEntryNotFound(String)

    var errorDescription: String? {
        switch self {
        case .bunNotFound:
            return "Could not find a `bun` executable. Install it from https://bun.sh and relaunch."
        case .lsdEntryNotFound(let path):
            return "Could not find src/bin/lsd.ts at \(path) — this app must be built in place inside the local-services repo."
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

    static func findBun() -> String? {
        for path in knownBunPaths where FileManager.default.isExecutableFile(atPath: path) {
            return path
        }
        return loginShellWhich("bun")
    }

    /// Resolves `src/bin/lsd.ts` relative to this source file's own on-disk path at compile time.
    static func findLsdEntry() -> String? {
        if let override = ProcessInfo.processInfo.environment["LOCAL_SERVICES_LSD_PATH"], FileManager.default.fileExists(atPath: override) {
            return override
        }
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
