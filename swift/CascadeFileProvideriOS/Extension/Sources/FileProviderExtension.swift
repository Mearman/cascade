import FileProvider

/// In-process Cascade File Provider for iOS.
///
/// Unlike the macOS extension, which talks to a separate daemon over a Unix
/// socket, the iOS extension links the engine directly: it builds a
/// `CascadeNode` over the app-group container and serves the Files app from it.
/// Item metadata comes from `list_dir`, content from `read_file`.
final class FileProviderExtension: NSObject, NSFileProviderReplicatedExtension {
    private let domain: NSFileProviderDomain

    required init(domain: NSFileProviderDomain) {
        self.domain = domain
        super.init()
        // Force the engine to construct and start eagerly so the first
        // enumeration does not pay the whole startup cost. Errors here are
        // surfaced later on the first request rather than crashing init.
        Task { _ = try? await CascadeEngine.shared.node() }
    }

    func invalidate() {}

    func item(
        for identifier: NSFileProviderItemIdentifier,
        request: NSFileProviderRequest,
        completionHandler: @escaping (NSFileProviderItem?, Error?) -> Void
    ) -> Progress {
        let progress = Progress(totalUnitCount: 1)
        if identifier == .rootContainer {
            completionHandler(FileProviderItem(path: "/", isDirectory: true), nil)
            progress.completedUnitCount = 1
            return progress
        }
        let path = FileProviderPath.path(forIdentifier: identifier)
        let parent = (path as NSString).deletingLastPathComponent
        let name = (path as NSString).lastPathComponent
        let listPath = parent.isEmpty ? "/" : parent
        Task {
            do {
                let node = try await CascadeEngine.shared.node()
                let entries = try await node.listDir(path: listPath)
                guard let match = entries.first(where: { $0.name == name }) else {
                    completionHandler(nil, NSFileProviderError(.noSuchItem))
                    progress.completedUnitCount = 1
                    return
                }
                completionHandler(FileProviderItem(path: path, isDirectory: match.isDir), nil)
                progress.completedUnitCount = 1
            } catch {
                completionHandler(nil, error)
                progress.completedUnitCount = 1
            }
        }
        return progress
    }

    func fetchContents(
        for itemIdentifier: NSFileProviderItemIdentifier,
        version requestedVersion: NSFileProviderItemVersion?,
        request: NSFileProviderRequest,
        completionHandler: @escaping (URL?, NSFileProviderItem?, Error?) -> Void
    ) -> Progress {
        let progress = Progress(totalUnitCount: 1)
        let path = FileProviderPath.path(forIdentifier: itemIdentifier)
        Task {
            do {
                let node = try await CascadeEngine.shared.node()
                let bytes = try await node.readFile(path: path)
                let temporary = FileManager.default.temporaryDirectory
                    .appendingPathComponent(UUID().uuidString)
                    .appendingPathExtension("cascade")
                try bytes.write(to: temporary)
                let item = FileProviderItem(path: path, isDirectory: false)
                completionHandler(temporary, item, nil)
                progress.completedUnitCount = 1
            } catch {
                completionHandler(nil, nil, error)
                progress.completedUnitCount = 1
            }
        }
        return progress
    }

    func createItem(
        basedOn itemTemplate: NSFileProviderItem,
        fields: NSFileProviderItemFields,
        contents url: URL?,
        options: NSFileProviderCreateItemOptions = [],
        request: NSFileProviderRequest,
        completionHandler: @escaping (NSFileProviderItem?, NSFileProviderItemFields, Bool, Error?) -> Void
    ) -> Progress {
        let progress = Progress(totalUnitCount: 1)
        let path = FileProviderPath.path(forIdentifier: itemTemplate.itemIdentifier)
        let isDirectory = itemTemplate.contentType == .folder
        Task {
            do {
                let node = try await CascadeEngine.shared.node()
                if isDirectory {
                    try await node.createDir(path: path)
                } else {
                    let bytes = try url.map { try Data(contentsOf: $0) } ?? Data()
                    try await node.upload(path: path, bytes: bytes)
                }
                let item = FileProviderItem(path: path, isDirectory: isDirectory)
                completionHandler(item, [], false, nil)
                progress.completedUnitCount = 1
            } catch {
                completionHandler(nil, [], false, error)
                progress.completedUnitCount = 1
            }
        }
        return progress
    }

    func modifyItem(
        _ item: NSFileProviderItem,
        baseVersion version: NSFileProviderItemVersion,
        changedFields: NSFileProviderItemFields,
        contents newContents: URL?,
        options: NSFileProviderModifyItemOptions = [],
        request: NSFileProviderRequest,
        completionHandler: @escaping (NSFileProviderItem?, NSFileProviderItemFields, Bool, Error?) -> Void
    ) -> Progress {
        let progress = Progress(totalUnitCount: 1)
        let path = FileProviderPath.path(forIdentifier: item.itemIdentifier)
        // Replacing the file's contents is the only modification the extension
        // bridges today; other changed fields (attributes, name) fall through.
        Task {
            do {
                if let newContents {
                    let node = try await CascadeEngine.shared.node()
                    let bytes = try Data(contentsOf: newContents)
                    try await node.upload(path: path, bytes: bytes)
                }
                let item = FileProviderItem(
                    path: path,
                    isDirectory: item.contentType == .folder
                )
                completionHandler(item, [], false, nil)
                progress.completedUnitCount = 1
            } catch {
                completionHandler(nil, [], false, error)
                progress.completedUnitCount = 1
            }
        }
        return progress
    }

    func deleteItem(
        identifier: NSFileProviderItemIdentifier,
        baseVersion version: NSFileProviderItemVersion,
        options: NSFileProviderDeleteItemOptions = [],
        request: NSFileProviderRequest,
        completionHandler: @escaping (Error?) -> Void
    ) -> Progress {
        let progress = Progress(totalUnitCount: 1)
        let path = FileProviderPath.path(forIdentifier: identifier)
        Task {
            do {
                let node = try await CascadeEngine.shared.node()
                try await node.delete(path: path)
                completionHandler(nil)
                progress.completedUnitCount = 1
            } catch {
                completionHandler(error)
                progress.completedUnitCount = 1
            }
        }
        return progress
    }

    func enumerator(
        for containerItemIdentifier: NSFileProviderItemIdentifier,
        request: NSFileProviderRequest
    ) throws -> NSFileProviderEnumerator {
        FileProviderEnumerator(enumeratedItemIdentifier: containerItemIdentifier)
    }
}
