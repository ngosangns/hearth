// swift-tools-version: 5.10
import PackageDescription

let package = Package(
    name: "HearthApp",
    platforms: [.macOS(.v13)],
    dependencies: [
        .package(url: "https://github.com/sparkle-project/Sparkle", exact: "2.10.0"),
    ],
    targets: [
        // tools-version 5.10 already defaults every target to the Swift 5 language mode (no Swift 6
        // strict-concurrency checking) — nothing extra to opt out of.
        .executableTarget(
            name: "HearthApp",
            dependencies: [.product(name: "Sparkle", package: "Sparkle")]
        ),
        .testTarget(name: "HearthAppTests", dependencies: ["HearthApp"]),
    ]
)
