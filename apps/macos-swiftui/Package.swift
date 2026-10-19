// swift-tools-version: 6.0
import PackageDescription

let package = Package(
    name: "hearth-app",
    platforms: [.macOS(.v14)],
    dependencies: [
        .package(path: "../../../ns-shell"),
    ],
    targets: [
        // Headless client of the hearth daemon. No UI imports.
        // Zed: crates/client. The daemon (go/) is crates/project.
        .target(name: "HearthKit", path: "Sources/HearthKit"),
        // Composition root. App binds HearthCommand. WindowShell is the
        // workspace chrome. Design is the component crate. See ARCHITECTURE.md.
        .executableTarget(
            name: "hearth-app",
            dependencies: ["HearthKit", .product(name: "NSShell", package: "ns-shell")],
            path: "Sources/hearth-app"
        ),
        .testTarget(
            name: "HearthKitTests",
            dependencies: ["HearthKit"],
            path: "Tests/HearthKitTests"
        ),
    ]
)
