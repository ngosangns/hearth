import CoreServices
import Foundation

/// Watches a workspace root for changes to its config file and calls `onChange` (debounced) — used
/// to trigger `hearthd manager reload` when `hearth.yaml`/`.yml`/`.json` is edited. Which of those
/// names is actually in play is `rust/crates/hearth-core/src/config_file.rs`'s job, re-run fresh on
/// every `hearthd` invocation, so any of the three counts.
///
/// FSEvents with per-file events, filtered to those names directly inside the root. A
/// `DispatchSource` on the directory's own descriptor only fires when directory *entries* change:
/// an editor's atomic save (write temp + rename) did, but an in-place write — plenty of editors,
/// `echo >> hearth.yaml` — never reached it, so the edit was silently not reloaded. FSEvents reports
/// both, and survives the file being deleted and recreated, which a watch on the file's own
/// descriptor would not.
final class ConfigFileWatcher {
    static let configFileNames: Set<String> = ["hearth.yaml", "hearth.yml", "hearth.json"]

    private let directory: String
    private let onChange: @Sendable () -> Void
    private let debounceInterval: TimeInterval
    private var stream: FSEventStreamRef?
    /// Callback delivery and path filtering run here, off the main queue.
    private let queue = DispatchQueue(label: "hearth.config-watch", qos: .utility)
    private let stateLock = NSLock()
    private var debounceWorkItem: DispatchWorkItem?
    /// Bumped by `start()`/`stop()` under `stateLock`. A debounce block already queued fires
    /// `onChange` only when it still matches, so `stop()` cannot reload after the watch is gone.
    private var generation: UInt64 = 0
    /// `realpath` of `directory` — FSEvents reports resolved paths (`/private/tmp/…` for `/tmp/…`).
    private var resolvedDirectory: String?

    init(directory: String, debounce: TimeInterval = 0.5, onChange: @escaping @Sendable () -> Void) {
        self.directory = directory
        self.onChange = onChange
        self.debounceInterval = debounce
    }

    func start() {
        stop()
        guard let resolved = Self.realPath(directory) else { return } // folder missing — nothing to watch
        stateLock.lock()
        resolvedDirectory = resolved
        stateLock.unlock()
        var context = FSEventStreamContext(
            version: 0,
            info: Unmanaged.passUnretained(self).toOpaque(),
            retain: nil,
            release: nil,
            copyDescription: nil
        )
        let callback: FSEventStreamCallback = { _, info, count, paths, _, _ in
            guard let info else { return }
            let watcher = Unmanaged<ConfigFileWatcher>.fromOpaque(info).takeUnretainedValue()
            // The CFArray is only valid inside the callback — copy it (immutable → a retain) and
            // hand the expensive `[String]` bridge + filtering to the private queue; `watcher` is
            // captured strongly there, so a late flush can never reach a deallocated watcher.
            let pathsArray = (Unmanaged<CFArray>.fromOpaque(paths).takeUnretainedValue() as NSArray).copy() as! NSArray
            let generation = watcher.currentGeneration()
            watcher.queue.async {
                let swiftPaths = pathsArray as? [String] ?? []
                watcher.handle(swiftPaths.prefix(count), generation: generation)
            }
        }
        let flags = FSEventStreamCreateFlags(kFSEventStreamCreateFlagFileEvents | kFSEventStreamCreateFlagUseCFTypes | kFSEventStreamCreateFlagNoDefer)
        guard let stream = FSEventStreamCreate(nil, callback, &context, [resolved] as CFArray, FSEventStreamEventId(kFSEventStreamEventIdSinceNow), 0.1, flags) else {
            stop()
            return
        }
        // Delivered on the main queue, which is also where `stop()` runs — so a callback can never
        // race the stream's invalidation and reach a deallocated watcher through `info`. The heavy
        // per-file work itself bounces to `queue` (see the callback).
        FSEventStreamSetDispatchQueue(stream, .main)
        guard FSEventStreamStart(stream) else {
            FSEventStreamInvalidate(stream)
            FSEventStreamRelease(stream)
            stop()
            return
        }
        self.stream = stream
    }

    func stop() {
        if let stream {
            FSEventStreamStop(stream)
            FSEventStreamInvalidate(stream)
            FSEventStreamRelease(stream)
        }
        stream = nil
        stateLock.lock()
        generation &+= 1
        debounceWorkItem?.cancel()
        debounceWorkItem = nil
        resolvedDirectory = nil
        stateLock.unlock()
    }

    deinit {
        stop()
    }

    private func currentGeneration() -> UInt64 {
        stateLock.lock()
        defer { stateLock.unlock() }
        return generation
    }

    private func handle<Paths: Sequence>(_ paths: Paths, generation: UInt64) where Paths.Element == String {
        stateLock.lock()
        let current = self.generation
        let resolvedDirectory = self.resolvedDirectory
        stateLock.unlock()
        guard current == generation, let resolvedDirectory else { return }
        let touchesConfig = paths.contains { path in
            let url = URL(fileURLWithPath: path)
            guard Self.configFileNames.contains(url.lastPathComponent) else { return false }
            // Only the root's own config — FSEvents is recursive, and a nested project's
            // `hearth.yaml` is not this workspace's.
            let parent = url.deletingLastPathComponent().path
            return parent == resolvedDirectory || Self.realPath(parent) == resolvedDirectory
        }
        if touchesConfig { scheduleDebounced(generation: generation) }
    }

    private func scheduleDebounced(generation: UInt64) {
        stateLock.lock()
        guard generation == self.generation else {
            stateLock.unlock()
            return
        }
        debounceWorkItem?.cancel()
        let work = DispatchWorkItem { [onChange, weak self] in
            guard let self else { return }
            self.stateLock.lock()
            let current = self.generation
            self.stateLock.unlock()
            guard current == generation else { return }
            onChange()
        }
        debounceWorkItem = work
        stateLock.unlock()
        DispatchQueue.main.asyncAfter(deadline: .now() + debounceInterval, execute: work)
    }

    private static func realPath(_ path: String) -> String? {
        guard let resolved = realpath(path, nil) else { return nil }
        defer { free(resolved) }
        return String(cString: resolved)
    }
}
