// swift-tools-version: 6.0
import PackageDescription

let package = Package(
    name: "hearth-app",
    platforms: [.macOS(.v14)],
    targets: [
        // Pure logic: hearth CLI runner, daemon HTTP client, wire models, workspace
        // list, service board rules, log buffer. No UI imports, so it is unit-testable.
        .target(name: "HearthKit", path: "Sources/HearthKit"),
        // SwiftUI app: state (Model), design system (Design), feature views.
        .executableTarget(
            name: "hearth-app",
            dependencies: ["HearthKit"],
            path: "Sources/hearth-app"
        ),
        .testTarget(
            name: "HearthKitTests",
            dependencies: ["HearthKit"],
            path: "Tests/HearthKitTests"
        ),
    ]
)
