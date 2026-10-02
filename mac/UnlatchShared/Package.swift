// swift-tools-version:5.9
// UnlatchShared: the Swift code shared by Unlatch.app (UI + engine agent) and UnlatchFileProvider.appex —
// Codable mirrors of the Rust IPC protocol, the libunlatch bridge, item/error mapping, and helpers.
// The Xcode project compiles these sources directly into each target; this package exists so
// `swift test` can check them (including against the Rust-generated JSON fixtures) in CI.
import Foundation
import PackageDescription

// `mac/scripts/build-rust.sh` puts the universal libunlatch.a here.
let rustLibDir = URL(fileURLWithPath: #filePath)
    .deletingLastPathComponent()
    .appendingPathComponent("../build/rust")
    .standardizedFileURL.path

// The protocol mirrors, codec and helpers also build on Linux (CI runs them there first, and the
// libunlatch they link is then the Linux build); only the File Provider mapping is macOS-only.
#if os(Linux)
let macOnlySources = ["FileProviderMapping.swift", "MaterializedSet.swift", "XPCProtocols.swift"]
let macOnlyTests = ["MappingTests.swift"]
let platformLinks: [LinkerSetting] = [.linkedLibrary("m"), .linkedLibrary("dl"), .linkedLibrary("pthread")]
#else
let macOnlySources: [String] = []
let macOnlyTests: [String] = []
let platformLinks: [LinkerSetting] = [
    .linkedLibrary("iconv"),
    .linkedFramework("CoreFoundation"),
    .linkedFramework("Security"),
]
#endif

let package = Package(
    name: "UnlatchShared",
    platforms: [.macOS(.v13)],
    products: [.library(name: "UnlatchShared", targets: ["UnlatchShared"])],
    targets: [
        .systemLibrary(name: "CUnlatch", path: "Sources/CUnlatch"),
        .target(
            name: "UnlatchShared",
            dependencies: ["CUnlatch"],
            path: "Sources/UnlatchShared",
            exclude: macOnlySources,
            linkerSettings: [.unsafeFlags(["-L", rustLibDir]), .linkedLibrary("unlatch")] + platformLinks
        ),
        .testTarget(
            name: "UnlatchSharedTests",
            dependencies: ["UnlatchShared"],
            path: "Tests/UnlatchSharedTests",
            exclude: macOnlyTests
        ),
    ],
    swiftLanguageVersions: [.v5]
)
