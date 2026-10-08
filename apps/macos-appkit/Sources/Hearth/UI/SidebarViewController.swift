import AppKit

/// Translucent Finder-style sidebar: brand, pane segments, add-folder, workspace list.
final class SidebarViewController: NSViewController {

    private let desk: DeskController
    private var listStack = NSStackView()
    private let listScroll = NSScrollView()
    private let segmented = NSSegmentedControl()
    private let field = NSTextField()
    private let refreshButton = NSButton()
    private let spinner = NSProgressIndicator()

    private var snapshot: DeskController.Snapshot?
    private var applyScheduled = false
    private var lastSignature = ""

    init(desk: DeskController) {
        self.desk = desk
        super.init(nibName: nil, bundle: nil)
    }

    required init?(coder: NSCoder) { fatalError() }

    override func loadView() {
        let effect = NSVisualEffectView()
        effect.material = .sidebar
        effect.blendingMode = .behindWindow
        effect.state = .active
        view = effect

        // Segmented control: Workspaces / Shared
        segmented.segmentCount = 2
        segmented.setLabel("Workspaces", forSegment: 0)
        segmented.setLabel("Shared", forSegment: 1)
        segmented.segmentStyle = .automatic
        segmented.trackingMode = .selectOne
        segmented.selectedSegment = 0
        segmented.target = self
        segmented.action = #selector(paneChanged)
        segmented.controlSize = .regular

        // Add-folder row
        field.placeholderString = "/Users/me/project"
        field.font = .systemFont(ofSize: 12)
        field.controlSize = .regular
        field.delegate = self
        let add = NSButton()
        add.bezelStyle = .accessoryBarAction
        add.image = symbolImage(Symbols.folderPlus, 13)
        add.target = self
        add.action = #selector(addFolder)
        add.toolTip = "Add folder"
        let browse = NSButton()
        browse.bezelStyle = .accessoryBarAction
        browse.image = symbolImage("ellipsis.circle", 13)
        browse.target = self
        browse.action = #selector(chooseFolder)
        browse.toolTip = "Choose folder…"
        let addRow = NSStackView(views: [field, add, browse])
        addRow.spacing = 4

        // Section header + refresh
        let title = makeLabel("WORKSPACES", size: 10, weight: .bold, color: .secondaryLabelColor)
        refreshButton.isBordered = false
        refreshButton.image = symbolImage(Symbols.restart, 12)
        refreshButton.contentTintColor = .secondaryLabelColor
        refreshButton.target = self
        refreshButton.action = #selector(refresh)
        refreshButton.toolTip = "Refresh workspaces"
        spinner.style = .spinning
        spinner.controlSize = .mini
        spinner.isDisplayedWhenStopped = false
        let titleRow = NSStackView(views: [title, NSView(), refreshButton, spinner])
        titleRow.alignment = .centerY
        titleRow.edgeInsets = NSEdgeInsets(top: 0, left: 8, bottom: 0, right: 8)

        // Workspace list (document swapped per rebuild — see rebuild()).
        listScroll.drawsBackground = false
        listScroll.hasVerticalScroller = true
        listScroll.scrollerStyle = .overlay
        listScroll.automaticallyAdjustsContentInsets = false

        let column = NSStackView(views: [segmented, addRow, titleRow, listScroll])
        column.orientation = .vertical
        column.spacing = 10
        column.alignment = .leading
        column.translatesAutoresizingMaskIntoConstraints = false
        view.addSubview(column)

        NSLayoutConstraint.activate([
            column.leadingAnchor.constraint(equalTo: view.leadingAnchor, constant: 8),
            column.trailingAnchor.constraint(equalTo: view.trailingAnchor, constant: -8),
            // Clear the unified toolbar row — .fullSizeContentView puts our
            // content under it (traffic lights + sidebar toggle). The safe
            // area guide already accounts for the titlebar+toolbar height.
            column.topAnchor.constraint(equalTo: view.safeAreaLayoutGuide.topAnchor, constant: 4),
            column.bottomAnchor.constraint(equalTo: view.bottomAnchor, constant: -10),
            segmented.widthAnchor.constraint(equalTo: column.widthAnchor),
            addRow.widthAnchor.constraint(equalTo: column.widthAnchor),
            titleRow.widthAnchor.constraint(equalTo: column.widthAnchor),
            listScroll.widthAnchor.constraint(equalTo: column.widthAnchor),
            listScroll.heightAnchor.constraint(greaterThanOrEqualToConstant: 80),
            field.heightAnchor.constraint(equalToConstant: 22),
            add.widthAnchor.constraint(equalToConstant: 26),
            browse.widthAnchor.constraint(equalToConstant: 26),
        ])
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
        segmented.selectedSegment = snapshot.pane == .shared ? 1 : 0
        refreshButton.isEnabled = snapshot.busy == 0
        snapshot.busy > 0 ? spinner.startAnimation(nil) : spinner.stopAnimation(nil)

        var parts: [String] = [snapshot.pane.rawValue, snapshot.selected?.id ?? "-",
                               snapshot.loadError ?? "-", String(snapshot.rows.count)]
        for row in snapshot.rows {
            parts.append("\(row.id):\(row.trusted):\(row.missing):\(row.stopped):\(row.hasToken):\(row.path)")
        }
        let signature = parts.joined(separator: "|")
        guard signature != lastSignature else { return }
        lastSignature = signature

        // Build detached, then swap the document — see BoardViewController.
        listStack = NSStackView()
        listStack.orientation = .vertical
        listStack.spacing = 2
        listStack.alignment = .leading
        listStack.edgeInsets = NSEdgeInsets(top: 0, left: 8, bottom: 8, right: 8)
        listStack.translatesAutoresizingMaskIntoConstraints = false

        if let error = snapshot.loadError {
            listStack.addArrangedSubview(NoticeView(error, icon: Symbols.warning, danger: true))
        }
        if snapshot.rows.isEmpty {
            let empty = NSStackView(views: [
                makeLabel("No workspaces yet.", size: 12, color: .secondaryLabelColor),
            ])
            empty.edgeInsets = NSEdgeInsets(top: 4, left: 4, bottom: 4, right: 4)
            listStack.addArrangedSubview(empty)
        } else {
            for row in snapshot.rows {
                listStack.addArrangedSubview(workspaceRow(row))
            }
            for view in listStack.arrangedSubviews {
                // The stack pins leading at the 8pt inset; pull trailing to -8.
                view.trailingAnchor.constraint(equalTo: listStack.trailingAnchor, constant: -8).isActive = true
            }
        }
        listScroll.documentView = listStack
        // Only legal once the document is inside the scroll view's hierarchy.
        listStack.widthAnchor.constraint(equalTo: listScroll.widthAnchor).isActive = true
    }

