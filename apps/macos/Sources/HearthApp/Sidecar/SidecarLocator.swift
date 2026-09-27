// Finds the compiled `hearthd` binary `DaemonConnection` spawns. Order: env override, a packaged
// `.app`'s bundled copy (`Contents/Resources/hearthd/bin/hearthd` — see `scripts/build-app.sh`), the
// installed `/Applications` copy, other known install locations, this checkout's `cargo build`
// output, then a login-shell `command -v hearthd`.
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

/// Every input to the lookup is a property so a test can drive the resolution order without a
/// real bundle, install, or login shell; `live` is the app's real environment.
struct SidecarLocator: Sendable {
    var environment: [String: String]
    var bundleResourceURL: URL?
    var isExecutable: @Sendable (String) -> Bool
    /// Installed `hearthd` lives inside Hearth.app (`task rust:install` / `macos:install`).
    var knownPaths: [String]
    /// This checkout's `rust/target/{release,debug}/hearthd`, release first.
    var devRepoPaths: [String]
    /// The login shell's raw `command -v hearthd` stdout, or `nil` if it could not run.
    var loginShellLookup: @Sendable (_ shell: String) async -> String?

    static let live = SidecarLocator(
        environment: ProcessInfo.processInfo.environment,
        bundleResourceURL: Bundle.main.resourceURL,
        isExecutable: { FileManager.default.isExecutableFile(atPath: $0) },
        knownPaths: [
            "/Applications/Hearth.app/Contents/Resources/hearthd/bin/hearthd",
            "/usr/local/bin/hearthd",
            "\(NSHomeDirectory())/.cargo/bin/hearthd",
        ],
        devRepoPaths: devRepoHearthdPaths(),
        loginShellLookup: { await runLoginShellLookup($0) }
    )

    /// The resolved path, reused across calls — every `hearthd` invocation (each config-change
    /// reload included) used to redo the lookup, down to spawning an interactive login shell.
    private static let cache = PathCache()

    static func findHearthdBinary() async -> String? {
        if let cached = cache.value, live.isExecutable(cached) { return cached }
        let found = await live.resolve()
        cache.value = found
        return found
    }

    /// The uncached resolution order.
    func resolve() async -> String? {
        if let override = environment["HEARTH_BIN_PATH"], isExecutable(override) {
            debugLog("hearthd (binary): \(override) (HEARTH_BIN_PATH override)")
            return override
        }
        if let bundled = bundleResourceURL?.appendingPathComponent("hearthd/bin/hearthd").path, isExecutable(bundled) {
            debugLog("hearthd (binary): \(bundled) (bundled app resource)")
            return bundled
        }
        for path in knownPaths where isExecutable(path) {
            debugLog("hearthd (binary): \(path) (known install location)")
            return path
        }
        for path in devRepoPaths where isExecutable(path) {
            debugLog("hearthd (binary): \(path) (dev repo `cargo build` output)")
            return path
        }
        let found = await loginShellLookup(environment["SHELL"] ?? "/bin/zsh").flatMap(executablePath(inShellOutput:))
        debugLog("hearthd (binary): \(found ?? "not found") (login shell `command -v`)")
        return found
    }

    /// An interactive rc file can print anything to stdout (banners, `nvm` notices) around the
    /// `command -v` answer — take the last line that is an absolute path to an executable, never
    /// the raw output.
    func executablePath(inShellOutput output: String) -> String? {
        output.split(whereSeparator: \.isNewline)
            .map { $0.trimmingCharacters(in: .whitespaces) }
            .last { $0.hasPrefix("/") && isExecutable($0) }
    }

    /// Set `HEARTHD_DEBUG=1` to log which `hearthd` this resolved to on stderr.
    private func debugLog(_ message: @autoclosure () -> String) {
        guard environment["HEARTHD_DEBUG"] == "1" else { return }
        FileHandle.standardError.write(Data("[SidecarLocator] \(message())\n".utf8))
    }

    /// Resolves `rust/target/{release,debug}/hearthd` relative to this source file's own on-disk path —
    /// only reached when there's no bundled/installed binary. Release preferred over debug.
    private static func devRepoHearthdPaths() -> [String] {
        var url = URL(fileURLWithPath: #filePath) // .../apps/macos/Sources/HearthApp/Sidecar/SidecarLocator.swift
        for _ in 0..<6 { url.deleteLastPathComponent() } // -> repo root
        return ["release", "debug"].map { url.appendingPathComponent("rust/target/\($0)/hearthd").path }
    }

    /// Bounded on its own, well inside `runHearthd`'s ceiling, so a blocking rc file costs a few
    /// seconds rather than the caller's whole budget. stdin is `/dev/null` (see `Subprocess.run`), so
    /// an rc file that prompts reads EOF instead of waiting on a terminal that isn't there.
    private static func runLoginShellLookup(_ shell: String) async -> String? {
        let result = try? await Subprocess.withTimeout(.seconds(10)) {
            try await Subprocess.run(URL(fileURLWithPath: shell), arguments: ["-ilc", "command -v hearthd"])
        }
        guard let result, result.status == 0 else { return nil }
        return result.stdout
    }
}

private final class PathCache: @unchecked Sendable {
    private let lock = NSLock()
    private var _value: String?
    var value: String? {
        get { lock.lock(); defer { lock.unlock() }; return _value }
        set { lock.lock(); _value = newValue; lock.unlock() }
    }
}
