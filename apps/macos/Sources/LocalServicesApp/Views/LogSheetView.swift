import SwiftUI

struct LogSheetView: View {
    let serviceLabel: String
    @StateObject private var log: LogController
    @Environment(\.dismiss) private var dismiss

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
                    Text(error).font(.caption).foregroundStyle(.red).lineLimit(1)
                }
                Button("Done") { dismiss() }
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
        .frame(minWidth: 560, minHeight: 360)
        .onAppear { log.start() }
        .onDisappear { log.stop() }
    }
}
