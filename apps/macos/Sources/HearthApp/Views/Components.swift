import SwiftUI

/// The one place that maps a service/instance state string to a color — service rows, sidebar
/// summaries, and shared-catalog rows all render state identically. Semantic system colors, so the
/// palette follows light/dark mode on its own.
enum StatusStyle {
    static func color(for state: String) -> Color {
        switch state {
        case "ready": return .green
        case "running": return .mint
        case "starting", "queued": return .yellow
        case "failed": return .red
        case "stopping": return .orange
        default: return .gray
        }
    }
}

/// 8-pt colored dot — the app's state glyph. Drawn once here so every list row agrees.
struct StatusDot: View {
    let state: String

    var body: some View {
        Circle()
            .fill(StatusStyle.color(for: state))
            .frame(width: 8, height: 8)
    }
}

/// The state word as a small tinted capsule — used where the state should be scannable at a
/// glance (log header, shared instance detail) rather than read as prose in a metadata line.
struct StateBadge: View {
    let state: String

    var body: some View {
        Text(state)
            .font(.caption.weight(.medium))
            .foregroundStyle(StatusStyle.color(for: state))
            .padding(.horizontal, 7)
            .padding(.vertical, 1)
            .background(StatusStyle.color(for: state).opacity(0.15), in: Capsule())
    }
}

/// Icon-only button — the tooltip (`help`) and the `Label` title carry the action's name; the
/// title is also the accessibility label. Used for every row/toolbar action so service lists stay
/// compact and consistent.
struct IconActionButton: View {
    let title: String
    let systemImage: String
    var role: ButtonRole? = nil
    /// `true` in list rows — a fixed box keeps a row of icons evenly spaced despite SF Symbols'
    /// different natural widths. `false` in toolbars, which size their items themselves.
    var compact: Bool = true
    let action: () -> Void

    init(_ title: String, systemImage: String, role: ButtonRole? = nil, compact: Bool = true, action: @escaping () -> Void) {
        self.title = title
        self.systemImage = systemImage
        self.role = role
        self.compact = compact
        self.action = action
    }

    var body: some View {
        Button(role: role, action: action) {
            Label(title, systemImage: systemImage)
                .labelStyle(.iconOnly)
                .frame(width: compact ? 22 : nil, height: compact ? 16 : nil)
                .contentShape(Rectangle())
        }
        .buttonStyle(.borderless)
        .help(title)
    }
}

/// A spinner at the same footprint as an `IconActionButton` — the row's action area does not
/// jump width when a state transition swaps a button for the spinner.
struct ActionSpinner: View {
    var body: some View {
        ProgressView()
            .controlSize(.small)
            .frame(width: 22, height: 16)
    }
}

/// A dismissible inline error strip shown above a list — replaces silent failures and modals for
/// recoverable action errors.
struct ErrorBanner: View {
    let text: String
    let onDismiss: () -> Void

    var body: some View {
        HStack(alignment: .top, spacing: 8) {
            Image(systemName: "exclamationmark.circle.fill")
                .foregroundStyle(.red)
            Text(text)
                .font(.callout)
                .lineLimit(2)
            Spacer()
            Button("Dismiss", action: onDismiss)
                .buttonStyle(.borderless)
                .font(.caption)
        }
        .padding(.horizontal, 10)
        .padding(.vertical, 6)
        .background(.red.opacity(0.12))
    }
}
