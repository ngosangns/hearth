import AppKit

/// Dedicated right column for logs: header with collapse/earlier, a monospaced
/// NSTextView with follow-scroll, and a floating "Latest" pill. When closed the
/// column collapses to a 56pt strip holding just the toggle.
final class LogColumnController: NSViewController {

    private let desk: DeskController

    private let header = NSStackView()
    private let toggle = NSButton()
    private let titleLabel = NSTextField()
    private let earlier = CallbackButton(title: "Earlier", symbol: Symbols.earlier)
    private let spinner = NSProgressIndicator()
    private let textView = NSTextView()
    private let scrollView = NSScrollView()
    private let body = NSView()
    private let jump = NSButton()

    private var snapshot: DeskController.Snapshot?
    private var applyScheduled = false
    private var follow = true
    private var lastSeq = -1

    init(desk: DeskController) {
        self.desk = desk
        super.init(nibName: nil, bundle: nil)
    }

    required init?(coder: NSCoder) { fatalError() }

    override func loadView() {
        view = NSView()
        view.wantsLayer = true
        view.layer?.backgroundColor = NSColor.underPageBackgroundColor.cgColor

        // Header: [toggle] [title] [earlier] [spinner]
        toggle.isBordered = false
        toggle.image = symbolImage(Symbols.chevronLeft, 11)
        toggle.contentTintColor = .secondaryLabelColor
        toggle.target = self
        toggle.action = #selector(toggleLog)
        toggle.toolTip = "Show or hide the log column"
        toggle.setButtonType(.pushOnPushOff)
        toggle.state = .on

        titleLabel.stringValue = "Log"
        titleLabel.font = .systemFont(ofSize: 12, weight: .bold)
        titleLabel.isBezeled = false
        titleLabel.isEditable = false
        titleLabel.drawsBackground = false
        titleLabel.lineBreakMode = .byTruncatingTail

        earlier.onClick = { [weak self] in self?.desk.expandLog() }

        spinner.style = .spinning
        spinner.controlSize = .mini
        spinner.isDisplayedWhenStopped = false

        header.orientation = .horizontal
        header.spacing = 6
        header.alignment = .centerY
        header.edgeInsets = NSEdgeInsets(top: 6, left: 8, bottom: 6, right: 8)
        header.addArrangedSubview(toggle)
        header.addArrangedSubview(titleLabel)
        header.addArrangedSubview(NSView())
        header.addArrangedSubview(earlier)
        header.addArrangedSubview(spinner)
        header.translatesAutoresizingMaskIntoConstraints = false

        // Body: scrollable monospace text + floating "Latest" pill.
        textView.isEditable = false
        textView.isSelectable = true
        textView.font = .monospacedSystemFont(ofSize: 11.5, weight: .regular)
        textView.textColor = .labelColor
        textView.backgroundColor = .underPageBackgroundColor
        textView.textContainerInset = NSSize(width: 8, height: 8)
        // Scrollable-document plumbing: width tracks the clip view, height
        // grows with content. Without autoresizingMask .width the frame stays 0.
        textView.minSize = .zero
        textView.maxSize = NSSize(width: CGFloat.greatestFiniteMagnitude, height: CGFloat.greatestFiniteMagnitude)
        textView.isVerticallyResizable = true
        textView.isHorizontallyResizable = false
        textView.autoresizingMask = [.width]
        textView.textContainer?.widthTracksTextView = true

        scrollView.documentView = textView
        scrollView.drawsBackground = false
        scrollView.hasVerticalScroller = true
        scrollView.scrollerStyle = .overlay
        scrollView.autohidesScrollers = true
        scrollView.translatesAutoresizingMaskIntoConstraints = false

        jump.isBordered = false
        jump.image = symbolImage(Symbols.latest, 10)
        jump.imagePosition = .imageLeading
        jump.title = "Latest"
        jump.font = .systemFont(ofSize: 10, weight: .semibold)
        jump.wantsLayer = true
        jump.layer?.cornerRadius = 10
        jump.layer?.backgroundColor = NSColor.controlBackgroundColor.cgColor
        jump.layer?.borderWidth = 0.5
        jump.layer?.borderColor = NSColor.separatorColor.cgColor
        jump.target = self
        jump.action = #selector(jumpToLatest)
        jump.isHidden = true
        jump.translatesAutoresizingMaskIntoConstraints = false

        body.addSubview(scrollView)
        body.addSubview(jump)
        scrollView.translatesAutoresizingMaskIntoConstraints = false
        body.translatesAutoresizingMaskIntoConstraints = false
        NSLayoutConstraint.activate([
            scrollView.leadingAnchor.constraint(equalTo: body.leadingAnchor),
            scrollView.trailingAnchor.constraint(equalTo: body.trailingAnchor),
            scrollView.topAnchor.constraint(equalTo: body.topAnchor),
            scrollView.bottomAnchor.constraint(equalTo: body.bottomAnchor),
            jump.trailingAnchor.constraint(equalTo: body.trailingAnchor, constant: -10),
            jump.bottomAnchor.constraint(equalTo: body.bottomAnchor, constant: -10),
            jump.heightAnchor.constraint(equalToConstant: 22),
        ])

        let separator = NSView()
        separator.wantsLayer = true
        separator.layer?.backgroundColor = NSColor.separatorColor.cgColor
        separator.translatesAutoresizingMaskIntoConstraints = false

        view.addSubview(header)
        view.addSubview(separator)
        view.addSubview(body)
        NSLayoutConstraint.activate([
            header.leadingAnchor.constraint(equalTo: view.leadingAnchor),
            header.trailingAnchor.constraint(equalTo: view.trailingAnchor),
            // The unified toolbar overlays the column top — keep the header
            // below it via the safe area guide.
            header.topAnchor.constraint(equalTo: view.safeAreaLayoutGuide.topAnchor),
            separator.leadingAnchor.constraint(equalTo: view.leadingAnchor),
            separator.trailingAnchor.constraint(equalTo: view.trailingAnchor),
            separator.topAnchor.constraint(equalTo: header.bottomAnchor),
            separator.heightAnchor.constraint(equalToConstant: 0.5),
            body.leadingAnchor.constraint(equalTo: view.leadingAnchor),
            body.trailingAnchor.constraint(equalTo: view.trailingAnchor),
            body.topAnchor.constraint(equalTo: separator.bottomAnchor),
            body.bottomAnchor.constraint(equalTo: view.bottomAnchor),
        ])

        NotificationCenter.default.addObserver(
            self, selector: #selector(didScroll),
            name: NSView.boundsDidChangeNotification, object: scrollView.contentView)
    }

