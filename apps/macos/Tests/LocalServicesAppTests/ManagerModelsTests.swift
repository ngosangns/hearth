// Decode-fidelity tests: every fixture below is byte-for-byte real output, captured from a live
// daemon (`lsd ... manager ensure/start`, then `curl`ing each endpoint directly)
// rather than hand-written to match what the Swift models expect. This is the one place that would
// catch these Codable structs silently drifting from what the daemon actually sends.

import XCTest
@testable import LocalServicesApp

final class ManagerModelsTests: XCTestCase {
    func testDecodesManagerConnectionFromLsdManagerEnsure() throws {
        let json = """
        {"instanceId":"c9dce777-795b-4158-a7f5-04277f130739","port":62066,"token":"dkzb_LkMKHIbJTrMOu0H3iM8RkbXwU9KBjcmjWvd1aQ","protocolVersion":1,"runtimeDirectory":"/tmp/lsd-json-sample/.local-services/runtime-v1","root":"/tmp/lsd-json-sample"}
        """
        let connection = try JSONDecoder().decode(ManagerConnection.self, from: Data(json.utf8))
        XCTAssertEqual(connection.instanceId, "c9dce777-795b-4158-a7f5-04277f130739")
        XCTAssertEqual(connection.port, 62066)
        XCTAssertEqual(connection.protocolVersion, 1)
        XCTAssertEqual(connection.baseURL, URL(string: "http://127.0.0.1:62066"))
    }

    func testDecodesServicesResponseWithAProcessIdentity() throws {
        let json = """
        {"services":[{"serviceId":"sleeper","desiredState":"running","actualState":"running-unready","readiness":"not-ready","generation":1,"createdAt":"2026-09-19T16:35:17.744Z","updatedAt":"2026-09-19T16:35:17.763Z","identity":{"managerInstanceId":"c9dce777-795b-4158-a7f5-04277f130739","serviceId":"sleeper","generation":1,"startedAt":"2026-09-19T16:35:17.756Z","pid":94901,"pgid":94901,"startIdentity":"Sat Sep 19 23:35:17 2026","commandFingerprint":"637cbdb3daf0341b069901e72cca1a318646a4bf08b67ab1823db46fca9c1aef"},"readinessKind":"process","readinessDetail":"process-liveness-only","currentOperationId":"1ab0426a-a58c-4a95-97f2-34509d727cd9"}]}
        """
        let response = try JSONDecoder().decode(ServicesResponse.self, from: Data(json.utf8))
        XCTAssertEqual(response.services.count, 1)
        let service = response.services[0]
        XCTAssertEqual(service.serviceId, "sleeper")
        XCTAssertEqual(service.actualState, "running-unready")
        XCTAssertEqual(service.identity?.pid, 94901)
        XCTAssertEqual(service.displayState, "starting") // running-unready collapses to "starting"
    }

    func testDecodesServicesResponseWithNoIdentityOrOptionalFields() throws {
        // A never-started service, as it appears on a fresh runtime directory — every optional field
        // absent, matching manager.test.ts's "lists every catalog service as stopped" coverage.
        let json = """
        {"services":[{"serviceId":"sleeper","desiredState":"stopped","actualState":"stopped","readiness":"unknown","generation":0,"createdAt":"2026-09-19T16:35:17.744Z","updatedAt":"2026-09-19T16:35:17.744Z"}]}
        """
        let response = try JSONDecoder().decode(ServicesResponse.self, from: Data(json.utf8))
        XCTAssertNil(response.services[0].identity)
        XCTAssertEqual(response.services[0].displayState, "stopped")
    }

