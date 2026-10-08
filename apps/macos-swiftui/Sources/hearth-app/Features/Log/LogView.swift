import SwiftUI
import AppKit
import HearthKit

/// Third column: the selected service's log (or the daemon log). Follows the tail until the
/// user scrolls up; a Latest button jumps back.
struct LogView: View {
    @Environment(AppModel.self) private var model

    var body: some View {
        let name = model.selectedService ?? ""
        VStack(spacing: 0) {
            PaneHeader(title: "Log", subtitle: name, systemImage: Icon.log) {
                if model.log.hasMore {
                    Button { model.expandLog() } label: { Label("Earlier", systemImage: Icon.earlier) }
                        .help("Load a larger window of earlier output")
                }
                Button { Finder.copy(model.log.plain) } label: { Label("Copy", systemImage: Icon.copy).labelStyle(.iconOnly) }
                    .disabled(model.log.text.isEmpty).help("Copy the visible log")
                Button { model.toggleLog() } label: { Label("Hide", systemImage: Icon.close).labelStyle(.iconOnly) }
                    .help("Hide the log column")
            }
            if model.selectedService == nil {
                StateView(systemImage: Icon.log, title: "Select a service",
                          message: "Choose a service to read its output.")
            } else if model.log.text.isEmpty {
                StateView(systemImage: Icon.log, title: "No output yet")
            } else {
                LogText(text: model.log.plain, revision: model.log.revision, resetToken: model.logEpoch)
            }
        }
    }
}

/// A selectable monospaced text view that sticks to the bottom while following.
struct LogText: NSViewRepresentable {
    let text: String
    let revision: Int
    let resetToken: Int

    func makeCoordinator() -> Coordinator { Coordinator() }

    func makeNSView(context: Context) -> NSView {
        let container = NSView()
        let scroll = NSScrollView()
        scroll.hasVerticalScroller = true
        scroll.autohidesScrollers = true
        scroll.drawsBackground = false
        scroll.translatesAutoresizingMaskIntoConstraints = false

        let textView = NSTextView()
        textView.isEditable = false
        textView.isSelectable = true
        textView.isRichText = false
        textView.drawsBackground = false
        textView.font = .monospacedSystemFont(ofSize: 11.5, weight: .regular)
        textView.textColor = .labelColor
        textView.textContainerInset = NSSize(width: 8, height: 8)
        textView.minSize = .zero
        textView.maxSize = NSSize(width: CGFloat.greatestFiniteMagnitude, height: CGFloat.greatestFiniteMagnitude)
        textView.isVerticallyResizable = true
        textView.isHorizontallyResizable = false
        textView.autoresizingMask = [.width]
        textView.textContainer?.widthTracksTextView = true
        textView.setAccessibilityLabel("Service log")
        scroll.documentView = textView

        let latest = NSButton(title: "Latest", target: context.coordinator, action: #selector(Coordinator.jump))
        latest.image = NSImage(systemSymbolName: Icon.latest, accessibilityDescription: nil)
        latest.imagePosition = .imageLeading
        latest.bezelStyle = .rounded
        latest.controlSize = .small
        latest.isHidden = true
        latest.translatesAutoresizingMaskIntoConstraints = false

        container.addSubview(scroll)
        container.addSubview(latest)
        NSLayoutConstraint.activate([
            scroll.leadingAnchor.constraint(equalTo: container.leadingAnchor),
            scroll.trailingAnchor.constraint(equalTo: container.trailingAnchor),
            scroll.topAnchor.constraint(equalTo: container.topAnchor),
            scroll.bottomAnchor.constraint(equalTo: container.bottomAnchor),
            latest.trailingAnchor.constraint(equalTo: container.trailingAnchor, constant: -12),
            latest.bottomAnchor.constraint(equalTo: container.bottomAnchor, constant: -12),
        ])
        let c = context.coordinator
        c.textView = textView
        c.scroll = scroll
        c.latest = latest
        scroll.contentView.postsBoundsChangedNotifications = true
        NotificationCenter.default.addObserver(c, selector: #selector(Coordinator.didScroll),
                                               name: NSView.boundsDidChangeNotification, object: scroll.contentView)
        return container
    }

    func updateNSView(_ nsView: NSView, context: Context) {
        let c = context.coordinator
        guard let textView = c.textView else { return }
        if c.lastToken != resetToken { c.lastToken = resetToken; c.follow = true }
        guard c.lastRevision != revision || c.lastToken != resetToken || textView.string != text else { return }
        c.lastRevision = revision
        if textView.string != text { textView.string = text }
        c.latest?.isHidden = c.follow
        if c.follow { c.scrollToEnd() }
    }

    static func dismantleNSView(_ nsView: NSView, coordinator: Coordinator) {
        NotificationCenter.default.removeObserver(coordinator)
    }

    @MainActor final class Coordinator: NSObject {
        weak var textView: NSTextView?
        weak var scroll: NSScrollView?
        weak var latest: NSButton?
        var follow = true
        var lastRevision = -1
        var lastToken = -1
        private var programmatic = false

        func scrollToEnd() {
            guard let textView else { return }
            programmatic = true
            textView.scrollToEndOfDocument(nil)
            programmatic = false
        }

        @objc func didScroll(_ note: Notification) {
            guard !programmatic, let scroll else { return }
            let clip = scroll.contentView
            let document = scroll.documentView?.frame.height ?? 0
            follow = document - clip.bounds.origin.y - clip.bounds.height < 48
            latest?.isHidden = follow
        }

        @objc func jump() {
            follow = true
            latest?.isHidden = true
            scrollToEnd()
        }
    }
}
