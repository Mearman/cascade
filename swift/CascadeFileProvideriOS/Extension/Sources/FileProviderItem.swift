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
        FileProviderPath.identifier(forPath: path)
    }

    var parentItemIdentifier: NSFileProviderItemIdentifier {
        FileProviderPath.identifier(forPath: FileProviderPath.parent(of: path))
    }

    var filename: String {
        FileProviderPath.name(of: path)
    }

    var contentType: UTType {
        isDirectory ? .folder : .data
    }

    var capabilities: NSFileProviderItemCapabilities {
        isDirectory ? [.allowsContentEnumerating, .allowsReading] : [.allowsReading]
    }
}
