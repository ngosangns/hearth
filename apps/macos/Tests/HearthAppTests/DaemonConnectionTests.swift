import XCTest
@testable import HearthApp

final class DaemonConnectionTests: XCTestCase {
    private func sh(_ script: String) -> (URL, [String]) {
        (URL(fileURLWithPath: "/bin/sh"), ["-c", script])
    }

    /// The bug this pins: the timeout raced a continuation that ignored cancellation, so the task
    /// group waited on the hung child forever and the timeout never surfaced.
    func testTimeoutTerminatesAHungChildAndReturnsPromptly() async throws {
        let start = ContinuousClock.now
        do {
            _ = try await Subprocess.withTimeout(.milliseconds(300)) {
                try await Subprocess.run(URL(fileURLWithPath: "/bin/sleep"), arguments: ["30"])
            }
            XCTFail("expected a timeout")
        } catch DaemonConnectionError.timedOut {}
        XCTAssertLessThan(ContinuousClock.now - start, .seconds(5))
    }

    /// An interactive login shell ignores SIGTERM, and a grandchild can keep the pipes open — the
    /// timeout must still return within SIGKILL grace + drain grace.
    func testTimeoutKillsAChildThatIgnoresSIGTERM() async throws {
        let (shell, args) = sh("trap '' TERM; sleep 10")
        let start = ContinuousClock.now
        do {
            _ = try await Subprocess.withTimeout(.milliseconds(300)) {
                try await Subprocess.run(shell, arguments: args)
            }
            XCTFail("expected a timeout")
        } catch DaemonConnectionError.timedOut {}
        XCTAssertLessThan(ContinuousClock.now - start, .seconds(8))
    }

    /// More than the ~64KB pipe buffer: a child whose output nobody reads blocks on `write` and
    /// never exits.
    func testOutputLargerThanThePipeBufferIsDrained() async throws {
        let (shell, args) = sh("head -c 300000 /dev/zero | tr '\\000' x")
        let result = try await Subprocess.withTimeout(.seconds(20)) {
            try await Subprocess.run(shell, arguments: args)
        }
        XCTAssertEqual(result.status, 0)
        XCTAssertEqual(result.stdout.utf8.count, 300_000)
    }

    func testANonZeroExitKeepsItsStatusAndStderr() async throws {
        let (shell, args) = sh("echo out; echo oops >&2; exit 3")
        let result = try await Subprocess.run(shell, arguments: args)
        XCTAssertEqual(result.status, 3)
        XCTAssertEqual(result.stdout, "out\n")
        XCTAssertEqual(result.stderr, "oops\n")
    }

    func testDecodeConnectionAcceptsTheSupportedProtocol() throws {
        let json = #"{"instanceId":"i","port":62066,"token":"t","protocolVersion":1,"runtimeDirectory":"/r","root":"/x"}"#
        let connection = try DaemonConnection.decodeConnection(json)
        XCTAssertEqual(connection.port, 62066)
    }

    /// A `PROTOCOL_VERSION` bump is breaking — refuse before any request, with a message that says
    /// what to do, rather than as scattered decode errors later.
    func testDecodeConnectionRejectsAnotherProtocolVersion() {
        let json = #"{"instanceId":"i","port":62066,"token":"t","protocolVersion":2,"runtimeDirectory":"/r","root":"/x"}"#
        XCTAssertThrowsError(try DaemonConnection.decodeConnection(json)) { error in
            guard case DaemonConnectionError.incompatibleProtocol(daemon: 2, app: ManagerClient.supportedProtocolVersion) = error else {
                return XCTFail("expected incompatibleProtocol, got \(error)")
            }
            XCTAssertTrue(error.localizedDescription.contains("Restart Daemon"))
        }
    }

    func testDecodeConnectionReportsMalformedOutput() {
        XCTAssertThrowsError(try DaemonConnection.decodeConnection("Error: no catalog")) { error in
            guard case DaemonConnectionError.malformedOutput = error else { return XCTFail("got \(error)") }
        }
    }
}
