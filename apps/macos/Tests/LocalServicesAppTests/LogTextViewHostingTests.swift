import AppKit
import SwiftUI
import XCTest

@testable import LocalServicesApp

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
        api.catalogHandler = { ServiceCatalogSummary(services: [CatalogService(id: "api", label: "API", kind: nil, ownership: nil)], groups: [:]) }
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
        api.catalogHandler = { ServiceCatalogSummary(services: [CatalogService(id: "api", label: "API", kind: nil, ownership: nil)], groups: [:]) }
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

        let rowRect = table.rect(ofRow: 0)
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
            table.selectRowIndexes(IndexSet(integer: 0), byExtendingSelection: false)
            try await Task.sleep(for: .milliseconds(100))
            XCTAssertEqual(controller.selectedServiceId, "api", "neither click nor programmatic selection reached the binding")
        }
        XCTAssertEqual(controller.selectedServiceId, "api")
    }

    /// The real app embeds `WorkspaceDetailView` in a `NavigationSplitView`'s `detail` slot —
    /// replicate that so the log panel is exercised inside the same split-view stack it runs in.
    @MainActor
    func testLogPanelInsideNavigationSplitViewDetail() async throws {
        let api = FakeManagerAPI()
        api.servicesHandler = { [makeService("api", actualState: "ready")] }
        api.catalogHandler = { ServiceCatalogSummary(services: [CatalogService(id: "api", label: "API", kind: nil, ownership: nil)], groups: [:]) }
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
}
