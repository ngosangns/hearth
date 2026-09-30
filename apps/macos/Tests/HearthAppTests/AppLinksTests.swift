import XCTest
@testable import HearthApp

final class AppLinksTests: XCTestCase {
    func testTheReleasesPageAndTheAppcastAreTheShippedURLs() {
        XCTAssertEqual(AppLinks.releases.absoluteString, "https://github.com/ngosangns/hearth/releases")
        XCTAssertEqual(AppLinks.appcast.absoluteString, "https://ngosangns.github.io/hearth/appcast.xml")
    }

    /// Sparkle reads `SUFeedURL` and `SUPublicEDKey` from the packaged Info.plist. Those strings
    /// have to be the same ones the appcast publisher and the menus use.
    func testInfoPlistMatchesTheSparkleConstants() throws {
        let plist = URL(fileURLWithPath: #filePath)
            .deletingLastPathComponent()
            .deletingLastPathComponent()
            .deletingLastPathComponent()
            .appendingPathComponent("Info.plist")
        let data = try Data(contentsOf: plist)
        let dict = try XCTUnwrap(PropertyListSerialization.propertyList(from: data, format: nil) as? [String: Any])
        XCTAssertEqual(dict["SUFeedURL"] as? String, AppLinks.appcast.absoluteString)
        XCTAssertEqual(dict["SUPublicEDKey"] as? String, AppLinks.publicEDKey)
        XCTAssertEqual(dict["SUEnableAutomaticChecks"] as? Bool, true)
        XCTAssertEqual(dict["SUScheduledCheckInterval"] as? Int, 86400)
    }
}
