import AppKit
import SwiftUI
import XCTest

@testable import HearthApp

final class LogTextViewHostingTests: XCTestCase {
    private func findTextView(in view: NSView) -> NSTextView? {
        if let tv = view as? NSTextView { return tv }
        for sub in view.subviews {
            if let found = findTextView(in: sub) { return found }
        }
        return nil
    }

    @MainActor
    private func makeWorkspace(path: String = "/tmp/ws") -> Workspace {
        Workspace(path: path, trusted: true)
    }

    @MainActor
    func testPanelRendersFetchedText() async throws {
        let api = FakeManagerAPI()
        var served = 0
        api.logsHandler = { _, _ in
            served += 1
            return makeLogSlice(data: served == 1 ? "hello log\n" : "", nextCursor: 10, generation: 1)
        }
        let log = LogController(client: api, serviceId: "api")

        let panel = ServiceLogPanel(serviceLabel: "api", log: log)
        let hosting = NSHostingController(rootView: panel)
        let window = NSWindow(contentViewController: hosting)
        window.setContentSize(NSSize(width: 480, height: 320))
        window.orderFront(nil)
        defer { window.orderOut(nil) }

        await log.fetchOnce()
        for _ in 0 ..< 50 {
            try await Task.sleep(for: .milliseconds(20))
            if findTextView(in: hosting.view)?.string == "hello log\n" { break }
        }
        log.stop()
        let textView = try XCTUnwrap(findTextView(in: hosting.view))
        XCTAssertEqual(textView.string, "hello log\n")
        // The string being set is not enough — the text view must also be laid out at a
        // non-zero width or every line wraps into invisibility and the panel renders blank.
        XCTAssertGreaterThan(textView.frame.width, 0, "log text view has zero width — content is laid out but invisible")
    }

    /// Drives the real `WorkspaceDetailView` → `ServiceListView` → `logPanel` path: connect with a
    /// fake client, set `selectedServiceId` the way a `List` click does, and expect the fetched log
    /// to reach the embedded `NSTextView`.
    @MainActor
    func testSelectingAServiceRendersItsLog() async throws {
        let api = FakeManagerAPI()
        api.servicesHandler = { [makeService("api", actualState: "ready")] }
        api.catalogHandler = { ServiceCatalogSummary(services: [CatalogService(id: "api", label: "API", kind: nil, ownership: nil)]) }
        api.urlsHandler = { [] }
        api.logsHandler = { _, _ in makeLogSlice(data: "api log line\n", nextCursor: 13, generation: 1) }

        let workspace = makeWorkspace()
        let controller = WorkspaceController(
            workspace: workspace,
            watchesConfigFile: false,
            connector: { _ in api }
        )
        await controller.connect()

        let detail = WorkspaceDetailView(controller: controller, workspace: workspace)
            .environmentObject(WorkspaceStore())
        let hosting = NSHostingController(rootView: detail)
        let window = NSWindow(contentViewController: hosting)
        window.setContentSize(NSSize(width: 760, height: 480))
        window.orderFront(nil)
        defer { window.orderOut(nil) }

        controller.selectedServiceId = "api"

        var rendered = ""
        for _ in 0 ..< 100 {
            try await Task.sleep(for: .milliseconds(50))
            if let text = findTextView(in: hosting.view)?.string, text.contains("api log line") {
                rendered = text
                break
            }
        }
        XCTAssertTrue(rendered.contains("api log line"), "log text never reached the NSTextView; current: \(rendered.debugDescription)")
    }

    private func findTableView(in view: NSView) -> NSTableView? {
        if let tv = view as? NSTableView { return tv }
        for sub in view.subviews {
            if let found = findTableView(in: sub) { return found }
        }
        return nil
    }

