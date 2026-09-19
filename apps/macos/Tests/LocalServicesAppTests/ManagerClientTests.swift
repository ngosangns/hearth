// Regression coverage for a bug where `logs()`'s hand-built `"path?query"` string went through
// `URL.appendingPathComponent`, which percent-encodes `?`/`&` as literal path characters instead of
// treating them as a query delimiter — the daemon's router then 404'd every log request as
// `service_not_found` (a plausible id never matches), and the app silently showed no log output.

import XCTest
@testable import LocalServicesApp

final class ManagerClientTests: XCTestCase {
    private func makeClient() -> ManagerClient {
        let connection = ManagerConnection(
            instanceId: "test", port: 50101, token: "tok3n", protocolVersion: 1,
            runtimeDirectory: "/tmp/x/.local-services/runtime-v1", root: "/tmp/x"
        )
        return ManagerClient(connection: connection)
    }

    func testRequestPutsQueryStringInTheURLQueryNotThePath() {
        let request = makeClient().request("/v1/logs/kafka?limit=16384&cursor=100&generation=1")
        let url = request.url!
        XCTAssertEqual(url.path, "/v1/logs/kafka")
        XCTAssertEqual(url.query, "limit=16384&cursor=100&generation=1")
        XCTAssertEqual(url.absoluteString, "http://127.0.0.1:50101/v1/logs/kafka?limit=16384&cursor=100&generation=1")
    }

    func testRequestWithNoQueryStringIsUnaffected() {
        let request = makeClient().request("/v1/services")
        XCTAssertEqual(request.url?.absoluteString, "http://127.0.0.1:50101/v1/services")
    }

    func testRequestSetsAuthAndProtocolHeaders() {
        let request = makeClient().request("/v1/services")
        XCTAssertEqual(request.value(forHTTPHeaderField: "authorization"), "Bearer tok3n")
        XCTAssertEqual(request.value(forHTTPHeaderField: "x-local-services-protocol"), "1")
    }
}
