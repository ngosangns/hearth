import XCTest

@testable import LocalServicesApp

final class ResolvedServiceUrlTests: XCTestCase {
    func testDisplayNameIsTheCatalogLabelWhenPresent() {
        let entry = ResolvedServiceUrl(serviceId: "api", label: "admin", url: "https://box.tail.ts.net:8443", requiresRunning: true)
        XCTAssertEqual(entry.displayName, "admin")
    }

    func testDisplayNameFallsBackToHostAndPort() {
        XCTAssertEqual(ResolvedServiceUrl(serviceId: "api", label: nil, url: "https://box.tail.ts.net:8443/x", requiresRunning: true).displayName, "box.tail.ts.net:8443")
        XCTAssertEqual(ResolvedServiceUrl(serviceId: "api", label: "", url: "https://devlocal.viclass.vn/math/", requiresRunning: true).displayName, "devlocal.viclass.vn")
    }

    /// The exact wire shape both daemons serve from `GET /v1/urls`.
    func testDecodesTheDaemonsResponse() throws {
        let json = #"{"urls":[{"serviceId":"api","label":"app","url":"http://127.0.0.1:18080/","requiresRunning":false},{"serviceId":"api","url":"http://127.0.0.1:18081/","requiresRunning":true}],"unresolved":[]}"#
        let decoded = try JSONDecoder().decode(UrlsResponse.self, from: Data(json.utf8))
        XCTAssertEqual(decoded.urls.count, 2)
        XCTAssertEqual(decoded.urls[0].label, "app")
        XCTAssertFalse(decoded.urls[0].requiresRunning)
        XCTAssertNil(decoded.urls[1].label)
    }
}

@MainActor
final class WorkspaceControllerUrlTests: XCTestCase {
    private func workspace() -> Workspace {
        Workspace(id: UUID(), path: "/tmp/project", trusted: true, addedAt: Date())
    }

    func testConnectFetchesUrlsAndFiltersThemPerService() async {
        let api = FakeManagerAPI()
        api.catalogHandler = { ServiceCatalogSummary(services: [], groups: [:]) }
        api.servicesHandler = { [makeService("api", actualState: "ready"), makeService("web", actualState: "stopped")] }
        api.urlsHandler = {
            [
                ResolvedServiceUrl(serviceId: "api", label: "admin", url: "https://box:8443", requiresRunning: true),
                ResolvedServiceUrl(serviceId: "web", label: nil, url: "http://127.0.0.1:3000", requiresRunning: false),
            ]
        }
        let sut = WorkspaceController(workspace: workspace(), watchesConfigFile: false, connector: { _ in api })
        await sut.connect()

        XCTAssertEqual(sut.urls.count, 2)
        XCTAssertEqual(sut.urls(for: "api").map(\.url), ["https://box:8443"])
        XCTAssertEqual(sut.urls(for: "web").map(\.url), ["http://127.0.0.1:3000"])
        XCTAssertTrue(sut.urls(for: "missing").isEmpty)
        sut.stop()
    }

    /// A daemon from before `/v1/urls` existed must not break the connection — URLs are optional.
    func testConnectSucceedsWhenTheDaemonHasNoUrlsEndpoint() async {
        let api = FakeManagerAPI()
        api.catalogHandler = { ServiceCatalogSummary(services: [], groups: [:]) }
        api.servicesHandler = { [makeService("api", actualState: "ready")] }
        // urlsHandler left unset -> throws, like a 404 from an older daemon.
        let sut = WorkspaceController(workspace: workspace(), watchesConfigFile: false, connector: { _ in api })
        await sut.connect()

        XCTAssertEqual(sut.phase, .connected)
        XCTAssertTrue(sut.urls.isEmpty)
        sut.stop()
    }
}
