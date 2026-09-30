import AppKit
import Sparkle
import SwiftUI

/// Sparkle updater, wired the way XKey does it: one `SPUStandardUpdaterController`, a background
/// check shortly after launch when automatic checks are on, and the app brought forward so the
/// update panel is visible from the menu bar.
///
/// A `swift build` binary has no `SUFeedURL`, so Sparkle stays off and Check for Updates opens the
/// releases page instead. The packaged app's Info.plist is what turns the real updater on.
final class UpdateController: NSObject, SPUUpdaterDelegate, SPUStandardUserDriverDelegate {
    static let shared = UpdateController()

    private var controller: SPUStandardUpdaterController?
    private var didStart = false

    private override init() {
        super.init()
    }

    /// True once Sparkle is running. Menu items that only make sense for the packaged app read this.
    var isRunning: Bool { controller != nil }

    var automaticallyChecksForUpdates: Bool {
        get { controller?.updater.automaticallyChecksForUpdates ?? false }
        set { controller?.updater.automaticallyChecksForUpdates = newValue }
    }

    var automaticChecksBinding: Binding<Bool> {
        Binding(
            get: { self.automaticallyChecksForUpdates },
            set: { self.automaticallyChecksForUpdates = $0 }
        )
    }

    func start() {
        guard !didStart else { return }
        didStart = true
        guard NSClassFromString("XCTestCase") == nil else { return }
        guard Bundle.main.object(forInfoDictionaryKey: "SUFeedURL") is String,
              Bundle.main.object(forInfoDictionaryKey: "SUPublicEDKey") is String else { return }

        controller = SPUStandardUpdaterController(
            startingUpdater: true,
            updaterDelegate: self,
            userDriverDelegate: self
        )

        guard controller?.updater.automaticallyChecksForUpdates == true else { return }
        DispatchQueue.main.asyncAfter(deadline: .now() + 3) { [weak self] in
            guard let updater = self?.controller?.updater, updater.canCheckForUpdates else { return }
            updater.checkForUpdatesInBackground()
        }
    }

    /// Manual check from the menu bar or the toolbar. Activates the app first so Sparkle's panel
    /// is not left behind other windows.
    func checkForUpdates() {
        NSApp.activate(ignoringOtherApps: true)
        if let updater = controller?.updater {
            updater.checkForUpdates()
        } else {
            NSWorkspace.shared.open(AppLinks.releases)
        }
    }

    func standardUserDriverWillHandleShowingUpdate(_ handleShowingUpdate: Bool, forUpdate update: SUAppcastItem, state: SPUUserUpdateState) {
        NSApp.activate(ignoringOtherApps: true)
    }

    func standardUserDriverShouldHandleShowingScheduledUpdate(_ update: SUAppcastItem, andInImmediateFocus immediateFocus: Bool) -> Bool {
        NSApp.activate(ignoringOtherApps: true)
        return true
    }
}
