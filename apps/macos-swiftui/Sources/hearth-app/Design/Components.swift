import SwiftUI
import HearthKit

/// Heading bar at the top of every column.
struct PaneHeader<Trailing: View>: View {
    let title: String
    var subtitle: String?
    var systemImage: String?
    @ViewBuilder var trailing: () -> Trailing

    var body: some View {
        HStack(spacing: Theme.Space.sm) {
            if let systemImage {
                Image(systemName: systemImage).foregroundStyle(.secondary).accessibilityHidden(true)
            }
            Text(title).font(.headline)
            if let subtitle {
                Text(subtitle).font(.subheadline).foregroundStyle(.secondary).monospacedDigit()
            }
            Spacer(minLength: 0)
            trailing()
        }
        .padding(.horizontal, Theme.Space.lg)
        .frame(height: 40)
        .background(.bar)
        .overlay(alignment: .bottom) { Divider() }
        .accessibilityElement(children: .contain)
        .accessibilityAddTraits(.isHeader)
    }
}

extension PaneHeader where Trailing == EmptyView {
    init(title: String, subtitle: String? = nil, systemImage: String? = nil) {
        self.init(title: title, subtitle: subtitle, systemImage: systemImage) { EmptyView() }
    }
}

/// Small section heading used inside a pane ("Skills  12").
struct SectionHeading<Trailing: View>: View {
    let title: String
    var count: Int?
    @ViewBuilder var trailing: () -> Trailing
    var body: some View {
        HStack {
            Text(title.uppercased()).font(.caption.weight(.semibold)).foregroundStyle(.secondary)
            if let count { Text("\(count)").font(.caption).monospacedDigit().foregroundStyle(.tertiary) }
            Spacer(minLength: 0)
            trailing()
        }
        .accessibilityAddTraits(.isHeader)
    }
}
extension SectionHeading where Trailing == EmptyView {
    init(title: String, count: Int? = nil) { self.init(title: title, count: count) { EmptyView() } }
}

/// Rounded grouped container.
struct Card<Content: View>: View {
    var padding: CGFloat = Theme.Space.md
    @ViewBuilder var content: () -> Content
    var body: some View {
        content()
            .padding(padding)
            .frame(maxWidth: .infinity, alignment: .leading)
            .background(.background.secondary, in: RoundedRectangle(cornerRadius: Theme.Radius.md))
            .overlay(RoundedRectangle(cornerRadius: Theme.Radius.md).strokeBorder(.separator))
    }
}

/// Capsule tag ("stdio", "mismatch" ...).
struct Tag: View {
    let text: String
    var tint: Color = .secondary
    var systemImage: String?
    var body: some View {
        HStack(spacing: 4) {
            if let systemImage { Image(systemName: systemImage).imageScale(.small) }
            Text(text).lineLimit(1)
        }
        .font(.caption.weight(.medium))
        .foregroundStyle(tint)
        .padding(.horizontal, 8).padding(.vertical, 2)
        .background(tint.opacity(0.14), in: Capsule())
    }
}

/// Centered placeholder for empty / error / idle panes.
struct StateView<Actions: View>: View {
    let systemImage: String
    let title: String
    var message: String?
    var tint: Color = .secondary
    @ViewBuilder var actions: () -> Actions
    var body: some View {
        VStack(spacing: Theme.Space.md) {
            Image(systemName: systemImage).font(.system(size: 34, weight: .light)).foregroundStyle(tint)
                .accessibilityHidden(true)
            Text(title).font(.headline)
            if let message {
                Text(message).font(.callout).foregroundStyle(.secondary).multilineTextAlignment(.center)
                    .textSelection(.enabled)
            }
            actions()
        }
        .padding(Theme.Space.xl)
        .frame(maxWidth: 420)
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .accessibilityElement(children: .contain)
    }
}
extension StateView where Actions == EmptyView {
    init(systemImage: String, title: String, message: String? = nil, tint: Color = .secondary) {
        self.init(systemImage: systemImage, title: title, message: message, tint: tint) { EmptyView() }
    }
}

/// Indeterminate "working" state with an optional cancel-free message.
struct WorkingView: View {
    let message: String
    var body: some View {
        VStack(spacing: Theme.Space.md) {
            ProgressView().controlSize(.large)
            Text(message).font(.callout).foregroundStyle(.secondary)
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .accessibilityElement(children: .combine)
        .accessibilityLabel(message)
    }
}

/// Floating status message; tap to dismiss.
struct ToastView: View {
    let toast: Toast
    let dismiss: () -> Void
    var body: some View {
        let (icon, tint): (String, Color) = switch toast.style {
        case .success: (Icon.success, .green)
        case .error: (Icon.error, .red)
        case .info: (Icon.info, .blue)
        }
        HStack(spacing: Theme.Space.sm) {
            Image(systemName: icon).foregroundStyle(tint)
            Text(toast.message).lineLimit(3).textSelection(.enabled)
            Button(action: dismiss) { Label("Dismiss", systemImage: Icon.close) }
                .buttonStyle(.plain).foregroundStyle(.secondary)
        }
        .font(.callout)
        .padding(.horizontal, Theme.Space.lg).padding(.vertical, Theme.Space.md)
        .background(.regularMaterial, in: RoundedRectangle(cornerRadius: Theme.Radius.md))
        .overlay(RoundedRectangle(cornerRadius: Theme.Radius.md).strokeBorder(.separator))
        .shadow(color: .black.opacity(0.15), radius: 12, y: 4)
        .frame(maxWidth: 520)
        .accessibilityElement(children: .combine)
    }
}


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

/// A selectable list row for the sidebar. Top-aligned, full width, rounded selection.
struct SelectableRow<Content: View>: View {
    let selected: Bool
    let action: () -> Void
    @ViewBuilder var content: () -> Content

    var body: some View {
        Button(action: action) {
            content()
                .padding(.horizontal, Theme.Space.sm).padding(.vertical, 6)
                .frame(maxWidth: .infinity, alignment: .leading)
                .background(selected ? Color.accentColor.opacity(0.18) : .clear,
                            in: RoundedRectangle(cornerRadius: Theme.Radius.sm))
                .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .accessibilityAddTraits(selected ? .isSelected : [])
    }
}
