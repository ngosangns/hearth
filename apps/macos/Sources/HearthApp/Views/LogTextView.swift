import AppKit
import Combine
import SwiftUI

/// Append-only monospaced log view. Follows the controller's `deltas` straight into the
/// `NSTextStorage` — appending, trimming the head in place, replacing only on a reset — so a poll
/// costs the size of what changed, not a diff and re-layout of the whole 256KB buffer.
struct LogTextView: NSViewRepresentable {
    let log: LogController

    private static let placeholder = "Waiting for output…"
    private static let attributes: [NSAttributedString.Key: Any] = [
        .font: NSFont.monospacedSystemFont(ofSize: NSFont.systemFontSize, weight: .regular),
        .foregroundColor: NSColor.labelColor,
    ]

    func makeCoordinator() -> Coordinator { Coordinator() }

    func makeNSView(context: Context) -> NSScrollView {
        let scroll = NSScrollView()
        scroll.hasVerticalScroller = true
        scroll.hasHorizontalScroller = false
        scroll.autohidesScrollers = true
        scroll.borderType = .noBorder
        scroll.drawsBackground = false

        let textView = NSTextView()
        textView.isEditable = false
        textView.isSelectable = true
        textView.drawsBackground = false
        textView.isRichText = false
        textView.font = .monospacedSystemFont(ofSize: NSFont.systemFontSize, weight: .regular)
        textView.textContainerInset = NSSize(width: 8, height: 8)
        textView.isVerticallyResizable = true
        textView.isHorizontallyResizable = false
        // The document view must track the clip view's width — without the mask a text view
        // created while the pane is still zero-sized keeps a zero-width frame forever, and
        // `widthTracksTextView` then wraps every line into invisibility (blank panel).
        textView.autoresizingMask = [.width]
        textView.textContainer?.widthTracksTextView = true
        textView.textContainer?.containerSize = NSSize(width: 0, height: CGFloat.greatestFiniteMagnitude)
        textView.minSize = NSSize(width: 0, height: 0)
        textView.maxSize = NSSize(width: CGFloat.greatestFiniteMagnitude, height: CGFloat.greatestFiniteMagnitude)

        scroll.documentView = textView
        context.coordinator.attach(textView, to: log)
        return scroll
    }

    func updateNSView(_ scroll: NSScrollView, context: Context) {
        context.coordinator.attach(nil, to: log) // re-subscribes only if the controller changed
    }

    @MainActor
    final class Coordinator {
        private weak var textView: NSTextView?
        private weak var log: LogController?
        private var subscription: AnyCancellable?
        private var showingPlaceholder = false

        /// Seeds the view from `log.text`, then follows `log.deltas`. Both happen on the main
        /// actor in one step, so no delta can land between the seed and the subscription.
        func attach(_ newTextView: NSTextView?, to newLog: LogController) {
            if let newTextView { textView = newTextView } else if newLog === log { return }
            log = newLog
            replace(with: newLog.text)
            subscription = newLog.deltas.sink { [weak self] delta in self?.apply(delta) }
        }

        private func apply(_ delta: LogDelta) {
            switch delta {
            case .reset(let text):
                replace(with: text)
            case .append(let text, let trimmedUTF16):
                guard let textView, let storage = textView.textStorage else { return }
                if showingPlaceholder {
                    replace(with: text)
                    return
                }
                let follow = isScrolledToBottom(textView)
                storage.beginEditing()
                if trimmedUTF16 > 0 {
                    storage.deleteCharacters(in: NSRange(location: 0, length: min(trimmedUTF16, storage.length)))
                }
                storage.append(NSAttributedString(string: text, attributes: LogTextView.attributes))
                storage.endEditing()
                if follow { scrollToEnd(textView) }
            }
        }

        private func replace(with text: String) {
            guard let textView, let storage = textView.textStorage else { return }
            // Full replacements still respect the read position: a `.reset` delta (the daemon
            // log's only shape) must not pull a scrolled-up user back to the tail every poll.
            let wasAtBottom = showingPlaceholder || isScrolledToBottom(textView)
            showingPlaceholder = text.isEmpty
            if showingPlaceholder {
                var attributes = LogTextView.attributes
                attributes[.foregroundColor] = NSColor.secondaryLabelColor
                storage.setAttributedString(NSAttributedString(string: LogTextView.placeholder, attributes: attributes))
            } else {
                storage.setAttributedString(NSAttributedString(string: text, attributes: LogTextView.attributes))
                if wasAtBottom { scrollToEnd(textView) }
            }
        }

        /// Appends keep following the tail only while the user is already at it — scrolling up to
        /// read something must not be undone by the next poll.
        private func isScrolledToBottom(_ textView: NSTextView) -> Bool {
            guard let clip = textView.enclosingScrollView?.contentView else { return true }
            return clip.bounds.maxY >= textView.frame.maxY - 24
        }

        private func scrollToEnd(_ textView: NSTextView) {
            textView.scrollRangeToVisible(NSRange(location: (textView.string as NSString).length, length: 0))
        }
    }
}

struct WindowFrameAutosave: NSViewRepresentable {
    let name: String

    func makeNSView(context: Context) -> NSView {
        let view = NSView()
        DispatchQueue.main.async {
            view.window?.setFrameAutosaveName(name)
        }
        return view
    }

    func updateNSView(_ nsView: NSView, context: Context) {
        DispatchQueue.main.async {
            nsView.window?.setFrameAutosaveName(name)
        }
    }
}
