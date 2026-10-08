import AppKit

enum Symbols {
    static let flame = "flame"
    static let folder = "folder"
    static let folderPlus = "folder.badge.plus"
    static let share = "point.3.connected.trianglepath.dotted"
    static let play = "play.fill"
    static let stop = "stop.fill"
    static let restart = "arrow.clockwise"
    static let trash = "trash"
    static let shield = "checkmark.shield"
    static let terminal = "terminal"
    static let chevronDown = "chevron.down"
    static let chevronRight = "chevron.right"
    static let chevronLeft = "chevron.left"
    static let external = "arrow.up.right.square"
    static let earlier = "clock.arrow.circlepath"
    static let cube = "cube"
    static let drives = "externaldrive.connected.to.line.below"
    static let warning = "exclamationmark.circle"
    static let download = "square.and.arrow.down"
    static let info = "info.circle"
    static let latest = "arrow.down.to.line"
    static let sidebarRight = "sidebar.right"
    static let sidebarLeft = "sidebar.left"
}

func symbolImage(_ name: String, _ size: CGFloat = 13) -> NSImage? {
    let config = NSImage.SymbolConfiguration(pointSize: size, weight: .medium)
    return NSImage(systemSymbolName: name, accessibilityDescription: nil)?.withSymbolConfiguration(config)
}

func makeLabel(_ text: String, size: CGFloat = 13, weight: NSFont.Weight = .regular,
               color: NSColor = .labelColor, mono: Bool = false) -> NSTextField {
    let label = NSTextField(labelWithString: text)
    label.font = mono
        ? .monospacedSystemFont(ofSize: size, weight: weight)
        : .systemFont(ofSize: size, weight: weight)
    label.textColor = color
    label.lineBreakMode = .byTruncatingTail
    label.setContentCompressionResistancePriority(.defaultLow, for: .horizontal)
    return label
}

/// Small rounded status chip ("infra", "job", "disabled", "missing"…).
final class ChipView: NSView {
    init(_ text: String, color: NSColor = .secondaryLabelColor) {
        super.init(frame: .zero)
        wantsLayer = true
        layer?.cornerRadius = 6
        layer?.backgroundColor = color.withAlphaComponent(0.12).cgColor
        let label = makeLabel(text, size: 9, weight: .semibold, color: color)
        label.translatesAutoresizingMaskIntoConstraints = false
        addSubview(label)
        NSLayoutConstraint.activate([
            label.leadingAnchor.constraint(equalTo: leadingAnchor, constant: 5),
            label.trailingAnchor.constraint(equalTo: trailingAnchor, constant: -5),
            label.topAnchor.constraint(equalTo: topAnchor, constant: 1),
            label.bottomAnchor.constraint(equalTo: bottomAnchor, constant: -1),
        ])
        setContentHuggingPriority(.required, for: .horizontal)
    }

    required init?(coder: NSCoder) { fatalError() }
}

/// State pill with a semantic dot, e.g. "● running".
final class PillView: NSView {
    init(_ text: String, color: NSColor) {
        super.init(frame: .zero)
        wantsLayer = true
        layer?.cornerRadius = 8
        layer?.backgroundColor = color.withAlphaComponent(0.12).cgColor
        let dot = NSView()
        dot.wantsLayer = true
        dot.layer?.cornerRadius = 3
        dot.layer?.backgroundColor = color.cgColor
        let label = makeLabel(text, size: 10, weight: .semibold, color: color)
        let stack = NSStackView(views: [dot, label])
        stack.spacing = 5
        stack.alignment = .centerY
        stack.translatesAutoresizingMaskIntoConstraints = false
        addSubview(stack)
        NSLayoutConstraint.activate([
            dot.widthAnchor.constraint(equalToConstant: 6),
            dot.heightAnchor.constraint(equalToConstant: 6),
            stack.leadingAnchor.constraint(equalTo: leadingAnchor, constant: 8),
            stack.trailingAnchor.constraint(equalTo: trailingAnchor, constant: -8),
            stack.topAnchor.constraint(equalTo: topAnchor, constant: 2),
            stack.bottomAnchor.constraint(equalTo: bottomAnchor, constant: -2),
        ])
        setContentHuggingPriority(.required, for: .horizontal)
    }

    required init?(coder: NSCoder) { fatalError() }
}

/// A clickable row with Finder-style rounded selection and hover fill.
final class RowButton: NSView {
    var onClick: () -> Void = {}
    var selected = false {
        didSet { needsDisplay = true }
    }

