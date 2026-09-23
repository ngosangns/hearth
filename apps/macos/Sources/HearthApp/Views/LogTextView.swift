import AppKit
import SwiftUI

/// Append-only monospaced log view. Replaces the whole buffer only when the controller resets
/// (rotation / stale cursor); otherwise appends the delta so SwiftUI does not rebuild a 256KB `Text`.
struct LogTextView: NSViewRepresentable {
    let text: String
    var isEmptyPlaceholder: Bool { text.isEmpty }

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
        textView.textColor = .labelColor
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
        textView.string = isEmptyPlaceholder ? "Waiting for output…" : text
        if isEmptyPlaceholder { textView.textColor = .secondaryLabelColor }

        scroll.documentView = textView
        context.coordinator.textView = textView
        context.coordinator.lastText = text
        return scroll
    }

    func updateNSView(_ scroll: NSScrollView, context: Context) {
        guard let textView = context.coordinator.textView else { return }
        let display = isEmptyPlaceholder ? "Waiting for output…" : text
        textView.textColor = isEmptyPlaceholder ? .secondaryLabelColor : .labelColor

        let last = context.coordinator.lastText
        if text.isEmpty || last.isEmpty || !text.hasPrefix(last) {
            textView.string = display
        } else if text.count > last.count {
            let appended = String(text.dropFirst(last.count))
            textView.textStorage?.append(NSAttributedString(
                string: appended,
                attributes: [
                    .font: NSFont.monospacedSystemFont(ofSize: NSFont.systemFontSize, weight: .regular),
                    .foregroundColor: NSColor.labelColor,
                ]
            ))
            let end = NSRange(location: textView.string.utf16.count, length: 0)
            textView.scrollRangeToVisible(end)
        }
        context.coordinator.lastText = text
    }

    final class Coordinator {
        var textView: NSTextView?
        var lastText = ""
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
