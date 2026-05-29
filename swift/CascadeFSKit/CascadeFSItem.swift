//
//  CascadeFSItem.swift
//  CascadeFSKit
//
//  Represents a file or directory in the Cascade virtual filesystem.
//  Wraps FSItem with Cascade-specific metadata (id, cached data, xattrs).
//  The FSKit kernel module sees these as real POSIX filesystem objects.
//

import Foundation
import FSKit

final class CascadeFSItem: FSItem {

    /// Cascade engine item identifier (format: "backend:native_id").
    let cascadeID: String

    /// Display name in the filesystem.
    let name: FSFileName

    /// Unique file ID assigned at creation.
    let fileID: UInt64

    /// Cached attributes from the engine.
    var attributes = FSItem.Attributes()

    /// Extended attributes.
    var xattrs: [FSFileName: Data] = [:]

    /// Cached file content (for small files; large files stream from engine).
    var data: Data?

    /// Child items for directories.
    private(set) var children: [FSFileName: CascadeFSItem] = [:]

    private static var nextID: UInt64 = FSItem.Identifier.rootDirectory.rawValue + 1

    static func getNextID() -> UInt64 {
        let current = nextID
        nextID += 1
        return current
    }

    init(cascadeID: String, name: FSFileName) {
        self.cascadeID = cascadeID
        self.name = name
        self.fileID = CascadeFSItem.getNextID()

        var timespec = timespec()
        timespec_get(&timespec, TIME_UTC)

        attributes.fileID = FSItem.Identifier(rawValue: fileID) ?? .invalid
        attributes.size = 0
        attributes.allocSize = 0
        attributes.flags = 0
        attributes.addedTime = timespec
        attributes.birthTime = timespec
        attributes.changeTime = timespec
        attributes.modifyTime = timespec
        attributes.accessTime = timespec
    }

    /// Convenience initialiser from a JSON dictionary received from the engine.
    convenience init(json: [String: Any]) throws {
        let id = json["id"] as? String ?? ""
        let filename = json["filename"] as? String ?? "unknown"
        self.init(cascadeID: id, name: FSFileName(string: filename))

        if let isDir = json["is_directory"] as? Bool, isDir {
            attributes.type = .directory
            attributes.mode = UInt32(S_IFDIR | 0o755)
        } else {
            attributes.type = .regularFile
            attributes.mode = UInt32(S_IFREG | 0o644)
        }

        if let size = json["size"] as? Int64 {
            attributes.size = UInt64(size)
            attributes.allocSize = UInt64(size)
        } else if let size = json["size"] as? Int {
            attributes.size = UInt64(size)
            attributes.allocSize = UInt64(size)
        }

        if let parentID = json["parent_id"] as? String {
            if parentID == "root" {
                attributes.parentID = .parentOfRoot
            } else {
                // Map parent cascade ID to its FSItem.Identifier if known.
                // For now we use the numeric representation.
                attributes.parentID = .rootDirectory
            }
        }

        attributes.uid = 0
        attributes.gid = 0
        attributes.linkCount = 1
    }

    func addItem(_ item: CascadeFSItem) {
        children[item.name] = item
    }

    func removeItem(_ item: CascadeFSItem) {
        children[item.name] = nil
    }
}
