// SPDX-License-Identifier: Apache-2.0
//
// Cascade File Provider enumerator.
//
// The replicated File Provider API enumerates two things separately:
// the current items inside a container (`enumerateItems`) and the
// stream of changes since a sync anchor (`enumerateChanges`).
//
// Cascade's bridge protocol does not yet model `enumerateChanges`; the
// engine pushes individual notifications over the socket instead. Until
// that side grows a real delta stream, this enumerator reports no
// further changes for `enumerateChanges` and relies on the system to
// re-enumerate when it compares `currentSyncAnchor` against its
// last-known anchor. The sync anchor itself is now real — see
// `currentSyncAnchor` below — so a change to any item beneath this
// container will invalidate the anchor and trigger a fresh enumeration.

import FileProvider
import Foundation
import os

private let enumeratorLogger = Logger(subsystem: "com.cascade.fileprovider", category: "FileProviderEnumerator")

/// Encode arbitrary bytes as a base64url-no-pad string.
///
/// The Rust File Provider bridge emits both page cursors and sync
/// cursors as base64url-no-pad: standard base64 with `+` → `-`, `/` →
/// `_`, and no `=` padding. Keeping the wire form JSON-safe avoids
/// quoting at the protocol level. Used by both `currentSyncAnchor`
/// (sync cursor) and `enumerateItems` (page cursor).
func base64URLNoPadEncode(_ data: Data) -> String {
    var encoded = data.base64EncodedString()
    encoded = encoded.replacingOccurrences(of: "+", with: "-")
    encoded = encoded.replacingOccurrences(of: "/", with: "_")
    encoded = encoded.replacingOccurrences(of: "=", with: "")
    return encoded
}

/// Decode a base64url-no-pad string back to its raw bytes.
///
/// Returns `nil` for malformed input. Re-pads to a multiple of four
/// characters and translates `-`/`_` back to `+`/`/` before handing off
/// to Foundation's standard base64 decoder.
func base64URLNoPadDecode(_ string: String) -> Data? {
    var standard = string.replacingOccurrences(of: "-", with: "+")
    standard = standard.replacingOccurrences(of: "_", with: "/")
    let padCount = (4 - standard.count % 4) % 4
    standard.append(String(repeating: "=", count: padCount))
    return Data(base64Encoded: standard)
}

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
                let initialCursor = decodeStartingPage(page)
                let (items, _) = try await actions.enumerateItems(
                    parentIdentifier: parentIdentifier,
                    pageCursor: initialCursor
                )
                observer.didEnumerate(items)
                observer.finishEnumerating(upTo: nil)
            } catch {
                observer.finishEnumeratingWithError(error)
            }
        }
    }

    /// Translate the system's `NSFileProviderPage` into the Rust-side
    /// opaque cursor string, or `nil` for the initial page.
    ///
    /// The system uses two well-known sentinels for the initial page —
    /// `initialPageSortedByName` and `initialPageSortedByDate` — whose
    /// `rawValue` bytes are not a cursor we emitted. Treat both as "no
    /// cursor". Anything else is a base64url-no-pad string we previously
    /// encoded into the page; round-trip it back through UTF-8.
    fileprivate func decodeStartingPage(_ page: NSFileProviderPage) -> String? {
        if page.rawValue == NSFileProviderPage.initialPageSortedByName as Data {
            return nil
        }
        if page.rawValue == NSFileProviderPage.initialPageSortedByDate as Data {
            return nil
        }
        return String(data: page.rawValue, encoding: .utf8)
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
