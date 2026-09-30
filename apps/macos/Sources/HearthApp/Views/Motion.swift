import AppKit
import SwiftUI

enum Motion {
    /// Nil when Reduce Motion is on. `.smooth` is macOS 14; macOS 13 gets the same duration.
    static var layout: Animation? {
        guard !NSWorkspace.shared.accessibilityDisplayShouldReduceMotion else { return nil }
        if #available(macOS 14, *) {
            return .smooth(duration: 0.22)
        }
        return .easeInOut(duration: 0.22)
    }
}

extension View {
    /// AppKit draws the log. A parent animation must not scale or fade that surface.
    func staticTerminalSurface() -> some View {
        transaction { transaction in
            transaction.animation = nil
            transaction.disablesAnimations = true
        }
    }
}
