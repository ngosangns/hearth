import SwiftUI

/// An inline (not modal) live log tail for whichever service is selected in `ServiceListView`'s
/// split view. The `LogController` is owned by `WorkspaceController` and reused when the same
/// service is focused again — this view only starts/stops its poll loop.
struct ServiceLogPanel: View {
    let serviceLabel: String
    @ObservedObject var log: LogController

    var body: some View {
        VStack(spacing: 0) {
            HStack {
                Text(serviceLabel).font(.headline)
                Spacer()
                if let error = log.lastError {
                    Label(error, systemImage: log.isGone ? "questionmark.circle" : "exclamationmark.circle")
                        .font(.caption)
                        .foregroundStyle(log.isGone ? Color.secondary : Color.red)
                        .lineLimit(1)
                }
            }
            .padding()
            Divider()
            LogTextView(text: log.text)
        }
        .onAppear { log.start() }
        .onDisappear { log.stop() }
    }
}
