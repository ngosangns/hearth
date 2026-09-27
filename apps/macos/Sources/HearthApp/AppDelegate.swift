import AppKit
import SwiftUI

/// Bridges SwiftUI's `openWindow` action out of the WindowGroup so the AppDelegate can reopen the
/// main window when the app is reactivated without one. The captured action stays valid after the
/// window closes because the WindowGroup scene persists for the app's lifetime.
enum MainWindow {
    static let id = "main"
    static var open: (() -> Void)?
}

/// The shared-services catalog window (`hearthd smp` browser) — a `Window` scene, so single-instance
/// by construction; no reopen-capture needed like `MainWindow`'s.
enum SharedWindow {
    static let id = "shared"
}

/// Where "Check for Updates…" goes. Derived from a GitHub `SUFeedURL` (`…/owner/repo/releases.atom`)
/// if the bundle ever declares one again (Sparkle is not linked today, so Info.plist carries none),
/// else this repo's releases page — one constant, so the toolbar and menu bar cannot disagree.
enum AppLinks {
    static let fallbackReleases = URL(string: "https://github.com/ngosangns/hearth/releases")!

    static let releases: URL = {
        guard let feed = (Bundle.main.object(forInfoDictionaryKey: "SUFeedURL") as? String).flatMap(URL.init(string:)) else {
            return fallbackReleases
        }
        return releasesPage(forFeed: feed) ?? fallbackReleases
    }()

    /// `https://github.com/<owner>/<repo>/…` → `https://github.com/<owner>/<repo>/releases`; `nil`
    /// for anything that is not a GitHub repo URL.
    static func releasesPage(forFeed feed: URL) -> URL? {
        let parts = feed.pathComponents.filter { $0 != "/" }
        guard feed.host == "github.com", parts.count >= 2 else { return nil }
        return URL(string: "https://github.com/\(parts[0])/\(parts[1])/releases")
    }
}

final class AppDelegate: NSObject, NSApplicationDelegate {
    /// A second instance posts this to ask the already-running one to show a window.
    private static let showWindowNotification = Notification.Name("dev.ngosangns.hearth.show-main-window")

    func applicationWillFinishLaunching(_ notification: Notification) {
        // Single instance. LaunchServices normally refuses a second copy, but `open -n` and running
        // the bare binary (scripts/dev.sh) bypass it — check for a live sibling ourselves.
        guard let other = Self.otherRunningInstance() else { return }
        DistributedNotificationCenter.default().postNotificationName(
            Self.showWindowNotification, object: nil, deliverImmediately: true)
        other.activate(options: [.activateAllWindows])
        NSApp.terminate(nil)
        // `terminate` this early in launch can be ignored; repeat once the run loop is up.
        DispatchQueue.main.async { NSApp.terminate(nil) }
    }

    func applicationDidFinishLaunching(_ notification: Notification) {
        DistributedNotificationCenter.default().addObserver(
            self, selector: #selector(showMainWindow), name: Self.showWindowNotification, object: nil)
    }

    func applicationShouldHandleReopen(_ sender: NSApplication, hasVisibleWindows flag: Bool) -> Bool {
        // Dock icon / `open` on the running app with no visible window: reopen the main window.
        guard !flag else { return false }
        guard MainWindow.open != nil else { return true } // no captured opener yet — default handling
        showMainWindow()
        return false
    }

    @objc private func showMainWindow() {
        NSApp.unhide(nil)
        NSRunningApplication.current.activate(options: [.activateAllWindows, .activateIgnoringOtherApps])
        for window in NSApp.windows where window.isMiniaturized {
            window.deminiaturize(nil)
        }
        if let window = NSApp.windows.first(where: { $0.canBecomeMain }) {
            window.makeKeyAndOrderFront(nil)
        } else {
            MainWindow.open?()
        }
    }

    private static func otherRunningInstance() -> NSRunningApplication? {
        let current = ProcessInfo.processInfo.processIdentifier
        if let bundleID = Bundle.main.bundleIdentifier {
            return NSRunningApplication.runningApplications(withBundleIdentifier: bundleID)
                .first { $0.processIdentifier != current }
        }
        // Bare-binary launch (no bundle): match on the executable path instead.
        let executable = Bundle.main.executableURL?.resolvingSymlinksInPath()
        return NSWorkspace.shared.runningApplications.first {
            $0.processIdentifier != current && $0.executableURL?.resolvingSymlinksInPath() == executable
        }
    }
}
