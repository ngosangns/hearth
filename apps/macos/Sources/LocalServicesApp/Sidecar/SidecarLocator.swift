// Finds what `DaemonConnection` needs to spawn `lsd`: preferably a real, compiled `lsd` binary (the
// Rust rewrite's `rust/bin/lsd` — no `bun` install needed at all), falling back to `bun run
// <lsd.ts>` against the original TypeScript source when no compiled binary is found anywhere.
//
// **The compiled-binary path (preferred).** Once rejected for exactly this purpose — see the
// "known limitations" note this replaced, and `rust/AGENTS.md`'s Phase 0 sharp edge — a freshly
// ad-hoc-signed Bun executable used to get SIGKILLed on launch by this machine's endpoint security,
// while a Rust binary ad-hoc-signed the same way does not (confirmed by the Rust rewrite's own
// Phase 0 spike, and re-confirmed against the real multi-thousand-line `lsd` binary in
// `rust/bin/lsd/tests/end_to_end.rs`'s `ad_hoc_signed_binary_runs_without_being_killed`). So once the
// Rust rewrite reached feature parity (`rust/AGENTS.md`), a compiled `lsd` became a real option here.
// `findLsdBinary()` checks, in order: an env override, a packaged `.app`'s bundled copy
// (`Contents/Resources/lsd/bin/lsd` — see `scripts/build-app.sh`), common install locations, a dev
// checkout's own `cargo build` output, then a login-shell `which lsd`.
//
// **The `bun run lsd.ts` fallback** (unchanged from before this file's rewrite) only fires when none
// of the above finds a compiled binary — e.g. a machine that has this repo checked out but never ran
// `cargo build --release -p lsd`. `lsd.ts` is located one of two ways: a packaged `.app` carries a
// copy of `src/` under `Contents/Resources/lsd/src` — `Bundle.main.resourceURL` finds that first. A
// `swift run`/`swift build` dev binary has no such bundle, so it falls back to resolving
// `src/bin/lsd.ts` from *this source file's own* on-disk path (`#filePath`), which only works built
// in place inside the `local-services` monorepo. The bundled TS copy is only `core`/`cli`, not
// `tui`/`mcp`'s dependencies, so `lsd tui` doesn't work through this fallback from a packaged app;
// this app never calls it, only `manager ensure/reload/stop`, `start`/`stop`/`restart`, `status`,
// `logs` — all of which the compiled binary also covers, so this is purely a compatibility fallback,
// not a capability gap.

import Foundation

enum SidecarLocatorError: Error, LocalizedError {
    case bunNotFound
    case lsdEntryNotFound(String)

    var errorDescription: String? {
        switch self {
        case .bunNotFound:
            return "Could not find a `bun` executable. Install it from https://bun.sh and relaunch."
        case .lsdEntryNotFound:
            return "Could not find a compiled `lsd` binary or lsd's src/bin/lsd.ts — this app must either be packaged (scripts/build-app.sh, which bundles both), have `cargo build --release -p lsd` run somewhere on this machine, or be built in place inside the local-services repo."
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

    /// Same rationale as `knownBunPaths` — a GUI-launched app's bare `PATH` won't have wherever
    /// `cargo install`/a package manager put a compiled `lsd`.
    private static let knownLsdBinaryPaths = [
        "/opt/homebrew/bin/lsd",
        "/usr/local/bin/lsd",
        "\(NSHomeDirectory())/.cargo/bin/lsd",
    ]

    /// Set `LSD_DEBUG=1` in the environment to log which `lsd`/`bun`/`lsd.ts` this resolved to on
    /// stderr — the first thing to check for a "manager unavailable"/"bun not found" report, and what
    /// confirms a packaged `.app` is actually using its bundled copy rather than falling back to a
    /// dev checkout that happens to still be on the same machine.
    private static var debugLoggingEnabled: Bool { ProcessInfo.processInfo.environment["LSD_DEBUG"] == "1" }
    private static func debugLog(_ message: @autoclosure () -> String) {
        guard debugLoggingEnabled else { return }
        FileHandle.standardError.write(Data("[SidecarLocator] \(message())\n".utf8))
    }

    /// A real, compiled `lsd` binary — preferred over `bun run lsd.ts` whenever one is found (see
    /// this file's own doc comment for why). `nil` means fall back to `findBun()`/`findLsdEntry()`.
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
