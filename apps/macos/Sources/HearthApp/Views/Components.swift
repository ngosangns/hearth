import AppKit
import SwiftUI

/// The one place that maps a service/instance state string to a color — service rows, sidebar
/// summaries, and shared-catalog rows all render state identically. Semantic system colors, so the
/// palette follows light/dark mode on its own.
enum StatusStyle {
    static func color(for state: String) -> Color {
        switch state {
        case "ready", "succeeded": return .green
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

/// The general pasteboard, as every Copy button uses it.
enum Pasteboard {
    static func copy(_ text: String) {
        NSPasteboard.general.clearContents()
        NSPasteboard.general.setString(text, forType: .string)
    }
}

/// The lifecycle buttons for a (display-collapsed) state that both a service row and a shared
/// instance share: Start from rest, Restart+Stop while up, Cancel for a queued start. A finite
/// command (`readiness: exit`) says Run, including after it has already `succeeded`. States with
/// their own affordances (`orphaned`, `external`) are handled by the caller before this.
struct LifecycleButtons: View {
    let state: String
    var finite: Bool = false
    let onAction: (ManagerAction) -> Void

    /// Title of the button that runs the service again. `nil` when this state is not a run/start.
    static func runTitle(state: String, finite: Bool) -> String? {
        switch state {
        case "succeeded": return "Run"
        case "stopped", "failed": return finite ? "Run" : "Start"
        default: return nil
        }
    }

    var body: some View {
        switch state {
        case "succeeded", "stopped", "failed":
            IconActionButton(Self.runTitle(state: state, finite: finite) ?? "Start", systemImage: "play.fill") { onAction(.start) }
        case "ready", "running", "starting":
            IconActionButton("Restart", systemImage: "arrow.clockwise") { onAction(.restart) }
            IconActionButton("Stop", systemImage: "stop.fill") { onAction(.stop) }
        // A service queued behind another operation on the same target stays `queued-start`
        // until something clears it — without this the row offered no way out of it at all.
        case "queued":
            IconActionButton("Cancel start", systemImage: "xmark") { onAction(.stop) }
        // `stopping` is genuinely in-flight — no extra action, the poll will move it.
        default:
            EmptyView()
        }
    }
}

/// "Stop Daemon…" takes the daemon AND every service it manages down, so every surface confirms it
/// with the same words.
enum StopDaemonConfirmation {
    static let title = "Stop this project's daemon?"
    static let message = "The daemon and every service it manages will be stopped."
    static let confirmLabel = "Stop Daemon"

    /// A modal alert, for the menu bar: `.menuBarExtraStyle(.menu)` renders its content as
    /// `NSMenu` items, which have no window to host a `confirmationDialog` — one attached there
    /// never appears, and the stop it guards could never be confirmed.
    @MainActor
    static func runModal(workspaceName: String) -> Bool {
        let alert = NSAlert()
        alert.alertStyle = .warning
        alert.messageText = title
        alert.informativeText = "\(workspaceName): \(message)"
        alert.addButton(withTitle: confirmLabel).hasDestructiveAction = true
        alert.addButton(withTitle: "Cancel")
        NSApp.activate(ignoringOtherApps: true)
        return alert.runModal() == .alertFirstButtonReturn
    }
}

extension View {
    /// The in-window form of `StopDaemonConfirmation`.
    func stopDaemonConfirmation(isPresented: Binding<Bool>, onConfirm: @escaping () -> Void) -> some View {
        confirmationDialog(StopDaemonConfirmation.title, isPresented: isPresented, titleVisibility: .visible) {
            Button(StopDaemonConfirmation.confirmLabel, role: .destructive, action: onConfirm)
            Button("Cancel", role: .cancel) {}
        } message: {
            Text(StopDaemonConfirmation.message)
        }
    }
}
