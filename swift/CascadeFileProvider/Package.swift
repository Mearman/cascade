// swift-tools-version: 5.9
// Swift Package manifest for compiling the File Provider extension sources outside Xcode.
// A production app extension target still needs entitlements, an Info.plist, and an Xcode
// project or generated project wrapper to register the File Provider domain with macOS.

import PackageDescription

let package = Package(
    name: "CascadeFileProvider",
    platforms: [.macOS(.v11)],
    products: [
        .library(name: "CascadeFileProvider", targets: ["CascadeFileProvider"]),
    ],
    targets: [
        .target(
            name: "CascadeFileProvider",
            path: ".",
            sources: [
                "CascadeFileProvider.swift",
                "FileProviderItem.swift",
                "FileProviderEnumerator.swift",
                "ActionHandler.swift",
            ]
        ),
    ]
)
