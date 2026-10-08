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
                    Button { model.loadEarlier() } label: { Label("Earlier", systemImage: Icon.earlier) }
                        .disabled(model.logLoading)
                        .help("Load the previous page. Scrolling to the top loads it too.")
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
                LogText(text: model.log.text, revision: model.log.revision, resetToken: model.logEpoch,
                        prepended: model.log.prepended) { model.loadEarlier() }
            }
        }
    }
}

/// A selectable monospaced text view that sticks to the bottom while following.
struct LogText: NSViewRepresentable {
    let text: String
    let revision: Int
    let resetToken: Int
    /// UTF-16 units added at the front of `text` since the previous revision.
    let prepended: Int
    let onNeedEarlier: () -> Void

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
        textView.isRichText = true
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
        c.onNeedEarlier = onNeedEarlier
        guard let textView = c.textView else { return }
        if c.lastToken != resetToken { c.lastToken = resetToken; c.follow = true }
        guard c.lastRevision != revision else { return }
        c.lastRevision = revision
        let anchor = c.follow ? nil : c.topCharacter()
        let selected = textView.selectedRanges
        c.performScroll {
            textView.textStorage?.setAttributedString(Self.colored(text))
            let length = (textView.string as NSString).length
            textView.selectedRanges = Self.shifted(selected, by: prepended, limit: length)
            if c.follow {
                c.scrollToEnd()
            } else if let anchor {
                c.scrollCharacterToTop(anchor + prepended)
            }
        }
        c.latest?.isHidden = c.follow
        // A tail shorter than the pane still has earlier bytes: pull a page so the column fills.
        // A reader parked within a viewport of the top pulls the next page. The tail-follow case
        // (content taller than the pane) does not.
        if let scroll = c.scroll {
            let bounds = scroll.contentView.bounds
            let document = scroll.documentView?.frame.height ?? 0
            let short = document > 0 && document <= bounds.height + 1
            let awayFromTail = document - bounds.origin.y - bounds.height >= 48
            if short || (!c.follow && awayFromTail && bounds.origin.y < bounds.height) {
                // After this update, so the model change is not nested inside the view render.
                DispatchQueue.main.async { [weak c] in c?.onNeedEarlier() }
            }
        }
    }

    /// Selection indexes are UTF-16, matching `prepended`.
    static func shifted(_ ranges: [NSValue], by delta: Int, limit: Int) -> [NSValue] {
        guard delta != 0 else { return ranges }
        return ranges.map { value in
            let range = value.rangeValue
            let start = min(max(0, range.location + delta), limit)
            let end = min(max(start, range.location + delta + range.length), limit)
            return NSValue(range: NSRange(location: start, length: end - start))
        }
    }

    static func colored(_ text: String) -> NSAttributedString {
        let font = NSFont.monospacedSystemFont(ofSize: 11.5, weight: .regular)
        let bold = NSFont.monospacedSystemFont(ofSize: 11.5, weight: .semibold)
        let out = NSMutableAttributedString()
        for run in LogBuffer.colorRuns(text) {
            var attrs: [NSAttributedString.Key: Any] = [
                .font: run.bold ? bold : font,
                .foregroundColor: ansiColor(run.fg, indexed: run.indexed) ?? .labelColor,
            ]
            if let bg = ansiColor(run.bg, indexed: run.indexed) { attrs[.backgroundColor] = bg }
            if run.italic { attrs[.obliqueness] = 0.2 }
            if run.underline { attrs[.underlineStyle] = NSUnderlineStyle.single.rawValue }
            if run.dim { attrs[.foregroundColor] = (attrs[.foregroundColor] as? NSColor)?.withAlphaComponent(0.55) ?? NSColor.labelColor.withAlphaComponent(0.55) }
            out.append(NSAttributedString(string: run.text, attributes: attrs))
        }
        return out
    }

    /// ANSI 0–15, plus the 256-color cube and greys. Nil is the label color.
    static func ansiColor(_ index: Int?, indexed: Bool) -> NSColor? {
        guard let index else { return nil }
        if !indexed, (0...15).contains(index) { return palette[index] }
        if (0...15).contains(index) { return palette[index] }
        if (232...255).contains(index) {
            let level = CGFloat(8 + (index - 232) * 10) / 255
            return NSColor(calibratedWhite: level, alpha: 1)
        }
        if (16...231).contains(index) {
            let n = index - 16
            let steps: [CGFloat] = [0, 95, 135, 175, 215, 255]
            return NSColor(calibratedRed: steps[n / 36] / 255, green: steps[(n / 6) % 6] / 255, blue: steps[n % 6] / 255, alpha: 1)
        }
        return nil
    }

    private static let palette: [NSColor] = [
        .black, NSColor(calibratedRed: 0.8, green: 0.15, blue: 0.15, alpha: 1),
        NSColor(calibratedRed: 0.15, green: 0.65, blue: 0.2, alpha: 1),
        NSColor(calibratedRed: 0.75, green: 0.6, blue: 0.1, alpha: 1),
        NSColor(calibratedRed: 0.2, green: 0.4, blue: 0.9, alpha: 1),
        NSColor(calibratedRed: 0.7, green: 0.25, blue: 0.7, alpha: 1),
        NSColor(calibratedRed: 0.15, green: 0.65, blue: 0.7, alpha: 1),
        NSColor(calibratedWhite: 0.75, alpha: 1),
        NSColor(calibratedWhite: 0.45, alpha: 1),
        NSColor(calibratedRed: 1, green: 0.35, blue: 0.35, alpha: 1),
        NSColor(calibratedRed: 0.4, green: 0.9, blue: 0.45, alpha: 1),
        NSColor(calibratedRed: 1, green: 0.9, blue: 0.4, alpha: 1),
        NSColor(calibratedRed: 0.45, green: 0.65, blue: 1, alpha: 1),
        NSColor(calibratedRed: 0.9, green: 0.5, blue: 0.9, alpha: 1),
        NSColor(calibratedRed: 0.45, green: 0.95, blue: 0.95, alpha: 1),
        .white,
    ]

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
        var onNeedEarlier: () -> Void = {}
        private var programmatic = false

        func performScroll(_ body: () -> Void) {
            let was = programmatic
            programmatic = true
            body()
            programmatic = was
        }

        func scrollToEnd() {
            textView?.scrollToEndOfDocument(nil)
        }

        /// `index` is a UTF-16 offset. Puts that character at the top of the clip.
        func scrollCharacterToTop(_ index: Int) {
            guard let textView, let scroll, let layout = textView.layoutManager, let container = textView.textContainer else { return }
            let length = (textView.string as NSString).length
            guard length > 0 else { return }
            layout.ensureLayout(for: container)
            let clamped = min(max(0, index), length - 1)
            let glyphs = layout.glyphRange(forCharacterRange: NSRange(location: clamped, length: 0), actualCharacterRange: nil)
            var rect = layout.boundingRect(forGlyphRange: glyphs, in: container)
            rect.origin.x += textView.textContainerOrigin.x
            rect.origin.y += textView.textContainerOrigin.y
            scroll.contentView.scroll(to: NSPoint(x: 0, y: max(0, rect.minY)))
            scroll.reflectScrolledClipView(scroll.contentView)
        }

        func topCharacter() -> Int {
            guard let textView, let scroll, let layout = textView.layoutManager, let container = textView.textContainer else { return 0 }
            layout.ensureLayout(for: container)
            let visible = scroll.contentView.bounds
            let point = NSPoint(
                x: visible.minX + textView.textContainerInset.width + 1 - textView.textContainerOrigin.x,
                y: visible.minY + textView.textContainerInset.height + 1 - textView.textContainerOrigin.y)
            return layout.characterIndex(for: point, in: container, fractionOfDistanceBetweenInsertionPoints: nil)
        }

        @objc func didScroll(_ note: Notification) {
            guard !programmatic, let scroll else { return }
            let clip = scroll.contentView
            let document = scroll.documentView?.frame.height ?? 0
            follow = document - clip.bounds.origin.y - clip.bounds.height < 48
            latest?.isHidden = follow
            // One viewport from the top starts the previous page, so the fetch lands before the edge.
            if !follow, clip.bounds.origin.y < clip.bounds.height { onNeedEarlier() }
        }

        @objc func jump() {
            follow = true
            latest?.isHidden = true
            performScroll { scrollToEnd() }
        }
    }
}
