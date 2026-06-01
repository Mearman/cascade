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

/// JSON payload shape returned by the Cascade engine for a VFS item.
///
/// The field names mirror `crates/presenter-fileprovider/src/items.rs::FileProviderItem`,
/// which is the canonical wire schema for the Rust ↔ Swift bridge.
struct CascadeVfsItem: Sendable {
    let id: String
    let parentID: String
    let filename: String
    let isDirectory: Bool
    let size: Int64?
    let contentType: String?
    let lastModified: Date?
    let cacheState: String

    init(json: [String: Any]) throws {
        self.id = try Self.string(json, "id")
        self.parentID = try Self.string(json, "parent_id")
        self.filename = try Self.string(json, "filename")
        self.isDirectory = try Self.bool(json, "is_directory")
        self.size = Self.optionalInt64(json, "size")
        self.contentType = json["content_type"] as? String
        self.cacheState = try Self.string(json, "cache_state")

        if let value = json["last_modified"] as? String {
            self.lastModified = ISO8601DateFormatter().date(from: value)
        } else {
            self.lastModified = nil
        }
    }

    var asJSON: [String: Any] {
        var json: [String: Any] = [
            "id": id,
            "parent_id": parentID,
            "filename": filename,
            "is_directory": isDirectory,
            "cache_state": cacheState,
        ]
        if let size { json["size"] = size }
        if let contentType { json["content_type"] = contentType }
        if let lastModified { json["last_modified"] = ISO8601DateFormatter().string(from: lastModified) }
        return json
    }

    private static func string(_ json: [String: Any], _ key: String) throws -> String {
        guard let value = json[key] as? String else {
            throw CascadeFileProviderError.invalidResponse("missing string field: \(key)")
        }
        return value
    }

    private static func bool(_ json: [String: Any], _ key: String) throws -> Bool {
        guard let value = json[key] as? Bool else {
            throw CascadeFileProviderError.invalidResponse("missing boolean field: \(key)")
        }
        return value
    }

    private static func optionalInt64(_ json: [String: Any], _ key: String) -> Int64? {
        if let value = json[key] as? Int64 { return value }
        if let value = json[key] as? Int { return Int64(value) }
        if let value = json[key] as? NSNumber { return value.int64Value }
        return nil
    }
}

/// `NSFileProviderItem` implementation backed by a `CascadeVfsItem`.
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
