import AppKit

/// Center column: workspace detail (meta, daemon actions, services, URLs) or the
/// shared-services pane when that segment is selected.
final class BoardViewController: NSViewController {

    let desk: DeskController
    /// The stack being populated — always detached from the window during
    /// `rebuild`, then swapped in as the scroll document in one pass.
    private var stack = NSStackView()
    private let scrollView = NSScrollView()
    private let progress = NSProgressIndicator()

    private(set) var snapshot: DeskController.Snapshot?
    private var applyScheduled = false
    /// Skip a rebuild when nothing visible changed — the 2s poll emits a
    /// snapshot every tick even when the board is identical.
    private var lastSignature = ""

    init(desk: DeskController) {
        self.desk = desk
        super.init(nibName: nil, bundle: nil)
    }

    required init?(coder: NSCoder) { fatalError() }

    override func loadView() {
        scrollView.drawsBackground = false
        scrollView.hasVerticalScroller = true
        scrollView.scrollerStyle = .overlay
        // Let the scroll view inset its content below the unified toolbar —
        // with .fullSizeContentView the titlebar overlays the column's top.
        scrollView.automaticallyAdjustsContentInsets = true
        view = scrollView

        progress.style = .bar
        progress.isIndeterminate = true
        progress.controlSize = .small
        progress.isDisplayedWhenStopped = false
        progress.translatesAutoresizingMaskIntoConstraints = false
        view.addSubview(progress)
        NSLayoutConstraint.activate([
            progress.leadingAnchor.constraint(equalTo: view.leadingAnchor),
            progress.trailingAnchor.constraint(equalTo: view.trailingAnchor),
            progress.topAnchor.constraint(equalTo: view.safeAreaLayoutGuide.topAnchor),
        ])
    }

    final class FlippedView: NSView {
        override var isFlipped: Bool { true }
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

    func rebuild() {
        guard let snapshot else { return }
        snapshot.busy > 0 ? progress.startAnimation(nil) : progress.stopAnimation(nil)

        let signature = Self.signature(snapshot)
        guard signature != lastSignature else { return }
        lastSignature = signature

        // Build detached: adding a subview to a live window runs the layout
        // engine per insertion — quadratic for a whole board. Swap in once.
        stack = NSStackView()
        stack.orientation = .vertical
        stack.alignment = .leading
        stack.spacing = 10
        stack.edgeInsets = NSEdgeInsets(top: 20, left: 20, bottom: 24, right: 20)
        stack.translatesAutoresizingMaskIntoConstraints = false

        if snapshot.pane == .shared {
            buildShared(snapshot)
        } else {
            buildWorkspace(snapshot)
        }

        // Flipped document keeps short content pinned at the top.
        let document = FlippedView()
        // Without this the autoresizing mask generates a fixed width==0/height==0
        // constraint that fights the stack's pins on every display cycle.
        document.translatesAutoresizingMaskIntoConstraints = false
        document.addSubview(stack)
        NSLayoutConstraint.activate([
            stack.leadingAnchor.constraint(equalTo: document.leadingAnchor),
            stack.trailingAnchor.constraint(equalTo: document.trailingAnchor),
            stack.topAnchor.constraint(equalTo: document.topAnchor),
            stack.bottomAnchor.constraint(equalTo: document.bottomAnchor),
        ])
        scrollView.documentView = document
        // Only legal once the document is inside the scroll view's hierarchy.
        stack.widthAnchor.constraint(equalTo: scrollView.widthAnchor).isActive = true

        // Crossfade the swap; the whole point of dedup+rebuild is that this
        // only runs when the board actually changed.
        if !prefersReducedMotion() {
            document.alphaValue = 0
            NSAnimationContext.runAnimationGroup { context in
                context.duration = 0.16
                context.timingFunction = CAMediaTimingFunction(name: .easeOut)
                document.animator().alphaValue = 1
            }
        }
    }

