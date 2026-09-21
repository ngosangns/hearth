import Dispatch
import Foundation

/// Watches a workspace's root directory for writes and calls `onChange` (debounced) — used to trigger
/// `lsd manager reload` when `local-services.yaml`/`.yml`/`.json` is edited, without this
/// app needing to know which of those filenames is actually in play (that's `config-file.ts`'s
/// job, re-run fresh on every `lsd` invocation).
///
/// A lightweight `DispatchSource` on the directory's own file descriptor, not the FSEvents API proper
/// — this fires on *any* write inside the directory (not just the config file), so a reload can be
/// triggered by an unrelated edit elsewhere in the folder. That's an accepted false-positive: a
/// reload of an unchanged catalog is a cheap no-op on the daemon side (`reloadCatalog` diffs against
/// the current catalog), and watching only the exact config filename would need to handle it being
/// renamed-in/deleted/recreated (common with editor atomic saves), which this simpler directory-level
/// watch sidesteps entirely.
final class ConfigFileWatcher {
    private let directory: String
    private let onChange: @Sendable () -> Void
    private var source: DispatchSourceFileSystemObject?
    private var fileDescriptor: CInt = -1
    private var debounceWorkItem: DispatchWorkItem?

    init(directory: String, debounce: TimeInterval = 0.5, onChange: @escaping @Sendable () -> Void) {
        self.directory = directory
        self.onChange = onChange
        self.debounceInterval = debounce
    }

    private let debounceInterval: TimeInterval

    func start() {
        stop()
        fileDescriptor = open(directory, O_EVTONLY)
        guard fileDescriptor >= 0 else { return } // folder missing/unreadable — nothing to watch
        let source = DispatchSource.makeFileSystemObjectSource(fileDescriptor: fileDescriptor, eventMask: [.write, .rename, .delete], queue: .main)
        source.setEventHandler { [weak self] in self?.scheduleDebounced() }
        let fd = fileDescriptor
        source.setCancelHandler { close(fd) }
        source.resume()
        self.source = source
    }

    func stop() {
        source?.cancel()
        source = nil
        debounceWorkItem?.cancel()
        debounceWorkItem = nil
    }

    deinit {
        stop()
    }

    private func scheduleDebounced() {
        debounceWorkItem?.cancel()
        let work = DispatchWorkItem { [onChange] in onChange() }
        debounceWorkItem = work
        DispatchQueue.main.asyncAfter(deadline: .now() + debounceInterval, execute: work)
    }
}