    private var hovering = false

    override init(frame: NSRect) {
        super.init(frame: frame)
        wantsLayer = true
        layer?.cornerRadius = 6
        let area = NSTrackingArea(
            rect: .zero,
            options: [.activeInKeyWindow, .inVisibleRect, .mouseEnteredAndExited],
            owner: self
        )
        addTrackingArea(area)
    }

    required init?(coder: NSCoder) { fatalError() }

    override func updateLayer() {
        if selected {
            layer?.backgroundColor = NSColor.selectedContentBackgroundColor
                .withAlphaComponent(0.85).cgColor
        } else if hovering {
            layer?.backgroundColor = NSColor.unemphasizedSelectedContentBackgroundColor
                .withAlphaComponent(0.5).cgColor
        } else {
            layer?.backgroundColor = NSColor.clear.cgColor
        }
    }

    override func mouseEntered(with event: NSEvent) { hovering = true; needsDisplay = true }
    override func mouseExited(with event: NSEvent) { hovering = false; needsDisplay = true }

    override func mouseUp(with event: NSEvent) {
        guard bounds.contains(convert(event.locationInWindow, from: nil)) else { return }
        onClick()
    }

    override var acceptsFirstResponder: Bool { true }

    override func keyDown(with event: NSEvent) {
        if event.keyCode == 36 || event.keyCode == 49 { onClick() }
    }
}

enum ButtonKind {
    case normal, primary, danger, armed, armedDanger
}

/// A push button carrying its own click closure — no selector plumbing.
final class CallbackButton: NSButton {
    var onClick: (() -> Void)?

    init(title: String, symbol: String? = nil, kind: ButtonKind = .normal, onClick: (() -> Void)? = nil) {
        super.init(frame: .zero)
        self.title = title
        bezelStyle = .rounded
        controlSize = .regular
        font = .systemFont(ofSize: 12.5, weight: .medium)
        if let symbol, let image = symbolImage(symbol, 12) {
            self.image = image
            imagePosition = .imageLeading
            imageHugsTitle = true
        }
        switch kind {
        case .normal:
            break
        case .primary:
            bezelColor = .controlAccentColor
        case .danger, .armedDanger:
            contentTintColor = .systemRed
        case .armed:
            contentTintColor = .controlAccentColor
        }
        self.onClick = onClick
        target = self
        action = #selector(fire)
    }

    required init?(coder: NSCoder) { fatalError() }

    @objc private func fire() { onClick?() }
}

/// A macOS push button with optional SF Symbol, styled by kind.
func makeButton(_ title: String, symbol: String? = nil, kind: ButtonKind = .normal,
                target: AnyObject?, action: Selector?) -> NSButton {
    let button = NSButton(title: title, target: target, action: action)
    button.bezelStyle = .rounded
    button.controlSize = .regular
    button.font = .systemFont(ofSize: 12.5, weight: .medium)
    if let symbol, let image = symbolImage(symbol, 12) {
        button.image = image
        button.imagePosition = .imageLeading
        button.imageHugsTitle = true
    }
    switch kind {
    case .normal:
        break
    case .primary:
        button.bezelStyle = .rounded
        button.bezelColor = .controlAccentColor
    case .danger, .armedDanger:
        button.contentTintColor = .systemRed
    case .armed:
        button.contentTintColor = .controlAccentColor
    }
    return button
}

/// Honor the user's Reduce Motion accessibility setting before animating.
func prefersReducedMotion() -> Bool {
    NSWorkspace.shared.accessibilityDisplayShouldReduceMotion
}

/// "/Users/me/work/app" → "~/work/app" for compact display.
func abbreviateTilde(_ path: String) -> String {
    (path as NSString).abbreviatingWithTildeInPath
}

/// A 0.5pt separator inset 10pt from the leading edge, for use inside
/// `groupCard` between rows.
func hairline() -> NSView {
    let wrap = NSView()
    let line = NSView()
    line.wantsLayer = true
    line.layer?.backgroundColor = NSColor.separatorColor.cgColor
    line.translatesAutoresizingMaskIntoConstraints = false
    wrap.addSubview(line)
    NSLayoutConstraint.activate([
        line.leadingAnchor.constraint(equalTo: wrap.leadingAnchor, constant: 10),
        line.trailingAnchor.constraint(equalTo: wrap.trailingAnchor),
        line.topAnchor.constraint(equalTo: wrap.topAnchor),
        line.bottomAnchor.constraint(equalTo: wrap.bottomAnchor),
        line.heightAnchor.constraint(equalToConstant: 0.5),
    ])
    return wrap
}

