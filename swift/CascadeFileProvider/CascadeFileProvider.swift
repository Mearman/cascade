import FileProvider
import Foundation
import UniformTypeIdentifiers

final class CascadeFileProvider: NSFileProviderExtension {
    private let actions = ActionHandler()

    override func item(for identifier: NSFileProviderItemIdentifier, completionHandler: @escaping (NSFileProviderItem?, Error?) -> Void) -> Progress {
        Task {
            do {
                let item = try await actions.item(for: identifier)
                completionHandler(item, nil)
            } catch {
                completionHandler(nil, error)
            }
        }
        return Progress(totalUnitCount: 1)
    }

    override func urlForItem(withPersistentIdentifier identifier: NSFileProviderItemIdentifier) -> URL? {
        guard let storageURL else { return nil }
        return storageURL.appendingPathComponent(identifier.rawValue, isDirectory: false)
    }

    override func persistentIdentifierForItem(at url: URL) -> NSFileProviderItemIdentifier? {
        NSFileProviderItemIdentifier(url.lastPathComponent)
    }

    override func providePlaceholder(at url: URL, completionHandler: @escaping (Error?) -> Void) {
        Task {
            do {
                guard let identifier = persistentIdentifierForItem(at: url) else {
                    throw CocoaError(.fileNoSuchFile)
                }
                let item = try await actions.item(for: identifier)
                let placeholderURL = NSFileProviderManager.placeholderURL(for: url)
                try FileManager.default.createDirectory(at: placeholderURL.deletingLastPathComponent(), withIntermediateDirectories: true)
                try NSFileProviderManager.writePlaceholder(at: placeholderURL, withMetadata: item)
                completionHandler(nil)
            } catch {
                completionHandler(error)
            }
        }
    }

    override func startProvidingItem(at url: URL, completionHandler: @escaping (Error?) -> Void) {
        Task {
            do {
                guard let identifier = persistentIdentifierForItem(at: url) else {
                    throw CocoaError(.fileNoSuchFile)
                }
                let sourceURL = try await actions.fetchContents(for: identifier)
                try FileManager.default.createDirectory(at: url.deletingLastPathComponent(), withIntermediateDirectories: true)
                if FileManager.default.fileExists(atPath: url.path) {
                    try FileManager.default.removeItem(at: url)
                }
                try FileManager.default.copyItem(at: sourceURL, to: url)
                completionHandler(nil)
            } catch {
                completionHandler(error)
            }
        }
    }

    override func itemChanged(at url: URL) {
        guard let identifier = persistentIdentifierForItem(at: url) else { return }
        Task {
            try await actions.uploadChangedItem(identifier: identifier, fileURL: url)
        }
    }

    override func stopProvidingItem(at url: URL) {
        try? FileManager.default.removeItem(at: url)
        providePlaceholder(at: url) { _ in }
    }

    override func enumerator(for containerItemIdentifier: NSFileProviderItemIdentifier) throws -> NSFileProviderEnumerator {
        FileProviderEnumerator(parentIdentifier: containerItemIdentifier, actions: actions)
    }
}