    /// `fill` stretches the row to the column width (cards, rows, banners);
    /// buttons and labels keep their intrinsic size. Never constrain the
    /// leading edge — the stack view already pins it at the 20pt inset, so a
    /// second leading constraint fights the engine every display cycle.
    static func signature(_ s: DeskController.Snapshot) -> String {
        var parts: [String] = [
            s.pane.rawValue,
            s.selected?.id ?? "-",
            s.notice ?? "-",
            s.loadError ?? "-",
            s.pendingKind ?? "-",
            s.pendingId ?? "-",
            s.summary,
            s.selectedService,
            s.selectedLine.map { "\($0.id):\($0.state):\($0.disabled)" } ?? "-",
            s.smpLive ? "1" : "0",
            "sel:\(s.selected?.trusted ?? false):\(s.selected?.missing ?? false):\(s.selected?.stopped ?? false):\(s.selected?.hasToken ?? false)",
        ]
        for url in s.urls { parts.append("u:\(url.url):\(url.label)") }
        for section in s.sections {
            parts.append("s:\(section.name ?? "-")")
            for line in section.services {
                parts.append("l:\(line.id):\(line.state):\(line.display):\(line.ports):\(line.disabled):\(line.finite):\(line.infra):\(line.shared):\(line.error ?? "-")")
            }
        }
        for recipe in s.recipes { parts.append("r:\(recipe.id)") }
        for instance in s.instances {
            parts.append("i:\(instance.id):\(instance.installState):\(instance.display):\(instance.state):\(instance.attachments):\(String(describing: instance.port))")
        }
        return parts.joined(separator: "|")
    }

    func stackAdd(_ view: NSView, fill: Bool = true) {
        stack.addArrangedSubview(view)
        view.translatesAutoresizingMaskIntoConstraints = false
        if fill {
            view.trailingAnchor.constraint(equalTo: stack.trailingAnchor, constant: -20).isActive = true
        }
    }

    // MARK: - workspace pane

    private func buildWorkspace(_ s: DeskController.Snapshot) {
        guard let selected = s.selected else {
            stackAdd(emptyState(
                symbol: Symbols.flame,
                "Select a workspace",
                "Add a project folder from the sidebar, or pick an existing one."))
            return
        }

        let titleRow = NSStackView()
        titleRow.orientation = .horizontal
        titleRow.alignment = .centerY
        titleRow.distribution = .gravityAreas
        titleRow.addArrangedSubview(makeLabel(selected.name, size: 20, weight: .semibold))
        stackAdd(titleRow)
        if !s.summary.isEmpty {
            stackAdd(makeLabel(s.summary, size: 12, color: .secondaryLabelColor, mono: true))
        }
        if let notice = s.notice {
            stackAdd(NoticeView(notice))
        }

        stackAdd(metaCard(selected))

        // Daemon lifecycle actions
        let actions = NSStackView()
        actions.spacing = 6
        actions.orientation = .horizontal
        if !selected.trusted && !selected.missing {
            let armed = s.pendingKind == "trust" && s.pendingId == selected.id
            actions.addArrangedSubview(CallbackButton(
                title: armed ? "Confirm trust" : "Trust",
                symbol: Symbols.shield, kind: armed ? .armed : .primary,
                onClick: { [weak self] in self?.desk.trust() }))
        }
        if selected.trusted && !selected.missing && (selected.stopped || !selected.hasToken) {
            actions.addArrangedSubview(CallbackButton(
                title: "Start", symbol: Symbols.play, kind: .primary,
                onClick: { [weak self] in self?.desk.startDaemon() }))
        }
        if selected.trusted && !selected.missing && !selected.stopped {
            let restartArmed = s.pendingKind == "restart-daemon" && s.pendingId == selected.id
            actions.addArrangedSubview(CallbackButton(
                title: restartArmed ? "Confirm restart" : "Restart daemon",
                symbol: Symbols.restart, kind: restartArmed ? .armed : .normal,
                onClick: { [weak self] in self?.desk.restartDaemon() }))
            let stopArmed = s.pendingKind == "stop" && s.pendingId == selected.id
            actions.addArrangedSubview(CallbackButton(
                title: stopArmed ? "Confirm stop" : "Stop daemon",
                symbol: Symbols.stop, kind: stopArmed ? .armedDanger : .normal,
                onClick: { [weak self] in self?.desk.stopDaemon() }))
        }
        let forgetArmed = s.pendingKind == "forget" && s.pendingId == selected.id
        actions.addArrangedSubview(CallbackButton(
            title: forgetArmed ? "Confirm forget" : "Forget",
            symbol: Symbols.trash, kind: forgetArmed ? .armedDanger : .normal,
            onClick: { [weak self] in self?.desk.forget() }))
        stackAdd(actions)

        guard selected.hasToken, !selected.stopped else {
            stackAdd(binaryFooter(s))
            return
        }

        // Bulk actions
        let bulk = NSStackView()
        bulk.spacing = 6
        bulk.orientation = .horizontal
        bulk.addArrangedSubview(CallbackButton(
            title: "Start all", symbol: Symbols.play,
            onClick: { [weak self] in self?.desk.startAll() }))
        let stopAllArmed = s.pendingKind == "stop-all"
        bulk.addArrangedSubview(CallbackButton(
            title: stopAllArmed ? "Confirm stop all" : "Stop all",
            symbol: Symbols.stop, kind: stopAllArmed ? .armedDanger : .normal,
            onClick: { [weak self] in self?.desk.stopAll() }))
        stackAdd(bulk)

        stackAdd(sectionHeader("Log"))
        stackAdd(groupCard([serviceRow(
            id: "$daemon", name: "daemon log", icon: Symbols.terminal,
            chips: [], ports: "", state: "log", selected: s.selectedService == "$daemon",
            error: nil)]))

        let hasGroups = s.sections.contains { $0.name != nil }
        for section in s.sections {
            if let name = section.name {
                stackAdd(groupHeader(name, snapshot: s))
            } else if hasGroups {
                stackAdd(sectionHeader("Other"))
            }
            let rows = section.services.map { service in
                serviceRow(
                    id: service.id, name: service.label, icon: nil,
                    chips: chipsFor(service), ports: service.ports, state: service.display,
                    stateColor: stateColor(service.state),
                    selected: s.selectedService == service.id, error: service.error)
            }
            if !rows.isEmpty {
                stackAdd(groupCard(rows))
            }
        }
        if s.sections.flatMap(\.services).isEmpty {
            stackAdd(makeLabel("No services in this catalog.", size: 12, color: .secondaryLabelColor))
        }

        if let line = s.selectedLine {
            if line.disabled {
                stackAdd(makeLabel("This service is disabled.", size: 12, color: .secondaryLabelColor))
            } else {
                stackAdd(serviceActions(line, snapshot: s))
            }
        }

        if !s.urls.isEmpty {
            stackAdd(sectionHeader("URLs"))
            stackAdd(groupCard(s.urls.map { urlRow($0) }))
        }

        stackAdd(binaryFooter(s))
    }