    /// The remaining untested link: a real `NSTableView` row selection must write through the
    /// `List(selection:)` binding into `controller.selectedServiceId`.
    @MainActor
    func testClickingAListRowSetsSelectedServiceId() async throws {
        let api = FakeManagerAPI()
        api.servicesHandler = { [makeService("api", actualState: "ready")] }
        api.catalogHandler = { ServiceCatalogSummary(services: [CatalogService(id: "api", label: "API", kind: nil, ownership: nil)]) }
        api.urlsHandler = { [] }
        api.logsHandler = { _, _ in makeLogSlice(data: "api log line\n", nextCursor: 13, generation: 1) }

        let workspace = makeWorkspace()
        let controller = WorkspaceController(
            workspace: workspace,
            watchesConfigFile: false,
            connector: { _ in api }
        )
        await controller.connect()

        let detail = WorkspaceDetailView(controller: controller, workspace: workspace)
            .environmentObject(WorkspaceStore())
        let hosting = NSHostingController(rootView: detail)
        let window = NSWindow(contentViewController: hosting)
        window.setContentSize(NSSize(width: 760, height: 480))
        NSApp.activate(ignoringOtherApps: true)
        window.makeKeyAndOrderFront(nil)
        defer { window.orderOut(nil) }

        var table: NSTableView?
        for _ in 0 ..< 100 {
            try await Task.sleep(for: .milliseconds(50))
            if let found = findTableView(in: hosting.view), found.numberOfRows > 0 {
                table = found
                break
            }
        }
        guard let table else {
            XCTFail("service list never materialised an NSTableView")
            return
        }

        // Row layout: section headers and the pinned "daemon log" row occupy the first rows, so
        // locate the service row by content — it is the only row carrying action buttons.
        var serviceRow = -1
        for row in 0 ..< table.numberOfRows {
            guard let rowView = table.rowView(atRow: row, makeIfNecessary: true) else { continue }
            if containsButton(rowView) { serviceRow = row; break }
        }
        guard serviceRow >= 0 else {
            XCTFail("no row rendering a service (no row has action buttons)")
            return
        }
        let rowRect = table.rect(ofRow: serviceRow)
        let pointInWindow = table.convert(NSPoint(x: rowRect.midX, y: rowRect.midY), to: nil)
        let pointOnScreen = window.convertPoint(toScreen: pointInWindow)
        guard let down = NSEvent.mouseEvent(with: .leftMouseDown, location: pointOnScreen, modifierFlags: [], timestamp: 0, windowNumber: window.windowNumber, context: nil, eventNumber: 0, clickCount: 1, pressure: 1),
              let up = NSEvent.mouseEvent(with: .leftMouseUp, location: pointOnScreen, modifierFlags: [], timestamp: 0.05, windowNumber: window.windowNumber, context: nil, eventNumber: 0, clickCount: 1, pressure: 0)
        else {
            XCTFail("could not synthesise mouse events")
            return
        }
        window.sendEvent(down)
        window.sendEvent(up)

        for _ in 0 ..< 100 {
            try await Task.sleep(for: .milliseconds(50))
            if controller.selectedServiceId == "api" { break }
        }
        if controller.selectedServiceId != "api" {
            // Isolate whether the failure is click delivery or the binding itself: drive the same
            // delegate path programmatically.
            table.selectRowIndexes(IndexSet(integer: serviceRow), byExtendingSelection: false)
            try await Task.sleep(for: .milliseconds(100))
            XCTAssertEqual(controller.selectedServiceId, "api", "neither click nor programmatic selection reached the binding")
        }
        XCTAssertEqual(controller.selectedServiceId, "api")
    }

    /// True when `view`'s subtree contains a text field/label displaying `text` — used to locate a
    /// row by what it shows instead of a fragile fixed index.
    /// True when `view`'s subtree contains a button — service rows carry Start/Stop action
    /// buttons; the pinned daemon row and section headers do not.
    private func containsButton(_ view: NSView) -> Bool {
        if view is NSButton { return true }
        return view.subviews.contains { containsButton($0) }
    }

    /// The real app embeds `WorkspaceDetailView` in a `NavigationSplitView`'s `detail` slot —
    /// replicate that so the log panel is exercised inside the same split-view stack it runs in.
    @MainActor
    func testLogPanelInsideNavigationSplitViewDetail() async throws {
        let api = FakeManagerAPI()
        api.servicesHandler = { [makeService("api", actualState: "ready")] }
        api.catalogHandler = { ServiceCatalogSummary(services: [CatalogService(id: "api", label: "API", kind: nil, ownership: nil)]) }
        api.urlsHandler = { [] }
        api.logsHandler = { _, _ in makeLogSlice(data: "api log line\n", nextCursor: 13, generation: 1) }

        let workspace = makeWorkspace()
        let controller = WorkspaceController(
            workspace: workspace,
            watchesConfigFile: false,
            connector: { _ in api }
        )
        await controller.connect()

        struct Harness: View {
            let controller: WorkspaceController
            let workspace: Workspace
            var body: some View {
                NavigationSplitView {
                    List { Text("workspace") }
                } detail: {
                    WorkspaceDetailView(controller: controller, workspace: workspace)
                }
            }
        }

        let hosting = NSHostingController(rootView: Harness(controller: controller, workspace: workspace).environmentObject(WorkspaceStore()))
        let window = NSWindow(contentViewController: hosting)
        window.setContentSize(NSSize(width: 900, height: 560))
        NSApp.activate(ignoringOtherApps: true)
        window.makeKeyAndOrderFront(nil)
        defer { window.orderOut(nil) }

        controller.selectedServiceId = "api"

        var rendered = ""
        for _ in 0 ..< 100 {
            try await Task.sleep(for: .milliseconds(50))
            if let text = findTextView(in: hosting.view)?.string, text.contains("api log line") {
                rendered = text
                break
            }
        }
        XCTAssertTrue(rendered.contains("api log line"), "log text never reached the NSTextView; current: \(rendered.debugDescription)")
        let textView = try XCTUnwrap(findTextView(in: hosting.view))
        XCTAssertGreaterThan(textView.frame.width, 0, "log text view has zero width inside the split-view stack — content is laid out but invisible")
        XCTAssertGreaterThan(textView.frame.height, 0, "log text view has zero height inside the split-view stack — content is laid out but invisible")
    }

