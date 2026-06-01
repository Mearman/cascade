// SPDX-License-Identifier: Apache-2.0
//
// Cascade macOS File Provider extension.
//
// This is the principal class for the `.appex` bundle. macOS has only
// ever supported `NSFileProviderReplicatedExtension` for non-iCloud
// providers; the older `NSFileProviderExtension` is unavailable on
// macOS regardless of deployment target.
//
// The replicated API is "pull" — the system asks us for items and
// content, then mirrors them into a system-managed working set on
// disk. We do not manage URLs, placeholders, or sandboxed copies of
// our own.
//
// Each operation here forwards to the Cascade engine over the
// existing Unix domain socket bridge (see `ActionHandler`). Where the
// Rust side has not grown a matching method yet, the handler is
// stubbed to return `NSFileProviderError.notAuthenticated` with a
// TODO that names the bridge endpoint the Rust side needs.

import FileProvider
import Foundation

final class CascadeFileProvider: NSObject, NSFileProviderReplicatedExtension {
    private let domain: NSFileProviderDomain
    private let actions: ActionHandler

    init(domain: NSFileProviderDomain) {
        self.domain = domain
        self.actions = ActionHandler()
        super.init()
    }

    func invalidate() {
        // The replicated API guarantees `invalidate` is called once,
        // before the extension is torn down. We hold no long-lived
        // resources beyond the per-request socket connections opened
        // by `ActionHandler`; nothing to do here today.
    }

    // MARK: - Item lookup

    func item(
        for identifier: NSFileProviderItemIdentifier,
        request: NSFileProviderRequest,
        completionHandler: @escaping (NSFileProviderItem?, Error?) -> Void
    ) -> Progress {
        let progress = Progress(totalUnitCount: 1)
        Task {
            do {
                let item = try await actions.item(for: identifier)
                progress.completedUnitCount = 1
                completionHandler(item, nil)
            } catch {
                completionHandler(nil, mapEngineError(error))
            }
        }
        return progress
    }

    // MARK: - Content fetch

    func fetchContents(
        for itemIdentifier: NSFileProviderItemIdentifier,
        version requestedVersion: NSFileProviderItemVersion?,
        request: NSFileProviderRequest,
        completionHandler: @escaping (URL?, NSFileProviderItem?, Error?) -> Void
    ) -> Progress {
        let progress = Progress(totalUnitCount: 1)
        Task {
            do {
                let contentURL = try await actions.fetchContents(for: itemIdentifier)
                let item = try await actions.item(for: itemIdentifier)
                progress.completedUnitCount = 1
                completionHandler(contentURL, item, nil)
            } catch {
                completionHandler(nil, nil, mapEngineError(error))
            }
        }
        return progress
    }

    // MARK: - Mutating operations

    func createItem(
        basedOn itemTemplate: NSFileProviderItem,
        fields: NSFileProviderItemFields,
        contents url: URL?,
        options: NSFileProviderCreateItemOptions,
        request: NSFileProviderRequest,
        completionHandler: @escaping (NSFileProviderItem?, NSFileProviderItemFields, Bool, Error?) -> Void
    ) -> Progress {
        let progress = Progress(totalUnitCount: 1)
        Task {
            do {
                let created = try await actions.createItem(
                    template: itemTemplate,
                    contents: url
                )
                progress.completedUnitCount = 1
                completionHandler(created, [], false, nil)
            } catch {
                completionHandler(nil, [], false, mapEngineError(error))
            }
        }
        return progress
    }

    func modifyItem(
        _ item: NSFileProviderItem,
        baseVersion version: NSFileProviderItemVersion,
        changedFields: NSFileProviderItemFields,
        contents newContents: URL?,
        options: NSFileProviderModifyItemOptions,
        request: NSFileProviderRequest,
        completionHandler: @escaping (NSFileProviderItem?, NSFileProviderItemFields, Bool, Error?) -> Void
    ) -> Progress {
        let progress = Progress(totalUnitCount: 1)
        Task {
            do {
                let modified = try await actions.modifyItem(
                    item: item,
                    changedFields: changedFields,
                    newContents: newContents
                )
                progress.completedUnitCount = 1
                completionHandler(modified, [], false, nil)
            } catch {
                completionHandler(nil, [], false, mapEngineError(error))
            }
        }
        return progress
    }

    func deleteItem(
        identifier: NSFileProviderItemIdentifier,
        baseVersion version: NSFileProviderItemVersion,
        options: NSFileProviderDeleteItemOptions,
        request: NSFileProviderRequest,
        completionHandler: @escaping (Error?) -> Void
    ) -> Progress {
        let progress = Progress(totalUnitCount: 1)
        Task {
            do {
                try await actions.deleteItem(identifier: identifier)
                progress.completedUnitCount = 1
                completionHandler(nil)
            } catch {
                completionHandler(mapEngineError(error))
            }
        }
        return progress
    }

    // MARK: - Enumeration

    func enumerator(
        for containerItemIdentifier: NSFileProviderItemIdentifier,
        request: NSFileProviderRequest
    ) throws -> NSFileProviderEnumerator {
        FileProviderEnumerator(parentIdentifier: containerItemIdentifier, actions: actions)
    }

    // MARK: - Helpers

    /// Promote a raw engine error to the File Provider error domain.
    ///
    /// macOS surfaces these via the Finder's "Unable to … (Error code)"
    /// dialogue. Structured RPC errors are already mapped to specific
    /// `NSFileProviderError` cases inside `ActionHandler`; those pass
    /// through untouched. Local transport and decode failures collapse
    /// to `.serverUnreachable` (socket gone) or `.cannotSynchronize`
    /// (engine returned an unparseable response) so the user sees a
    /// meaningful state regardless of which side of the bridge failed.
    private func mapEngineError(_ error: Error) -> Error {
        if let cascadeError = error as? CascadeFileProviderError {
            switch cascadeError {
            case .socket:
                return NSFileProviderError(.serverUnreachable)
            case .invalidResponse:
                return NSFileProviderError(.cannotSynchronize)
            }
        }
        return error
    }
}