    private func metaCard(_ row: DeskController.WorkspaceRow) -> NSView {
        let card = NSView()
        card.wantsLayer = true
        card.layer?.cornerRadius = 10
        card.layer?.backgroundColor = NSColor.controlBackgroundColor.cgColor
        card.layer?.borderWidth = 0.5
        card.layer?.borderColor = NSColor.separatorColor.cgColor

        let daemon: String = {
            if row.missing { return "missing folder" }
            if row.stopped { return "stopped" }
            return row.hasToken ? "attached" : "not attached"
        }()
        let pairs: [(String, String)] = [
            ("Path", row.fullPath),
            ("Trust", row.trusted ? "trusted" : "untrusted"),
            ("Daemon", daemon),
        ]
        let grid = NSGridView(views: pairs.map { pair -> [NSView] in
            [makeLabel(pair.0, size: 12, color: .secondaryLabelColor),
             makeLabel(pair.1, size: 12)]
        })
        grid.column(at: 0).xPlacement = .trailing
        grid.rowSpacing = 4
        grid.columnSpacing = 14
        grid.translatesAutoresizingMaskIntoConstraints = false
        card.addSubview(grid)
        NSLayoutConstraint.activate([
            grid.leadingAnchor.constraint(equalTo: card.leadingAnchor, constant: 12),
            grid.trailingAnchor.constraint(lessThanOrEqualTo: card.trailingAnchor, constant: -12),
            grid.topAnchor.constraint(equalTo: card.topAnchor, constant: 10),
            grid.bottomAnchor.constraint(equalTo: card.bottomAnchor, constant: -10),
        ])
        return card
    }

    private func sectionHeader(_ title: String) -> NSView {
        let label = makeLabel(title.uppercased(), size: 10, weight: .bold, color: .secondaryLabelColor)
        let wrap = NSView()
        wrap.addSubview(label)
        label.translatesAutoresizingMaskIntoConstraints = false
        NSLayoutConstraint.activate([
            label.leadingAnchor.constraint(equalTo: wrap.leadingAnchor),
            label.topAnchor.constraint(equalTo: wrap.topAnchor, constant: 10),
            label.bottomAnchor.constraint(equalTo: wrap.bottomAnchor),
        ])
        return wrap
    }