    /// The real app can mount the panel while its split-view pane still has zero size, then grow
    /// it. Without an autoresizing mask the document `NSTextView` keeps its zero-width frame
    /// forever — `widthTracksTextView` then wraps every line into invisibility and the panel
    /// renders blank even though `string` holds the log.
    @MainActor
    func testPanelCreatedAtZeroSizeThenResizedKeepsTextVisible() async throws {
        let api = FakeManagerAPI()
        api.logsHandler = { _, _ in makeLogSlice(data: "api log line\n", nextCursor: 13, generation: 1) }
        let log = LogController(client: api, serviceId: "api")

        let panel = ServiceLogPanel(serviceLabel: "api", log: log)
        let hosting = NSHostingController(rootView: panel)
        let window = NSWindow(contentViewController: hosting)
        window.setContentSize(NSSize(width: 0, height: 0))
        window.orderFront(nil)
        defer { window.orderOut(nil) }

        // Grow the window after the view exists — the split-view pane opening path in the app.
        window.setContentSize(NSSize(width: 480, height: 320))
        await log.fetchOnce()
        for _ in 0 ..< 50 {
            try await Task.sleep(for: .milliseconds(20))
            if findTextView(in: hosting.view)?.string == "api log line\n" { break }
        }
        log.stop()
        let textView = try XCTUnwrap(findTextView(in: hosting.view))
        XCTAssertTrue(textView.string.contains("api log line"), "log text never reached the NSTextView; current: \(textView.string.debugDescription)")
        XCTAssertGreaterThan(textView.frame.width, 0, "log text view stayed zero-width after the scroll view was resized — content is laid out but invisible")
    }

    /// Appends past the byte cap trim the text storage's head in place; what the view shows must
    /// still be exactly the controller's buffer.
    @MainActor
    func testTrimmedAppendsKeepTheViewEqualToTheBuffer() async throws {
        let api = FakeManagerAPI()
        let chunk = String(repeating: "→ a log line\n", count: 5_000) // ~75KB
        api.logsHandler = { _, _ in makeLogSlice(data: chunk, nextCursor: 1, generation: 1) }
        let log = LogController(client: api, serviceId: "api")

        let hosting = NSHostingController(rootView: ServiceLogPanel(serviceLabel: "api", log: log))
        let window = NSWindow(contentViewController: hosting)
        window.setContentSize(NSSize(width: 480, height: 320))
        window.orderFront(nil)
        defer { window.orderOut(nil) }
        for _ in 0 ..< 50 where findTextView(in: hosting.view) == nil {
            try await Task.sleep(for: .milliseconds(20))
        }
        log.stop() // the panel's `onAppear` started polling; step it by hand instead

        for _ in 0 ..< 6 {
            await log.fetchOnce()
        }
        let textView = try XCTUnwrap(findTextView(in: hosting.view))
        XCTAssertEqual(textView.string, log.text)
    }

