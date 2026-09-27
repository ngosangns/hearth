import XCTest
@testable import HearthApp

final class AppLinksTests: XCTestCase {
    func testReleasesPageIsDerivedFromAGitHubFeed() {
        XCTAssertEqual(
            AppLinks.releasesPage(forFeed: URL(string: "https://github.com/ngosangns/hearth/releases.atom")!),
            URL(string: "https://github.com/ngosangns/hearth/releases")
        )
    }

    func testANonGitHubFeedHasNoReleasesPage() {
        XCTAssertNil(AppLinks.releasesPage(forFeed: URL(string: "https://example.com/appcast.xml")!))
    }

    /// No `SUFeedURL` in the bundle (Sparkle is not linked) — the fallback is this repo's page.
    func testTheFallbackIsThisRepositorysReleases() {
        XCTAssertEqual(AppLinks.fallbackReleases.absoluteString, "https://github.com/ngosangns/hearth/releases")
    }
}
