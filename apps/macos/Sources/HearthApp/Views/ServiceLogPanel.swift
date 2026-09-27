import AppKit
import SwiftUI

/// An inline (not modal) live log tail for whichever service is selected in `ServiceListView`'s
/// split view. The `LogController` is owned by `WorkspaceController` and reused when the same
/// service is focused again — this view only starts/stops its poll loop.
struct ServiceLogPanel: View {
    let serviceLabel: String
    /// Optional live state for the badge in the header (nil for the daemon log row).
    var state: String?
    @ObservedObject var log: LogController

    var body: some View {
        VStack(spacing: 0) {
            HStack(spacing: 8) {
                Image(systemName: "terminal")
                    .foregroundStyle(.secondary)
                Text(serviceLabel).font(.headline)
                if let state {
                    StateBadge(state: state)
                }
                Spacer()
                if let error = log.lastError {
                    Label(error, systemImage: log.isGone ? "questionmark.circle" : "exclamationmark.circle")
                        .font(.caption)
                        .foregroundStyle(log.isGone ? Color.secondary : Color.red)
                        .lineLimit(1)
                }
                IconActionButton("Copy the visible log buffer", systemImage: "doc.on.doc") {
                    NSPasteboard.general.clearContents()
                    NSPasteboard.general.setString(log.text, forType: .string)
                }
                .disabled(log.text.isEmpty)
            }
            .padding(.horizontal, 12)
            .padding(.vertical, 8)
            .background(.bar)
            Divider()
            LogTextView(text: log.text)
                .background(Color(nsColor: .underPageBackgroundColor))
        }
        .onAppear { log.start() }
        .onDisappear { log.stop() }
    }
}