    /// Switching the selected service must retarget the mounted `NSTextView` instead of building
    /// a second one and laying the buffer out again.
    @MainActor
    func testSwitchingServicesReusesTheLogTextView() async throws {
        let api = FakeManagerAPI()
        api.servicesHandler = { [makeService("api", actualState: "ready")] }
        api.catalogHandler = { ServiceCatalogSummary(services: [CatalogService(id: "api", label: "API", kind: nil, ownership: nil)]) }
        api.urlsHandler = { [] }
        api.daemonLogHandler = { makeLogSlice(data: "daemon log line\n", nextCursor: 16, generation: 1, reset: true) }
        api.logsHandler = { _, _ in makeLogSlice(data: "api log line\n", nextCursor: 13, generation: 1) }

        let workspace = makeWorkspace()
        let controller = WorkspaceController(workspace: workspace, watchesConfigFile: false, connector: { _ in api })
        await controller.connect()
        defer { controller.stop() }

        let hosting = NSHostingController(rootView: WorkspaceDetailView(controller: controller, workspace: workspace).environmentObject(WorkspaceStore()))
        let window = NSWindow(contentViewController: hosting)
        window.setContentSize(NSSize(width: 760, height: 480))
        window.orderFront(nil)
        defer { window.orderOut(nil) }

        controller.selectedServiceId = LogController.daemonServiceId
        let first = try await waitForTextView(in: hosting.view, containing: "daemon log line")

        controller.selectedServiceId = "api"
        let second = try await waitForTextView(in: hosting.view, containing: "api log line")
        XCTAssertTrue(first === second, "selecting another service built a new NSTextView")
        XCTAssertFalse(second.string.contains("daemon log line"))
        XCTAssertEqual(findTextViews(in: hosting.view).count, 1)
    }

    /// Switching workspaces updates the mounted detail. The log view stays.
    @MainActor
    func testSwitchingWorkspacesReusesTheLogTextView() async throws {
        let apiA = FakeManagerAPI()
        apiA.servicesHandler = { [makeService("api", actualState: "ready")] }
        apiA.catalogHandler = { ServiceCatalogSummary(services: [CatalogService(id: "api", label: "API", kind: nil, ownership: nil)]) }
        apiA.urlsHandler = { [] }
        apiA.logsHandler = { _, _ in makeLogSlice(data: "workspace-a log\n", nextCursor: 16, generation: 1) }
        let apiB = FakeManagerAPI()
        apiB.servicesHandler = { [makeService("api", actualState: "ready")] }
        apiB.catalogHandler = { ServiceCatalogSummary(services: [CatalogService(id: "api", label: "API", kind: nil, ownership: nil)]) }
        apiB.urlsHandler = { [] }
        apiB.logsHandler = { _, _ in makeLogSlice(data: "workspace-b log\n", nextCursor: 16, generation: 1) }

        let workspaceA = makeWorkspace(path: "/tmp/ws-a")
        let workspaceB = makeWorkspace(path: "/tmp/ws-b")
        let controllerA = WorkspaceController(workspace: workspaceA, watchesConfigFile: false, connector: { _ in apiA })
        let controllerB = WorkspaceController(workspace: workspaceB, watchesConfigFile: false, connector: { _ in apiB })
        await controllerA.connect()
        await controllerB.connect()
        defer { controllerA.stop(); controllerB.stop() }
        controllerA.selectedServiceId = "api"
        controllerB.selectedServiceId = "api"

        let box = DetailIndex()
        let hosting = NSHostingController(
            rootView: SwitchingDetail(box: box, workspaces: [workspaceA, workspaceB], controllers: [controllerA, controllerB])
                .environmentObject(WorkspaceStore())
        )
        let window = NSWindow(contentViewController: hosting)
        window.setContentSize(NSSize(width: 760, height: 480))
        window.orderFront(nil)
        defer { window.orderOut(nil) }

        let first = try await waitForTextView(in: hosting.view, containing: "workspace-a log")
        box.index = 1
        let second = try await waitForTextView(in: hosting.view, containing: "workspace-b log")
        XCTAssertTrue(first === second, "switching workspaces built a new NSTextView")
        XCTAssertFalse(second.string.contains("workspace-a log"))
        XCTAssertEqual(findTextViews(in: hosting.view).count, 1)
    }

    @MainActor
    private func waitForTextView(in root: NSView, containing needle: String) async throws -> NSTextView {
        var matched: NSTextView?
        for _ in 0 ..< 100 {
            try await Task.sleep(for: .milliseconds(50))
            if let textView = findTextView(in: root), textView.string.contains(needle) {
                matched = textView
                break
            }
        }
        let current = findTextViews(in: root).map(\.string).joined(separator: " | ")
        return try XCTUnwrap(matched, "log text \(needle.debugDescription) never reached the NSTextView; current: \(current.debugDescription)")
    }

    private func findTextViews(in view: NSView) -> [NSTextView] {
        var found: [NSTextView] = []
        if let textView = view as? NSTextView { found.append(textView) }
        for sub in view.subviews { found.append(contentsOf: findTextViews(in: sub)) }
        return found
    }
}

@MainActor
private final class DetailIndex: ObservableObject {
    @Published var index = 0
}

private struct SwitchingDetail: View {
    @ObservedObject var box: DetailIndex
    let workspaces: [Workspace]
    let controllers: [WorkspaceController]

    var body: some View {
        WorkspaceDetailView(controller: controllers[box.index], workspace: workspaces[box.index])
    }
}