    private func groupHeader(_ name: String, snapshot s: DeskController.Snapshot) -> NSView {
        let header = sectionHeader(name)
        let buttons = NSStackView()
        buttons.spacing = 6
        buttons.orientation = .horizontal
        if ServiceBoard.groupIsUp(s.sections, name: name) {
            let armed = s.pendingKind == "restart-group" && s.pendingId == name
            buttons.addArrangedSubview(CallbackButton(
                title: armed ? "Confirm restart" : "Restart group",
                symbol: Symbols.restart, kind: armed ? .armed : .normal,
                onClick: { [weak self] in self?.desk.restartGroup(name) }))
        } else {
            buttons.addArrangedSubview(CallbackButton(
                title: "Start group", symbol: Symbols.play,
                onClick: { [weak self] in self?.desk.startGroup(name) }))
        }
        let stopArmed = s.pendingKind == "stop-group" && s.pendingId == name
        buttons.addArrangedSubview(CallbackButton(
            title: stopArmed ? "Confirm stop" : "Stop group",
            symbol: Symbols.stop, kind: stopArmed ? .armedDanger : .normal,
            onClick: { [weak self] in self?.desk.stopGroup(name) }))
        header.addSubview(buttons)
        buttons.translatesAutoresizingMaskIntoConstraints = false
        NSLayoutConstraint.activate([
            buttons.leadingAnchor.constraint(greaterThanOrEqualTo: header.subviews[0].trailingAnchor, constant: 12),
            buttons.trailingAnchor.constraint(equalTo: header.trailingAnchor),
            buttons.centerYAnchor.constraint(equalTo: header.centerYAnchor),
        ])
        header.setContentHuggingPriority(.defaultLow, for: .horizontal)
        return header
    }

    private func chipsFor(_ service: ServiceBoard.ServiceLine) -> [(String, NSColor)] {
        var chips: [(String, NSColor)] = []
        if service.shared { chips.append(("shared", .systemPurple)) }
        if service.infra { chips.append(("infra", .secondaryLabelColor)) }
        if service.disabled { chips.append(("disabled", .secondaryLabelColor)) }
        if service.finite { chips.append(("job", .secondaryLabelColor)) }
        return chips
    }

    private func serviceRow(id: String, name: String, icon: String?,
                            chips: [(String, NSColor)], ports: String, state: String,
                            stateColor: NSColor = .secondaryLabelColor,
                            selected: Bool, error: String?) -> NSView {
        let button = RowButton()
        button.selected = selected
        button.layer?.cornerRadius = 8
        if selected {
            button.layer?.borderWidth = 1
            button.layer?.borderColor = NSColor.controlAccentColor.withAlphaComponent(0.3).cgColor
        }

        var topViews: [NSView] = []
        if let icon {
            let iconView = NSImageView(image: symbolImage(icon, 13) ?? NSImage())
            iconView.contentTintColor = .secondaryLabelColor
            topViews.append(iconView)
        }
        topViews.append(makeLabel(name, size: 13, weight: .semibold))
        if !chips.isEmpty {
            let chipRow = NSStackView(views: chips.map { ChipView($0.0, color: $0.1) })
            chipRow.spacing = 4
            topViews.append(chipRow)
        }
        let top = NSStackView(views: topViews)
        top.spacing = 6
        top.alignment = .centerY

        var columnViews: [NSView] = [top]
        if !ports.isEmpty {
            columnViews.append(makeLabel(ports, size: 11, color: .secondaryLabelColor, mono: true))
        }
        if let error {
            let err = makeLabel(error, size: 11, color: .systemRed)
            err.maximumNumberOfLines = 0
            err.lineBreakMode = .byWordWrapping
            columnViews.append(err)
        }
        let column = NSStackView(views: columnViews)
        column.orientation = .vertical
        column.alignment = .leading
        column.spacing = 2

        let pill = PillView(state, color: stateColor)
        let row = NSStackView(views: [column, pill])
        row.alignment = .centerY
        row.distribution = .gravityAreas
        row.edgeInsets = NSEdgeInsets(top: 7, left: 10, bottom: 7, right: 10)
        row.translatesAutoresizingMaskIntoConstraints = false
        column.setContentHuggingPriority(.defaultLow, for: .horizontal)
        pill.setContentHuggingPriority(.required, for: .horizontal)

        button.addSubview(row)
        NSLayoutConstraint.activate([
            row.leadingAnchor.constraint(equalTo: button.leadingAnchor),
            row.trailingAnchor.constraint(equalTo: button.trailingAnchor),
            row.topAnchor.constraint(equalTo: button.topAnchor),
            row.bottomAnchor.constraint(equalTo: button.bottomAnchor),
        ])
        button.onClick = { [weak self] in self?.desk.selectService(id) }
        return button
    }

