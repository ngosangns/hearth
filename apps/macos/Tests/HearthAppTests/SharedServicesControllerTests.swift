import XCTest

@testable import HearthApp

func makeSharedInstance(_ id: String, state: String? = "ready", installState: String = "installed", attachments: [SharedAttachment] = []) -> SharedInstance {
    let split = id.split(separator: "@").map(String.init)
    return SharedInstance(
        id: id,
        name: split.first ?? id,
        version: split.count > 1 ? split[1] : "",
        port: 43001,
        installState: installState,
        installError: nil,
        state: state.map { SharedInstanceState(actualState: $0, readiness: $0) },
        attachments: attachments
    )
}

final class SharedServicesControllerTests: XCTestCase {
    private var controller: SharedServicesController!
    private var api: FakeManagerAPI!

    @MainActor
    private func makeController() -> SharedServicesController {
        api = FakeManagerAPI()
        api.sharedInstancesHandler = { [makeSharedInstance("redis@7.2")] }
        api.sharedCatalogHandler = { SharedCatalogDocument(version: 1, services: ["redis": SharedFamily(versions: ["7.2": SharedRecipeSummary(description: nil)])]) }
        return SharedServicesController(connector: { self.api! })
    }

    @MainActor
    func testConnectPublishesInstancesAndCatalog() async {
        controller = makeController()
        await controller.connect()
        XCTAssertEqual(controller.phase, .connected)
        XCTAssertEqual(controller.instances.map(\.id), ["redis@7.2"])
        XCTAssertNotNil(controller.catalogDoc?.services["redis"])
    }

    @MainActor
    func testInstallCallsTheSharedEndpointAndRefreshes() async {
        controller = makeController()
        await controller.connect()
        controller.install("redis@7.2")
        // `install` dispatches a Task — wait for the action to clear.
        for _ in 0..<100 where controller.actionsInFlight.contains("redis@7.2") { try? await Task.sleep(for: .milliseconds(10)) }
        XCTAssertEqual(api.installed, ["redis@7.2"])
    }

    @MainActor
    func testRemoveCallsTheSharedEndpoint() async {
        controller = makeController()
        await controller.connect()
        controller.remove("redis@7.2")
        for _ in 0..<100 where controller.actionsInFlight.contains("redis@7.2") { try? await Task.sleep(for: .milliseconds(10)) }
        XCTAssertEqual(api.removed, ["redis@7.2"])
    }

    @MainActor
    func testConnectFailureSurfacesTheReason() async {
        let failing = SharedServicesController(connector: { throw ManagerClientError.operationFailed("no sidecar") })
        await failing.connect()
        guard case .failed(let message) = failing.phase else { return XCTFail("expected failed, got \(failing.phase)") }
        XCTAssertTrue(message.contains("no sidecar"))
    }
}