    private func workspaceRow(_ row: DeskController.WorkspaceRow) -> NSView {
        let selected = snapshot?.selected?.id == row.id
        let button = RowButton()
        button.selected = selected

        let iconName = row.missing ? Symbols.warning : Symbols.folder
        let icon = NSImageView(image: symbolImage(iconName, 15) ?? NSImage())
        icon.contentTintColor = selected ? .controlAccentColor : .secondaryLabelColor

        let name = makeLabel(row.name, size: 13, weight: .semibold)
        let path = makeLabel(abbreviateTilde(row.path), size: 11, color: .secondaryLabelColor)

        let chips = NSStackView()
        chips.spacing = 4
        chips.orientation = .horizontal
        if row.missing {
            chips.addArrangedSubview(ChipView("missing", color: .systemRed))
        } else if row.trusted {
            chips.addArrangedSubview(ChipView("trusted"))
        } else {
            chips.addArrangedSubview(ChipView("untrusted", color: .systemOrange))
        }
        if row.stopped {
            chips.addArrangedSubview(ChipView("stopped"))
        }

        let text = NSStackView(views: [name, path, chips])
        text.orientation = .vertical
        text.alignment = .leading
        text.spacing = 1

        let content = NSStackView(views: [icon, text])
        content.spacing = 8
        content.alignment = .centerY
        content.edgeInsets = NSEdgeInsets(top: 6, left: 8, bottom: 6, right: 8)
        content.translatesAutoresizingMaskIntoConstraints = false

        button.addSubview(content)
        NSLayoutConstraint.activate([
            content.leadingAnchor.constraint(equalTo: button.leadingAnchor),
            content.trailingAnchor.constraint(equalTo: button.trailingAnchor),
            content.topAnchor.constraint(equalTo: button.topAnchor),
            content.bottomAnchor.constraint(equalTo: button.bottomAnchor),
        ])
        let id = row.id
        button.onClick = { [weak self] in self?.desk.select(id) }
        return button
    }

    @objc private func paneChanged() {
        desk.showPane(segmented.selectedSegment == 1 ? .shared : .workspaces)
    }

    @objc private func addFolder() {
        let input = field.stringValue
        guard !input.trimmingCharacters(in: .whitespaces).isEmpty else { return }
        field.stringValue = ""
        desk.addFolder(input)
    }

    @objc private func chooseFolder() {
        let panel = NSOpenPanel()
        panel.canChooseDirectories = true
        panel.canChooseFiles = false
        panel.allowsMultipleSelection = false
        panel.prompt = "Add workspace"
        panel.begin { [weak self] response in
            guard response == .OK, let url = panel.url else { return }
            self?.desk.addFolder(url.path)
        }
    }

    @objc private func refresh() {
        desk.refreshList()
    }
}

extension SidebarViewController: NSTextFieldDelegate {
    func control(_ control: NSControl, textView: NSTextView, doCommandBy command: Selector) -> Bool {
        if command == #selector(NSResponder.insertNewline(_:)) {
            addFolder()
            return true
        }
        return false
    }
}