    deinit {
        NotificationCenter.default.removeObserver(self)
    }

    func apply(_ snapshot: DeskController.Snapshot) {
        self.snapshot = snapshot
        if applyScheduled { return }
        applyScheduled = true
        DispatchQueue.main.async { [weak self] in
            self?.applyScheduled = false
            self?.rebuild()
        }
    }

    private func rebuild() {
        guard let snapshot else { return }

        let expanded = snapshot.logOpen
        body.isHidden = !expanded
        earlier.isHidden = !(expanded && snapshot.logHasMore)
        toggle.image = symbolImage(expanded ? Symbols.chevronRight : Symbols.chevronLeft, 11)
        toggle.state = expanded ? .on : .off
        titleLabel.isHidden = !expanded

        let name = snapshot.selectedService == "$daemon" ? "daemon" : snapshot.selectedService
        titleLabel.stringValue = "Log — \(name)"

        // A new service selection resets the follow-scroll.
        if snapshot.logSeq != lastSeq {
            lastSeq = snapshot.logSeq
            follow = true
            jump.isHidden = true
        }

        if expanded {
            if snapshot.logText.isEmpty {
                if textView.string != placeholder {
                    let attributes: [NSAttributedString.Key: Any] = [
                        .font: NSFont.systemFont(ofSize: 11.5),
                        .foregroundColor: NSColor.secondaryLabelColor,
                    ]
                    textView.textStorage?.setAttributedString(
                        NSAttributedString(string: placeholder, attributes: attributes))
                }
            } else if textView.string != snapshot.logText {
                textView.string = snapshot.logText
                if follow { scrollToBottom() }
            }
        }

        snapshot.busy > 0 && expanded ? spinner.startAnimation(nil) : spinner.stopAnimation(nil)
    }

    private let placeholder = "No output yet."

    @objc private func toggleLog() {
        desk.toggleLog()
    }

    @objc private func jumpToLatest() {
        follow = true
        jump.isHidden = true
        scrollToBottom()
    }

    @objc private func didScroll(_ notification: Notification) {
        let clip = scrollView.contentView
        let document = scrollView.documentView?.frame.height ?? 0
        let nearBottom = document - clip.bounds.origin.y - clip.bounds.height < 48
        follow = nearBottom
        jump.isHidden = nearBottom || textView.string.isEmpty
    }

    private func scrollToBottom() {
        textView.layoutManager?.ensureLayout(for: textView.textContainer!)
        textView.scrollToEndOfDocument(nil)
    }
}
