import SwiftUI

/// An inline (not modal) live log tail for whichever service is selected in `ServiceListView`'s
/// split view. One instance per selection — the parent gives it a fresh `LogController` (and resets
/// SwiftUI identity via `.id(selectedServiceId)`) every time the selection changes, so there is never
/// a stale poll loop running for a service that's no longer focused.
struct ServiceLogPanel: View {
    let serviceLabel: String
    @StateObject private var log: LogController

    init(serviceLabel: String, controller: LogController) {
        self.serviceLabel = serviceLabel
        _log = StateObject(wrappedValue: controller)
    }

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
            ScrollViewReader { proxy in
                ScrollView {
                    Text(log.text.isEmpty ? "Waiting for output…" : log.text)
                        .font(.system(.body, design: .monospaced))
                        .foregroundStyle(log.text.isEmpty ? .secondary : .primary)
                        .textSelection(.enabled)
                        .frame(maxWidth: .infinity, alignment: .leading)
                        .padding(8)
                        .id("bottom")
                }
                .onChange(of: log.text) { _ in
                    withAnimation(.easeOut(duration: 0.1)) { proxy.scrollTo("bottom", anchor: .bottom) }
                }
            }
        }
        .onAppear { log.start() }
        .onDisappear { log.stop() }
    }
}
