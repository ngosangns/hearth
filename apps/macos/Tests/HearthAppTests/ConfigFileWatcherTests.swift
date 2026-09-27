import XCTest
@testable import HearthApp

@MainActor
final class ConfigFileWatcherTests: XCTestCase {
    private var directory: URL!

    override func setUp() async throws {
        directory = FileManager.default.temporaryDirectory.appendingPathComponent("config-watch-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        try Data("services: {}\n".utf8).write(to: directory.appendingPathComponent("hearth.yaml"))
        // Let the setup writes age out, so only what the test does next can reach the stream.
        try await Task.sleep(for: .milliseconds(500))
    }

    override func tearDown() async throws {
        try? FileManager.default.removeItem(at: directory)
    }

    private func waitUntil(_ what: String, timeout: Duration = .seconds(5), _ condition: () -> Bool) async {
        let deadline = ContinuousClock.now.advanced(by: timeout)
        while !condition() {
            if ContinuousClock.now >= deadline { return XCTFail("timed out waiting for \(what)") }
            try? await Task.sleep(for: .milliseconds(20))
        }
    }

    /// The bug this pins: a directory-level `DispatchSource` only saw entry changes, so an in-place
    /// write (`echo >> hearth.yaml`, most editors' non-atomic save) never triggered a reload.
    func testAnInPlaceAppendToTheConfigFileFires() async throws {
        let fired = Counter()
        let watcher = ConfigFileWatcher(directory: directory.path, debounce: 0.05) { fired.increment() }
        watcher.start()
        defer { watcher.stop() }
        try await Task.sleep(for: .milliseconds(200))

        let handle = try FileHandle(forWritingTo: directory.appendingPathComponent("hearth.yaml"))
        try handle.seekToEnd()
        try handle.write(contentsOf: Data("# edited\n".utf8))
        try handle.close()

        await waitUntil("the in-place write to fire") { fired.value > 0 }
    }

    func testAnAtomicSaveFires() async throws {
        let fired = Counter()
        let watcher = ConfigFileWatcher(directory: directory.path, debounce: 0.05) { fired.increment() }
        watcher.start()
        defer { watcher.stop() }
        try await Task.sleep(for: .milliseconds(200))

        try Data("services: {}\n# saved\n".utf8).write(to: directory.appendingPathComponent("hearth.yaml"), options: .atomic)

        await waitUntil("the atomic save to fire") { fired.value > 0 }
    }

    /// Unrelated files — and a nested project's own `hearth.yaml` — are not this workspace's config.
    func testOtherFilesDoNotFire() async throws {
        let fired = Counter()
        let watcher = ConfigFileWatcher(directory: directory.path, debounce: 0.05) { fired.increment() }
        watcher.start()
        defer { watcher.stop() }
        try await Task.sleep(for: .milliseconds(200))

        try Data("x".utf8).write(to: directory.appendingPathComponent("notes.txt"))
        let nested = directory.appendingPathComponent("nested")
        try FileManager.default.createDirectory(at: nested, withIntermediateDirectories: true)
        try Data("services: {}\n".utf8).write(to: nested.appendingPathComponent("hearth.yaml"))
        try await Task.sleep(for: .seconds(1))

        XCTAssertEqual(fired.value, 0)
    }
}