    private func serviceActions(_ line: ServiceBoard.ServiceLine, snapshot s: DeskController.Snapshot) -> NSView {
        let row = NSStackView()
        row.spacing = 6
        row.orientation = .horizontal
        if ServiceBoard.showsStop(line.state) {
            let armed = s.pendingKind == "project-stop" && s.pendingId == line.id
            row.addArrangedSubview(CallbackButton(
                title: armed ? "Confirm stop" : "Stop",
                symbol: Symbols.stop, kind: armed ? .armedDanger : .normal,
                onClick: { [weak self] in self?.desk.stopService(line.id) }))
        } else {
            row.addArrangedSubview(CallbackButton(
                title: "Start", symbol: Symbols.play, kind: .primary,
                onClick: { [weak self] in self?.desk.startService(line.id) }))
        }
        let restartArmed = s.pendingKind == "project-restart" && s.pendingId == line.id
        row.addArrangedSubview(CallbackButton(
            title: restartArmed ? "Confirm restart" : "Restart",
            symbol: Symbols.restart, kind: restartArmed ? .armed : .normal,
            onClick: { [weak self] in self?.desk.restartService(line.id) }))
        if line.state == "externally-owned" {
            let armed = s.pendingKind == "kill" && s.pendingId == line.id
            row.addArrangedSubview(CallbackButton(
                title: armed ? "Confirm reclaim" : "Reclaim port",
                symbol: Symbols.warning, kind: armed ? .armedDanger : .normal,
                onClick: { [weak self] in self?.desk.reclaimPort(line.id) }))
        }
        return row
    }

    private func urlRow(_ url: ServiceBoard.VisibleURL) -> NSView {
        let link = CallbackButton(title: url.label, symbol: Symbols.external, kind: .normal) {
            NSWorkspace.shared.open(URL(string: url.url) ?? URL(fileURLWithPath: "/"))
        }
        link.bezelStyle = .inline
        link.isBordered = false
        link.contentTintColor = .controlAccentColor
        let sub = makeLabel(url.url, size: 11, color: .secondaryLabelColor, mono: true)
        let row = NSStackView(views: [link, sub])
        row.spacing = 8
        row.alignment = .firstBaseline
        // Match service-row padding so URLs sit evenly inside the group card.
        row.edgeInsets = NSEdgeInsets(top: 7, left: 10, bottom: 7, right: 10)
        return row
    }

    private func binaryFooter(_ s: DeskController.Snapshot) -> NSView {
        let wrap = NSView()
        let text = makeLabel(
            "\(s.binaryLine)\nexecutable: \(s.binaryExists ? "yes" : "no")",
            size: 10, color: .secondaryLabelColor, mono: true)
        text.maximumNumberOfLines = 0
        text.lineBreakMode = .byWordWrapping
        wrap.addSubview(text)
        text.translatesAutoresizingMaskIntoConstraints = false
        NSLayoutConstraint.activate([
            text.leadingAnchor.constraint(equalTo: wrap.leadingAnchor),
            text.trailingAnchor.constraint(lessThanOrEqualTo: wrap.trailingAnchor),
            text.topAnchor.constraint(equalTo: wrap.topAnchor, constant: 18),
            text.bottomAnchor.constraint(equalTo: wrap.bottomAnchor),
        ])
        let line = NSView()
        line.wantsLayer = true
        line.layer?.backgroundColor = NSColor.separatorColor.cgColor
        line.translatesAutoresizingMaskIntoConstraints = false
        wrap.addSubview(line)
        NSLayoutConstraint.activate([
            line.leadingAnchor.constraint(equalTo: wrap.leadingAnchor),
            line.trailingAnchor.constraint(equalTo: wrap.trailingAnchor),
            line.topAnchor.constraint(equalTo: wrap.topAnchor, constant: 10),
            line.heightAnchor.constraint(equalToConstant: 0.5),
        ])
        return wrap
    }
}
