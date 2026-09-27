import XCTest
@testable import HearthApp

final class SidecarLocatorTests: XCTestCase {
    private func locator(
        executables: Set<String>,
        environment: [String: String] = [:],
        bundle: URL? = URL(fileURLWithPath: "/App.app/Contents/Resources"),
        shellOutput: String? = nil,
        shellCalls: Counter = Counter()
    ) -> SidecarLocator {
        SidecarLocator(
            environment: environment,
            bundleResourceURL: bundle,
            isExecutable: { executables.contains($0) },
            knownPaths: ["/known/first", "/known/second"],
            devRepoPaths: ["/repo/release", "/repo/debug"],
            loginShellLookup: { _ in
                shellCalls.increment()
                return shellOutput
            }
        )
    }

    private let bundled = "/App.app/Contents/Resources/hearthd/bin/hearthd"

    func testEnvironmentOverrideWinsOverEverything() async {
        let found = await locator(
            executables: ["/override", bundled, "/known/first"],
            environment: ["HEARTH_BIN_PATH": "/override"]
        ).resolve()
        XCTAssertEqual(found, "/override")
    }

    func testAnOverrideThatIsNotExecutableIsSkipped() async {
        let found = await locator(executables: [bundled], environment: ["HEARTH_BIN_PATH": "/missing"]).resolve()
        XCTAssertEqual(found, bundled)
    }

    func testBundledCopyBeatsInstalledLocations() async {
        let found = await locator(executables: [bundled, "/known/first"]).resolve()
        XCTAssertEqual(found, bundled)
    }

    func testKnownLocationsAreTriedInOrderBeforeTheDevRepo() async {
        let found = await locator(executables: ["/known/second", "/repo/release"]).resolve()
        XCTAssertEqual(found, "/known/second")
    }

    func testDevRepoPrefersReleaseOverDebug() async {
        let found = await locator(executables: ["/repo/debug", "/repo/release"]).resolve()
        XCTAssertEqual(found, "/repo/release")
    }

    func testTheLoginShellIsOnlyAskedAsALastResort() async {
        let calls = Counter()
        _ = await locator(executables: ["/repo/debug"], shellCalls: calls).resolve()
        XCTAssertEqual(calls.value, 0)

        let found = await locator(executables: ["/opt/bin/hearthd"], shellOutput: "/opt/bin/hearthd\n", shellCalls: calls).resolve()
        XCTAssertEqual(found, "/opt/bin/hearthd")
        XCTAssertEqual(calls.value, 1)
    }

    /// Interactive rc files print banners around the answer — only an absolute path to an
    /// executable is accepted, never the raw output.
    func testLoginShellNoiseIsFilteredOut() async {
        let noisy = "Welcome back!\nnvm: using node 20\n/opt/bin/hearthd\n"
        let found = await locator(executables: ["/opt/bin/hearthd"], shellOutput: noisy).resolve()
        XCTAssertEqual(found, "/opt/bin/hearthd")

        let onlyNoise = await locator(executables: [], shellOutput: "Welcome back!\n/not/executable\n").resolve()
        XCTAssertNil(onlyNoise)
    }

    func testNothingFoundIsNil() async {
        let found = await locator(executables: [], bundle: nil).resolve()
        XCTAssertNil(found)
    }
}
