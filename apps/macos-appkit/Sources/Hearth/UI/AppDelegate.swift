import AppKit

final class AppDelegate: NSObject, NSApplicationDelegate, NSToolbarDelegate {

    let desk = DeskController()
    private var window: NSWindow!
    private var sidebar: SidebarViewController!
    private var board: BoardViewController!
    private var logPane: LogColumnController!
    private var timer: Timer?

    private let sidebarToggleID = NSToolbarItem.Identifier("sidebarToggle")
    private let refreshID = NSToolbarItem.Identifier("refresh")
    private let logToggleID = NSToolbarItem.Identifier("logToggle")

    func applicationDidFinishLaunching(_ notification: Notification) {
        sidebar = SidebarViewController(desk: desk)
        board = BoardViewController(desk: desk)
        logPane = LogColumnController(desk: desk)

        split = NSSplitViewController()
        split.splitView.isVertical = true
        split.splitView.dividerStyle = .thin

        let sidebarItem = NSSplitViewItem(sidebarWithViewController: sidebar)
        sidebarItem.minimumThickness = 220
        sidebarItem.maximumThickness = 320
        sidebarItem.preferredThicknessFraction = 0.22
        // Collapsing the sidebar keeps the window fixed; siblings absorb the
        // width — same contract as the log column.
        sidebarItem.canCollapse = true
        sidebarItem.collapseBehavior = .preferResizingSiblingsWithFixedSplitView
        self.sidebarItem = sidebarItem

        let boardItem = NSSplitViewItem(viewController: board)
        boardItem.minimumThickness = 400

        let logItem = NSSplitViewItem(viewController: logPane)
        logItem.minimumThickness = 240
        // No maximumThickness: any thickness max on a split item becomes a
        // window resize constraint and caps the whole window's width.
        // preferResizingSiblingsWithFixedSplitView keeps the split view (and so
        // the window) the same size while collapse/expand animates; the other
        // behaviors resize the window.
        logItem.canCollapse = true
        logItem.collapseBehavior = .preferResizingSiblingsWithFixedSplitView
        logItem.isCollapsed = true
        self.logItem = logItem

        split.addSplitViewItem(sidebarItem)
        split.addSplitViewItem(boardItem)
        split.addSplitViewItem(logItem)

        window = NSWindow(
            contentRect: NSRect(x: 0, y: 0, width: 1180, height: 720),
            styleMask: [.titled, .closable, .miniaturizable, .resizable, .fullSizeContentView],
            backing: .buffered,
            defer: false
        )
        window.titlebarAppearsTransparent = true
        window.title = "Hearth"
        window.minSize = NSSize(width: 880, height: 480)

        let toolbar = NSToolbar(identifier: "HearthMain")
        toolbar.delegate = self
        toolbar.displayMode = .iconOnly
        window.toolbar = toolbar
        window.toolbarStyle = .unifiedCompact
        window.contentViewController = split
        window.center()
        installMainMenu()
        window.makeKeyAndOrderFront(nil)

        desk.onChange = { [weak self] snapshot in
            self?.apply(snapshot)
        }
        desk.start()

        timer = Timer.scheduledTimer(withTimeInterval: 2, repeats: true) { [weak self] _ in
            self?.desk.tick()
        }
        // The split view's initial layout pass shrinks the window to its
        // fitting width; so does the first collapse animation. Resize after
        // both settle.
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.6) { [weak self] in
            self?.window.setContentSize(NSSize(width: 1180, height: 720))
            self?.window.center()
        }
    }

    private var logItem: NSSplitViewItem?
    private var sidebarItem: NSSplitViewItem?
    private var lastExpandedLog: Bool?
    private var split: NSSplitViewController!
    private var didInitialSize = false

    private func apply(_ snapshot: DeskController.Snapshot) {
        window.title = snapshot.pane == .shared
            ? "Shared"
            : (snapshot.selected?.name ?? "Hearth")
        // The launch-time layout + collapse pass can leave the window narrower
        // than intended; settle it once the first real snapshot arrives.
        if !didInitialSize {
            didInitialSize = true
            let size = window.contentLayoutRect.size
            if size.width < 1180 || size.height < 720 {
                window.setContentSize(NSSize(width: 1180, height: 720))
            }
        }
        sidebar.apply(snapshot)
        board.apply(snapshot)
        logPane.apply(snapshot)

        // The log column expands only while a workspace daemon is attached.
        // Collapse via the split item's animator gives a native slide with the
        // siblings absorbing the freed width (fixedSplitView collapse behavior).
        let showLog = snapshot.pane == .workspaces
            && snapshot.selected?.hasToken == true
            && snapshot.selected?.stopped == false
        let expanded = showLog && snapshot.logOpen
        if expanded != lastExpandedLog {
            lastExpandedLog = expanded
            if prefersReducedMotion() {
                logItem?.isCollapsed = !expanded
            } else {
                logItem?.animator().isCollapsed = !expanded
            }
        }
    }

    // MARK: - menu bar

    @objc private func refreshNow() {
        desk.refreshList()
        desk.refreshShared()
    }

    @objc private func toggleLogColumn() {
        desk.toggleLog()
    }

    private func installMainMenu() {
        let main = NSMenu()

        let appItem = NSMenuItem()
        main.addItem(appItem)
        let appMenu = NSMenu()
        appItem.submenu = appMenu
        appMenu.addItem(withTitle: "About Hearth",
                        action: #selector(NSApplication.orderFrontStandardAboutPanel(_:)),
                        keyEquivalent: "")
        appMenu.addItem(.separator())
        appMenu.addItem(withTitle: "Hide Hearth",
                        action: #selector(NSApplication.hide(_:)), keyEquivalent: "h")
        let hideOthers = NSMenuItem(title: "Hide Others",
                                    action: #selector(NSApplication.hideOtherApplications(_:)),
                                    keyEquivalent: "h")
        hideOthers.keyEquivalentModifierMask = [.command, .option]
        appMenu.addItem(hideOthers)
        appMenu.addItem(withTitle: "Show All",
                        action: #selector(NSApplication.unhideAllApplications(_:)),
                        keyEquivalent: "")
        appMenu.addItem(.separator())
        appMenu.addItem(withTitle: "Quit Hearth",
                        action: #selector(NSApplication.terminate(_:)), keyEquivalent: "q")

        // Standard edit menu — without it, Cmd+X/C/V and friends do nothing in
        // the sidebar's path field or the log text view.
        let editItem = NSMenuItem()
        main.addItem(editItem)
        let editMenu = NSMenu(title: "Edit")
        editItem.submenu = editMenu
        editMenu.addItem(withTitle: "Undo", action: Selector(("undo:")), keyEquivalent: "z")
        let redo = NSMenuItem(title: "Redo", action: Selector(("redo:")), keyEquivalent: "z")
        redo.keyEquivalentModifierMask = [.command, .shift]
        editMenu.addItem(redo)
        editMenu.addItem(.separator())
        editMenu.addItem(withTitle: "Cut", action: #selector(NSText.cut(_:)), keyEquivalent: "x")
        editMenu.addItem(withTitle: "Copy", action: #selector(NSText.copy(_:)), keyEquivalent: "c")
        editMenu.addItem(withTitle: "Paste", action: #selector(NSText.paste(_:)), keyEquivalent: "v")
        editMenu.addItem(withTitle: "Select All", action: #selector(NSText.selectAll(_:)), keyEquivalent: "a")

        let viewItem = NSMenuItem()
        main.addItem(viewItem)
        let viewMenu = NSMenu(title: "View")
        viewItem.submenu = viewMenu
        let refresh = NSMenuItem(title: "Refresh", action: #selector(refreshNow), keyEquivalent: "r")
        refresh.target = self
        viewMenu.addItem(refresh)
        let sidebarToggle = NSMenuItem(title: "Toggle Sidebar",
                                       action: #selector(toggleSidebar), keyEquivalent: "s")
        sidebarToggle.keyEquivalentModifierMask = [.command, .control]
        sidebarToggle.target = self
        viewMenu.addItem(sidebarToggle)
        let logToggle = NSMenuItem(title: "Toggle Log Column",
                                 action: #selector(toggleLogColumn), keyEquivalent: "l")
        logToggle.keyEquivalentModifierMask = [.command, .option]
        logToggle.target = self
        viewMenu.addItem(logToggle)

        let windowItem = NSMenuItem()
        main.addItem(windowItem)
        let windowMenu = NSMenu(title: "Window")
        windowItem.submenu = windowMenu
        windowMenu.addItem(withTitle: "Minimize",
                           action: #selector(NSWindow.performMiniaturize(_:)), keyEquivalent: "m")
        windowMenu.addItem(withTitle: "Zoom",
                           action: #selector(NSWindow.performZoom(_:)), keyEquivalent: "")
        windowMenu.addItem(.separator())
        windowMenu.addItem(withTitle: "Close Window",
                           action: #selector(NSWindow.performClose(_:)), keyEquivalent: "w")

        NSApplication.shared.mainMenu = main
        NSApplication.shared.windowsMenu = windowMenu
    }

    @objc private func toggleSidebar() {
        guard let sidebarItem else { return }
        if prefersReducedMotion() {
            sidebarItem.isCollapsed.toggle()
        } else {
            sidebarItem.animator().isCollapsed.toggle()
        }
    }

    // MARK: - NSToolbarDelegate

    func toolbarAllowedItemIdentifiers(_ toolbar: NSToolbar) -> [NSToolbarItem.Identifier] {
        [sidebarToggleID, .sidebarTrackingSeparator, .flexibleSpace, .space, refreshID, logToggleID]
    }

    func toolbarDefaultItemIdentifiers(_ toolbar: NSToolbar) -> [NSToolbarItem.Identifier] {
        [sidebarToggleID, .sidebarTrackingSeparator, .flexibleSpace, refreshID, logToggleID]
    }

    func toolbar(_ toolbar: NSToolbar,
                 itemForItemIdentifier itemIdentifier: NSToolbarItem.Identifier,
                 willBeInsertedIntoToolbar flag: Bool) -> NSToolbarItem? {
        if itemIdentifier == .sidebarTrackingSeparator {
            return NSTrackingSeparatorToolbarItem(
                identifier: itemIdentifier, splitView: split.splitView, dividerIndex: 0)
        }
        let item = NSToolbarItem(itemIdentifier: itemIdentifier)
        item.isBordered = true
        item.target = self
        switch itemIdentifier {
        case sidebarToggleID:
            item.label = "Sidebar"
            item.paletteLabel = "Toggle Sidebar"
            item.toolTip = "Show or hide the workspace sidebar"
            item.image = symbolImage(Symbols.sidebarLeft, 15)
            item.action = #selector(toggleSidebar)
        case refreshID:
            item.label = "Refresh"
            item.paletteLabel = "Refresh"
            item.toolTip = "Reload workspaces and services"
            item.image = symbolImage(Symbols.restart, 15)
            item.action = #selector(refreshNow)
        case logToggleID:
            item.label = "Log"
            item.paletteLabel = "Toggle Log Column"
            item.toolTip = "Show or hide the log column"
            item.image = symbolImage(Symbols.sidebarRight, 15)
            item.action = #selector(toggleLogColumn)
        default:
            return nil
        }
        return item
    }

    func applicationShouldTerminateAfterLastWindowClosed(_ sender: NSApplication) -> Bool {
        true
    }
}
