// swift-tools-version: 5.9
// Swift Package manifest for compiling the FSKit extension sources outside Xcode.
// A production app extension target still needs entitlements, an Info.plist, and an Xcode
// project or generated project wrapper to register the FSKit module with macOS.
// FSKit requires macOS 15.4+ (Sequoia) as a minimum deployment target.

import PackageDescription

let package = Package(
    name: "CascadeFSKit",
    platforms: [.macOS(.v15_4)],
    products: [
        .library(name: "CascadeFSKit", targets: ["CascadeFSKit"]),
    ],
    targets: [
        .target(
            name: "CascadeFSKit",
            path: ".",
            sources: [
                "CascadeFSKitExtension.swift",
                "CascadeConstants.swift",
                "CascadeFileSystem.swift",
                "CascadeFSItem.swift",
                "CascadeFSVolume.swift",
                "CascadeFSVolumeOps.swift",
                "EngineClient.swift",
            ]
        ),
    ]
)
