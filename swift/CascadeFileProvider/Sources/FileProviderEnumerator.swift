// SPDX-License-Identifier: Apache-2.0
//
// Cascade File Provider enumerator.
//
// The replicated File Provider API enumerates two things separately:
// the current items inside a container (`enumerateItems`) and the
// stream of changes since a sync anchor (`enumerateChanges`).
//
// Cascade's bridge protocol does not yet model sync anchors; the
// engine pushes individual notifications over the socket instead.
// Until that side grows a real cursor, this enumerator returns the
// engine's current view for `enumerateItems` and reports no further
// changes for `enumerateChanges`. The replicated API tolerates that:
// the system will simply re-enumerate when it next needs a refresh.

import FileProvider
import Foundation

final class FileProviderEnumerator: NSObject, NSFileProviderEnumerator {
    private let parentIdentifier: NSFileProviderItemIdentifier
    private let actions: ActionHandler

    init(parentIdentifier: NSFileProviderItemIdentifier, actions: ActionHandler) {
        self.parentIdentifier = parentIdentifier
        self.actions = actions
    }

    func invalidate() {}

    func enumerateItems(for observer: NSFileProviderEnumerationObserver, startingAt page: NSFileProviderPage) {
        Task {
            do {
                let items = try await actions.enumerateItems(parentIdentifier: parentIdentifier, page: page.rawValue)
                observer.didEnumerate(items)
                observer.finishEnumerating(upTo: nil)
            } catch {
                observer.finishEnumeratingWithError(error)
            }
        }
    }

    func enumerateChanges(for observer: NSFileProviderChangeObserver, from syncAnchor: NSFileProviderSyncAnchor) {
        // TODO(fileprovider): the Rust bridge needs an `enumerateChanges`
        //   method that returns a delta keyed on a server cursor before
        //   this implementation can do anything useful. Until then we
        //   acknowledge the anchor unchanged and let the system fall
        //   back to a full re-enumeration when it chooses.
        observer.finishEnumeratingChanges(upTo: syncAnchor, moreComing: false)
    }

    // TODO(fileprovider): once the Rust bridge exposes a stable change
    // cursor (e.g. a `currentSyncCursor` method returning an opaque
    // byte string the engine increments per write), return its bytes
    // here. Returning an empty Data tells the system the anchor is
    // meaningless, which forces a full re-enumeration on every sync
    // tick. Acceptable while the engine cursor isn't wired through;
    // not acceptable for steady-state operation.
    func currentSyncAnchor(completionHandler: @escaping (NSFileProviderSyncAnchor?) -> Void) {
        completionHandler(NSFileProviderSyncAnchor(Data()))
    }
}
