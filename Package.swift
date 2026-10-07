// swift-tools-version: 6.0
import PackageDescription

let package = Package(
    name: "SpeedTracker",
    platforms: [.macOS(.v14)],
    dependencies: [
        .package(url: "https://github.com/facebook/zstd.git", exact: "1.5.7")
    ],
    targets: [
        .systemLibrary(name: "CSQLite", pkgConfig: "sqlite3"),
        // UI-free measurement, log readers and historical analytics.
        .target(
            name: "SpeedTrackerCore",
            dependencies: ["CSQLite", .product(name: "libzstd", package: "zstd")],
            swiftSettings: [.swiftLanguageMode(.v5)]
        ),
        // Menu bar app.
        .executableTarget(
            name: "SpeedTracker",
            dependencies: ["SpeedTrackerCore"],
            swiftSettings: [.swiftLanguageMode(.v5)]
        ),
        .testTarget(
            name: "SpeedTrackerCoreTests",
            dependencies: ["SpeedTrackerCore"],
            swiftSettings: [.swiftLanguageMode(.v5)]
        ),
    ]
)
