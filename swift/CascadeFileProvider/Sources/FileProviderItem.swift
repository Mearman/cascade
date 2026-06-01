// SPDX-License-Identifier: Apache-2.0
//
// Cascade File Provider item model.
//
// The macOS replicated File Provider API exposes items through
// `NSFileProviderItem`. The model below is a thin value type that
// wraps the JSON shape returned by the Cascade engine over the local
// Unix domain socket bridge, plus the conformance the system requires.
//
// Only the metadata Cascade actually tracks today is surfaced here.
// Anything the engine does not yet model (favourites, share state,
// download progress) is omitted rather than faked.

import FileProvider
import Foundation
import UniformTypeIdentifiers

/// `NSFileProviderItem` implementation backed by a `CascadeVfsItem`.
///
/// The `CascadeVfsItem` wire shape is defined in `ActionHandler.swift`
/// alongside the rest of the RPC envelope types so the entire protocol
/// surface stays in one place.
///
/// `contentType: UTType` replaces the macOS-unavailable
/// `typeIdentifier: String`. `itemVersion` is required by the replicated
/// API so the system can distinguish a content change from a metadata
/// change. Both halves of the version are derived from data that
/// already changes when the underlying item does — the modification
/// date for content, the cache state for metadata — so a fresh fetch
/// from the engine produces an item the system can compare against the
/// last value it saw.
final class FileProviderItem: NSObject, NSFileProviderItem {
    let item: CascadeVfsItem

    init(item: CascadeVfsItem) {
        self.item = item
    }

    var itemIdentifier: NSFileProviderItemIdentifier {
        NSFileProviderItemIdentifier(item.id)
    }

    var parentItemIdentifier: NSFileProviderItemIdentifier {
        item.parentID == "root" ? .rootContainer : NSFileProviderItemIdentifier(item.parentID)
    }

    var filename: String {
        item.filename
    }

    var contentType: UTType {
        if item.isDirectory {
            return .folder
        }
        if let raw = item.contentType, let type = UTType(mimeType: raw) {
            return type
        }
        if let raw = item.contentType, let type = UTType(raw) {
            return type
        }
        return .data
    }

    var documentSize: NSNumber? {
        item.size.map(NSNumber.init(value:))
    }

    var contentModificationDate: Date? {
        item.lastModified
    }

    var capabilities: NSFileProviderItemCapabilities {
        if item.isDirectory {
            return [.allowsReading, .allowsWriting, .allowsAddingSubItems, .allowsRenaming, .allowsDeleting]
        }
        return [.allowsReading, .allowsWriting, .allowsRenaming, .allowsDeleting, .allowsReparenting]
    }

    var isUploaded: Bool {
        item.cacheState != "uploading"
    }

    var isDownloaded: Bool {
        item.cacheState == "cached" || item.cacheState == "pinned"
    }

    var isMostRecentVersionDownloaded: Bool {
        isDownloaded
    }

    /// Replicated File Provider versioning.
    ///
    /// `contentVersion` flips when the file contents may have changed.
    /// We derive it from the engine's last-modified timestamp because
    /// that is the strongest signal the bridge currently exposes; if
    /// the engine has no modification date, a zero-filled token tells
    /// the system "no version known yet".
    ///
    /// `metadataVersion` flips when item attributes — capabilities,
    /// cache state, filename — may have changed. We derive it from the
    /// engine's reported cache state; that is conservative but cheap.
    var itemVersion: NSFileProviderItemVersion {
        let content = versionToken(forContent: true)
        let metadata = versionToken(forContent: false)
        return NSFileProviderItemVersion(contentVersion: content, metadataVersion: metadata)
    }

    private func versionToken(forContent: Bool) -> Data {
        if forContent {
            if let date = item.lastModified {
                var seconds = Int64(date.timeIntervalSince1970.rounded())
                return withUnsafeBytes(of: &seconds) { Data($0) }
            }
            return Data(count: MemoryLayout<Int64>.size)
        }
        return Data(item.cacheState.utf8) + Data(item.filename.utf8)
    }
}
