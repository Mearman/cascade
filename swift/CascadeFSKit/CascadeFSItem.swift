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

/// Actor-isolated file-ID allocator.
///
/// FSKit dispatches its callbacks concurrently. A plain `static var`
/// without synchronisation lets two concurrent `getNextID()` calls return
/// the same value. Moving the counter behind an actor serialises access
/// so every item gets a unique ID even under concurrent creation.
private actor IDAllocator {
    private var next: UInt64

    init(startingFrom value: UInt64) {
        next = value
    }

    func nextID() -> UInt64 {
        let current = next
        next += 1
        return current
    }
}

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

    /// Shared, actor-isolated ID allocator. Starts above the root-directory ID
    /// so it never collides with well-known kernel constants.
    private static let idAllocator = IDAllocator(
        startingFrom: FSItem.Identifier.rootDirectory.rawValue + 1
    )

    /// Maps cascade parent IDs to their `FSItem.Identifier` values.
    /// Populated by `CascadeFSVolume` when items are created via engine
    /// responses, so that children can resolve their real parent instead of
    /// being flattened to the root.
    private static var parentRegistry: [String: FSItem.Identifier] = [:]

    /// Serialises mutations to `parentRegistry`.
    private static let registryLock = NSLock()

    /// Allocate the next unique file ID. The actor ensures that even when
    /// two FSKit callbacks race through this path they cannot receive the
    /// same value.
    ///
    /// `await` is required because `IDAllocator` is an actor. FSKit's volume
    /// operations are already `async`, so the suspension cost is negligible.
    static func allocateID() async -> UInt64 {
        await idAllocator.nextID()
    }

    /// Register a mapping from a cascade engine ID to the `FSItem.Identifier`
    /// that was allocated for the same FSItem.
    ///
    /// Called by `CascadeFSVolumeOps` after each new `CascadeFSItem` is
    /// constructed from an engine response, and then consulted by subsequently
    /// created children that carry the same cascade ID in their `parent_id`
    /// field.
    static func registerParent(_ cascadeID: String, as fsIdentifier: FSItem.Identifier) {
        registryLock.lock()
        defer { registryLock.unlock() }
        parentRegistry[cascadeID] = fsIdentifier
    }

    /// Look up the `FSItem.Identifier` for a cascade parent ID that was
    /// previously registered with `registerParent(_:as:)`.
    static func identifier(forParentCascadeID cascadeID: String) -> FSItem.Identifier? {
        registryLock.lock()
        defer { registryLock.unlock() }
        return parentRegistry[cascadeID]
    }

    init(cascadeID: String, name: FSFileName, fileID: UInt64) {
        self.cascadeID = cascadeID
        self.name = name
        self.fileID = fileID

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
    convenience init(json: [String: Any]) async throws {
        let id = json["id"] as? String ?? ""
        let filename = json["filename"] as? String ?? "unknown"
        let allocatedID = await CascadeFSItem.allocateID()
        self.init(cascadeID: id, name: FSFileName(string: filename), fileID: allocatedID)

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
            } else if let resolved = CascadeFSItem.identifier(forParentCascadeID: parentID) {
                attributes.parentID = resolved
            } else {
                // Parent not yet known — leave as the kernel default rather
                // than falsely claiming root. The parent will be resolved on
                // the next readdir that includes the real parent.
                attributes.parentID = FSItem.Identifier.invalid
            }
        }

        // Register this item so subsequent children created from engine
        // responses can resolve their parent relationship back to us.
        CascadeFSItem.registerParent(id, as: attributes.fileID)

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
