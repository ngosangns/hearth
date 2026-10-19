import SwiftUI
import HearthKit
import NSShell

/// Wire state with a semantic dot, e.g. "● running". Fits its content; never relies on colour alone.
struct StatePill: View {
    let state: String
    var label: String?

    var body: some View {
        let tint = Self.tint(state)
        let text = label ?? ServiceBoard.displayState(state)
        HStack(spacing: 5) {
            Circle().fill(tint).frame(width: 6, height: 6)
            Text(text).lineLimit(1)
        }
        .font(.caption.weight(.medium))
        .foregroundStyle(tint)
        .padding(.horizontal, 8).padding(.vertical, 2)
        .background(tint.opacity(0.14), in: Capsule())
        .fixedSize()
        .accessibilityElement(children: .ignore)
        .accessibilityLabel("State: \(text)")
    }

    static func tint(_ state: String) -> Color {
        switch state {
        case "ready", "succeeded": .green
        case "running", "running-unready": .blue
        case "starting", "preparing", "queued-start", "stopping": .orange
        case "failed", "orphaned", "externally-owned": .red
        default: .secondary
        }
    }
}
