import Foundation

/// Holds the single in-process `CascadeNode` shared by the extension.
///
/// The node is constructed lazily on first use and started once. Construction
/// roots the engine at a directory inside the app-group container so the host
/// app and the extension address the same state database and cache. The actor
/// serialises construction so concurrent enumerator and fetch requests cannot
/// race two nodes into existence.
actor CascadeEngine {
    static let shared = CascadeEngine()

    /// App-group identifier shared by the host app and the extension.
    ///
    /// Both targets declare this group in their entitlements; the container URL
    /// it resolves to is the engine's config root.
    static let appGroupIdentifier = "group.co.uk.mearman.cascade"

    private var node: CascadeNode?

    /// Return the started node, constructing and starting it on first call.
    func node() async throws -> CascadeNode {
        if let node {
            return node
        }
        let configDir = try Self.configDirectory()
        let node = try await CascadeNode(configDir: configDir)
        try await node.start()
        self.node = node
        return node
    }

    /// Resolve the engine's config directory inside the app-group container.
    ///
    /// Falls back to the extension's own caches directory when the app group is
    /// unavailable (for example in a build without the entitlement provisioned),
    /// so the extension still has a writable root rather than failing to start.
    private static func configDirectory() throws -> String {
        let fileManager = FileManager.default
        let base: URL
        if let container = fileManager.containerURL(
            forSecurityApplicationGroupIdentifier: appGroupIdentifier
        ) {
            base = container
        } else {
            base = try fileManager.url(
                for: .cachesDirectory,
                in: .userDomainMask,
                appropriateFor: nil,
                create: true
            )
        }
        let configDir = base.appendingPathComponent("cascade", isDirectory: true)
        try fileManager.createDirectory(at: configDir, withIntermediateDirectories: true)
        return configDir.path
    }
}
