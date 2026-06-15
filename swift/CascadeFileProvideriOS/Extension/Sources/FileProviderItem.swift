import FileProvider
import UniformTypeIdentifiers

/// An `NSFileProviderItem` backed by a single Cascade VFS entry.
///
/// The File Provider addresses every item by an opaque `NSFileProviderItemIdentifier`.
/// We encode the item's VFS-absolute path directly into that identifier (with the
/// reserved root identifier mapped to the VFS root), so an identifier round-trips
/// to the path the engine understands without a separate lookup table.
final class FileProviderItem: NSObject, NSFileProviderItem {
    private let path: String
    private let isDirectory: Bool

    init(path: String, isDirectory: Bool) {
        self.path = path
        self.isDirectory = isDirectory
    }

    /// The VFS-absolute path this item represents.
    var vfsPath: String { path }

    var itemIdentifier: NSFileProviderItemIdentifier {
        FileProviderItem.identifier(forPath: path)
    }

    var parentItemIdentifier: NSFileProviderItemIdentifier {
        let parent = (path as NSString).deletingLastPathComponent
        if parent.isEmpty || parent == "/" {
            return .rootContainer
        }
        return FileProviderItem.identifier(forPath: parent)
    }

    var filename: String {
        let last = (path as NSString).lastPathComponent
        return last.isEmpty ? "/" : last
    }

    var contentType: UTType {
        isDirectory ? .folder : .data
    }

    var capabilities: NSFileProviderItemCapabilities {
        isDirectory ? [.allowsContentEnumerating, .allowsReading] : [.allowsReading]
    }

    /// Map an opaque item identifier back to a VFS-absolute path.
    ///
    /// The root container maps to the VFS root; every other identifier carries
    /// its path verbatim.
    static func path(forIdentifier identifier: NSFileProviderItemIdentifier) -> String {
        if identifier == .rootContainer {
            return "/"
        }
        return identifier.rawValue
    }

    /// Map a VFS-absolute path to an opaque item identifier.
    static func identifier(forPath path: String) -> NSFileProviderItemIdentifier {
        if path.isEmpty || path == "/" {
            return .rootContainer
        }
        return NSFileProviderItemIdentifier(path)
    }
}
