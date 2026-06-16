import FileProvider

/// Path <-> item-identifier logic, extracted as a single source of truth so it
/// can be unit-tested standalone (a hostless logic-test target compiles just
/// this file, with no dependency on the Rust FFI or a host app).
///
/// The File Provider addresses items by an opaque `NSFileProviderItemIdentifier`.
/// The VFS-absolute path is encoded directly into that identifier, with the
/// reserved root identifier mapped to the VFS root `/`, so an identifier
/// round-trips to the path the engine understands without a lookup table.
enum FileProviderPath {
    /// Map a VFS-absolute path to an opaque item identifier. The root (empty or
    /// `/`) maps to the root container; every other path is carried verbatim.
    static func identifier(forPath path: String) -> NSFileProviderItemIdentifier {
        if path.isEmpty || path == "/" {
            return .rootContainer
        }
        return NSFileProviderItemIdentifier(path)
    }

    /// Map an opaque item identifier back to a VFS-absolute path. The root
    /// container maps to the VFS root; every other identifier carries its path
    /// verbatim.
    static func path(forIdentifier identifier: NSFileProviderItemIdentifier) -> String {
        if identifier == .rootContainer {
            return "/"
        }
        return identifier.rawValue
    }

    /// The VFS-absolute path of the parent of `path`. The root and any top-level
    /// path resolve to the root.
    static func parent(of path: String) -> String {
        let parent = (path as NSString).deletingLastPathComponent
        if parent.isEmpty || parent == "/" {
            return "/"
        }
        return parent
    }

    /// The final path segment of `path`, or `/` for the root.
    static func name(of path: String) -> String {
        let last = (path as NSString).lastPathComponent
        return last.isEmpty ? "/" : last
    }
}