/// System Settings-style grouped card: rounded controlBackground fill, a
/// hairline border, and hairline separators between rows. Rows are inset 3pt so
/// the RowButton selection highlight sits inside the card edge.
func groupCard(_ rows: [NSView]) -> NSView {
    let card = NSView()
    card.wantsLayer = true
    card.layer?.cornerRadius = 10
    card.layer?.backgroundColor = NSColor.controlBackgroundColor.cgColor
    card.layer?.borderWidth = 0.5
    card.layer?.borderColor = NSColor.separatorColor.cgColor

    let inner = NSStackView()
    inner.orientation = .vertical
    inner.alignment = .leading
    inner.spacing = 0
    inner.edgeInsets = NSEdgeInsets(top: 3, left: 3, bottom: 3, right: 3)
    for (index, row) in rows.enumerated() {
        if index > 0 { inner.addArrangedSubview(hairline()) }
        inner.addArrangedSubview(row)
        row.translatesAutoresizingMaskIntoConstraints = false
        row.trailingAnchor.constraint(equalTo: inner.trailingAnchor, constant: -3).isActive = true
    }
    inner.translatesAutoresizingMaskIntoConstraints = false
    card.addSubview(inner)
    NSLayoutConstraint.activate([
        inner.leadingAnchor.constraint(equalTo: card.leadingAnchor),
        inner.trailingAnchor.constraint(equalTo: card.trailingAnchor),
        inner.topAnchor.constraint(equalTo: card.topAnchor),
        inner.bottomAnchor.constraint(equalTo: card.bottomAnchor),
    ])
    return card
}

/// Centered empty-state block: faded SF Symbol over a title and subtitle.
func emptyState(symbol: String, _ title: String, _ subtitle: String) -> NSView {
    let icon = NSImageView(image: symbolImage(symbol, 30) ?? NSImage())
    icon.contentTintColor = .tertiaryLabelColor
    let heading = makeLabel(title, size: 14, weight: .semibold)
    let sub = makeLabel(subtitle, size: 12, color: .secondaryLabelColor)
    let column = NSStackView(views: [icon, heading, sub])
    column.orientation = .vertical
    column.spacing = 6
    column.alignment = .centerX

    let wrap = NSView()
    wrap.addSubview(column)
    column.translatesAutoresizingMaskIntoConstraints = false
    NSLayoutConstraint.activate([
        column.centerXAnchor.constraint(equalTo: wrap.centerXAnchor),
        column.centerYAnchor.constraint(equalTo: wrap.centerYAnchor),
        column.topAnchor.constraint(greaterThanOrEqualTo: wrap.topAnchor, constant: 24),
        wrap.bottomAnchor.constraint(greaterThanOrEqualTo: column.bottomAnchor, constant: 24),
        wrap.heightAnchor.constraint(greaterThanOrEqualToConstant: 160),
    ])
    return wrap
}

/// Inline notice/banner row with an icon.
final class NoticeView: NSView {
    let label: NSTextField

    init(_ text: String, icon: String = Symbols.info, color: NSColor = .secondaryLabelColor,
         danger: Bool = false) {
        label = makeLabel(text, size: 12, color: danger ? .systemRed : .labelColor)
        super.init(frame: .zero)
        wantsLayer = true
        layer?.cornerRadius = 8
        layer?.backgroundColor = danger
            ? NSColor.systemRed.withAlphaComponent(0.1).cgColor
            : NSColor.quaternaryLabelColor.withAlphaComponent(0.2).cgColor
        let iconView = NSImageView(image: symbolImage(icon, 12) ?? NSImage())
        iconView.contentTintColor = color
        label.maximumNumberOfLines = 0
        label.lineBreakMode = .byWordWrapping
        let stack = NSStackView(views: [iconView, label])
        stack.spacing = 7
        stack.alignment = .firstBaseline
        stack.translatesAutoresizingMaskIntoConstraints = false
        addSubview(stack)
        NSLayoutConstraint.activate([
            stack.leadingAnchor.constraint(equalTo: leadingAnchor, constant: 10),
            stack.trailingAnchor.constraint(equalTo: trailingAnchor, constant: -10),
            stack.topAnchor.constraint(equalTo: topAnchor, constant: 8),
            stack.bottomAnchor.constraint(equalTo: bottomAnchor, constant: -8),
        ])
    }

    required init?(coder: NSCoder) { fatalError() }
}
