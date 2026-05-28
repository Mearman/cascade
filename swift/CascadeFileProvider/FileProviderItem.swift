import FileProvider
import Foundation
import UniformTypeIdentifiers

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

final class FileProviderItem: NSObject, NSFileProviderItem {
    private let item: CascadeVfsItem

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

    var typeIdentifier: String {
        if item.isDirectory {
            return UTType.folder.identifier
        }
        if let contentType = item.contentType {
            return contentType
        }
        return UTType.data.identifier
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
}