    func testVisuallyEqualIgnoresUpdatedAt() {
        let a = makeService("api", actualState: "ready")
        let b = ServiceLifecycleState(
            serviceId: a.serviceId,
            desiredState: a.desiredState,
            actualState: a.actualState,
            readiness: a.readiness,
            generation: a.generation,
            identity: a.identity,
            readinessKind: a.readinessKind,
            readinessDetail: a.readinessDetail,
            createdAt: a.createdAt,
            updatedAt: "2099-01-01T00:00:00.000Z",
            exitedAt: a.exitedAt,
            exitCode: a.exitCode,
            error: a.error,
            currentOperationId: a.currentOperationId
        )
        XCTAssertTrue(a.isVisuallyEqual(to: b))
        XCTAssertFalse(a.isVisuallyEqual(to: makeService("api", actualState: "stopped")))
    }

    func testDecodesCatalogResponse() throws {
        let json = """
        {"catalog":{"startFailurePolicy":"stop-on-first-failure-keep-started","services":[{"id":"sleeper","profiles":{"run":{"commandStatus":"verified","readiness":{"kind":"process"},"command":{"command":{"argv":["sleep","30"]},"cwd":"."}}}}],"groups":{}}}
        """
        let response = try JSONDecoder().decode(CatalogResponse.self, from: Data(json.utf8))
        XCTAssertEqual(response.catalog.services.map(\.id), ["sleeper"])
        XCTAssertEqual(response.catalog.services[0].displayName, "sleeper") // falls back to id when label is absent
        XCTAssertEqual(response.catalog.groups, [:])
    }

    func testDecodesLogSlice() throws {
        let json = """
        {"serviceId":"sleeper","generation":1,"cursor":0,"nextCursor":0,"data":"","reset":false,"truncated":false}
        """
        let slice = try JSONDecoder().decode(LogSlice.self, from: Data(json.utf8))
        XCTAssertEqual(slice.serviceId, "sleeper")
        XCTAssertFalse(slice.reset)
    }

    func testDecodesManagerErrorEnvelope() throws {
        // Real shape from ManagerHttpError's json() response (manager.ts) on e.g. an unknown service.
        let json = """
        {"error":{"code":"invalid_service","message":"serviceId must be a catalog service"}}
        """
        let envelope = try JSONDecoder().decode(ManagerErrorEnvelope.self, from: Data(json.utf8))
        XCTAssertEqual(envelope.error.code, "invalid_service")
    }

    func testDisplayStateCollapsesEveryActualState() {
        let cases: [(String, String)] = [
            ("ready", "ready"),
            ("queued-start", "queued"),
            ("running", "starting"),
            ("running-unready", "starting"),
            ("starting", "starting"),
            ("preparing", "starting"),
            ("stopping", "stopping"),
            ("failed", "failed"),
            ("orphaned", "orphaned"),
            ("externally-owned", "external"),
            ("stopped", "stopped"),
        ]
        for (actual, expectedDisplay) in cases {
            let state = ServiceLifecycleState(serviceId: "x", desiredState: "running", actualState: actual, readiness: "unknown", generation: 0, identity: nil, readinessKind: nil, readinessDetail: nil, createdAt: "2026-01-01T00:00:00.000Z", updatedAt: "2026-01-01T00:00:00.000Z", exitedAt: nil, exitCode: nil, error: nil, currentOperationId: nil)
            XCTAssertEqual(state.displayState, expectedDisplay, "actualState \(actual)")
        }
    }
}

final class WorkspaceTests: XCTestCase {
    func testDisplayNameIsTheLastPathComponent() {
        let workspace = Workspace(path: "/Users/dev/Github/myapp")
        XCTAssertEqual(workspace.displayName, "myapp")
    }

    func testExistsOnDiskIsFalseForAMissingPath() {
        let workspace = Workspace(path: "/definitely/does/not/exist/\(UUID().uuidString)")
        XCTAssertFalse(workspace.existsOnDisk)
    }

    func testExistsOnDiskIsTrueForARealDirectory() {
        let workspace = Workspace(path: NSTemporaryDirectory())
        XCTAssertTrue(workspace.existsOnDisk)
    }
}
