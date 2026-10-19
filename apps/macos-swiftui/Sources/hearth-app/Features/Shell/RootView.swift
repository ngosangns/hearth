import SwiftUI
import HearthKit
import NSShell

/// Left dock, center pane, log dock, status bar. Toast and confirm sit on `WindowShell`.
struct RootView: View {
    @Environment(AppModel.self) private var model

    var body: some View {
        WindowShell(
            toast: model.toast,
            dismissToast: { model.dismissToast() },
            confirmation: model.confirmation,
            clearConfirmation: { model.confirmation = nil }
        ) {
            VStack(spacing: 0) {
                if model.cliMissing { CLIMissingBanner() }
                ColumnLayout()
                StatusBar()
            }
        }
    }
}

private struct CLIMissingBanner: View {
    @Environment(AppModel.self) private var model

    var body: some View {
        MissingToolBanner(message: "The hearth binary was not found. Run `task install` or set HEARTH_BIN.") {
            model.start()
        }
    }
}

/// Bottom status bar: what is running right now, plus the hearth version.
struct StatusBar: View {
    @Environment(AppModel.self) private var model

    var body: some View {
        StatusStrip(accessibilityLabel: model.operations.last.map { "Working: \($0.title)" } ?? "Ready") {
            HStack(spacing: Theme.Space.sm) {
                if let op = model.operations.last {
                    ProgressView().controlSize(.small)
                    Text(op.title + (model.operations.count > 1 ? " (+\(model.operations.count - 1))" : "") + "…")
                        .lineLimit(1)
                } else {
                    Image(systemName: Icon.success).foregroundStyle(.green).imageScale(.small)
                    Text("Ready")
                }
                Spacer()
                if !model.summary.isEmpty { Text(model.summary).monospacedDigit() }
                if let v = model.cliVersion { Text(v).monospacedDigit() }
            }
        }
    }
}

/// Sidebar and detail columns keep their own width (content width by default, draggable); the log
/// column takes everything that is left. With the log hidden the detail column fills the window.
/// `HSplitView` was dropped because it shares extra width between all panes and ignores `maxWidth`.
private struct ColumnLayout: View {
    @Environment(AppModel.self) private var model
    @AppStorage("hearth.sidebarWidth") private var sidebarWidth = Double(Column.sidebar.ideal)
    @AppStorage("hearth.detailWidth") private var detailWidth = Double(Column.detail.ideal)

    var body: some View {
        GeometryReader { geo in
            let logShown = model.showsLog
            let sidebar = Column.sidebar.clamp(sidebarWidth)
            // Never let the fixed columns squeeze the log below its minimum.
            let detailRoom = geo.size.width - sidebar - (logShown ? Column.logMin : 0)
            let detail = max(Column.detail.min, min(Column.detail.clamp(detailWidth), detailRoom))
            HStack(spacing: 0) {
                SidebarView().frame(width: sidebar)
                ColumnDivider(width: $sidebarWidth, range: Column.sidebar)
                if logShown {
                    DetailView().frame(width: detail)
                    ColumnDivider(width: $detailWidth, range: Column.detail)
                    LogView().frame(maxWidth: .infinity)
                } else {
                    DetailView().frame(maxWidth: .infinity)
                }
            }
        }
    }
}

private enum Column {
    struct Range { let min: CGFloat, ideal: CGFloat, max: CGFloat
        func clamp(_ w: CGFloat) -> CGFloat { Swift.min(max, Swift.max(min, w)) }
    }
    static let sidebar = Range(min: 220, ideal: 280, max: 400)
    static let detail = Range(min: 420, ideal: 560, max: 900)
    static let logMin: CGFloat = 280
}

/// One-point divider with a wider drag target that resizes the column on its left.
private struct ColumnDivider: View {
    @Binding var width: Double
    fileprivate let range: Column.Range
    @State private var startWidth: Double?

    var body: some View {
        Divider()
            .overlay {
                Color.clear.frame(width: 9).contentShape(Rectangle())
                    .onHover { inside in
                        if inside { NSCursor.resizeLeftRight.push() } else { NSCursor.pop() }
                    }
                    .gesture(
                        DragGesture(minimumDistance: 1, coordinateSpace: .global)
                            .onChanged { drag in
                                let start = startWidth ?? width
                                startWidth = start
                                width = range.clamp(start + drag.translation.width)
                            }
                            .onEnded { _ in startWidth = nil }
                    )
            }
            .accessibilityHidden(true)
    }
}
