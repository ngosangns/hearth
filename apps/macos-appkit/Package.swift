// swift-tools-version: 5.9
import PackageDescription

let package = Package(
    name: "HearthApp",
    platforms: [.macOS(.v13)],
    targets: [
        .executableTarget(
            name: "Hearth",
            path: "Sources/Hearth"
        )
    ]
)
