import FileProvider

/// Enumerates the children of one container by listing the Cascade VFS.
///
/// The File Provider asks the enumerator for the contents of a container
/// identified by an item identifier; we map that identifier to a VFS path,
/// call `list_dir` on the in-process node, and feed each entry back as a
/// `FileProviderItem`. A single page carries the whole listing — the engine's
/// `list_dir` returns the directory in full, so there is no cursor to resume.
final class FileProviderEnumerator: NSObject, NSFileProviderEnumerator {
    private let containerPath: String

    init(enumeratedItemIdentifier: NSFileProviderItemIdentifier) {
        self.containerPath = FileProviderItem.path(forIdentifier: enumeratedItemIdentifier)
    }

    func invalidate() {}

    func enumerateItems(
        for observer: NSFileProviderEnumerationObserver,
        startingAt page: NSFileProviderPage
    ) {
        let containerPath = self.containerPath
        Task {
            do {
                let node = try await CascadeEngine.shared.node()
                let entries = try await node.listDir(path: containerPath)
                let prefix = containerPath == "/" ? "" : containerPath
                let items: [NSFileProviderItem] = entries.map { entry in
                    let childPath = "\(prefix)/\(entry.name)"
                    return FileProviderItem(path: childPath, isDirectory: entry.isDir)
                }
                observer.didEnumerate(items)
                observer.finishEnumerating(upTo: nil)
            } catch {
                observer.finishEnumeratingWithError(error)
            }
        }
    }
}
