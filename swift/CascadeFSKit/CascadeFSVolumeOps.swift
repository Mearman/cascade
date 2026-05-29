//
//  CascadeFSVolumeOps.swift
//  CascadeFSKit
//
//  FSVolume protocol conformances for CascadeFSVolume.
//  These are the actual filesystem operation handlers that the macOS
//  kernel invokes via FSKit. Each operation is translated into an
//  engine protocol message sent over the Unix domain socket.
//
//  Supported operations:
//  - FSVolume.Operations: lifecycle, attributes, lookup, readdir
//  - FSVolume.PathConfOperations: pathconf values
//  - FSVolume.ReadWriteOperations: read/write data
//  - FSVolume.OpenCloseOperations: open/close tracking
//  - FSVolume.XattrOperations: extended attributes
//

import Foundation
import FSKit
import os

// MARK: - Error helpers

func posixError(_ code: POSIXErrorCode) -> Error {
    fs_errorForPOSIXError(code.rawValue)
}

// MARK: - PathConf

extension CascadeFSVolume: FSVolume.PathConfOperations {
    var maximumLinkCount: Int { -1 }
    var maximumNameLength: Int { 255 }
    var restrictsOwnershipChanges: Bool { false }
    var truncatesLongNames: Bool { false }
    var maximumXattrSize: Int { 65536 }
    var maximumFileSize: UInt64 { UInt64.max }
}

// MARK: - Capabilities (computed once)

private let cascadeCapabilities: FSVolume.SupportedCapabilities = {
    let caps = FSVolume.SupportedCapabilities()
    caps.supportsHardLinks = false
    caps.supportsSymbolicLinks = true
    caps.supportsPersistentObjectIDs = true
    caps.doesNotSupportVolumeSizes = false
    caps.supportsHiddenFiles = true
    caps.supports64BitObjectIDs = true
    // Cascade stores filenames case-sensitively regardless of backend,
    // matching the internal VFS path resolution semantics.
    caps.caseFormat = .sensitive
    return caps
}()

// MARK: - Core Operations

extension CascadeFSVolume: FSVolume.Operations {

    var supportedVolumeCapabilities: FSVolume.SupportedCapabilities {
        cascadeCapabilities
    }

    var volumeStatistics: FSStatFSResult {
        let result = FSStatFSResult(fileSystemTypeName: CascadeConstants.fileSystemName)
        // Report large values — the engine manages actual space.
        result.blockSize = 4096
        result.ioSize = 65536
        result.totalBlocks = 1_000_000
        result.availableBlocks = 500_000
        result.freeBlocks = 500_000
        result.totalFiles = 1_000_000
        result.freeFiles = 500_000
        return result
    }

    func activate(options: FSTaskOptions) async throws -> FSItem {
        logger.debug("activate")
        // Ask the engine for the root directory contents.
        // The root item is created locally; its children come from the engine.
        return root
    }

    func deactivate(options: FSDeactivateOptions = []) async throws {
        logger.debug("deactivate")
    }

    func mount(options: FSTaskOptions) async throws {
        logger.debug("mount — Cascade volume mounted")
    }

    func unmount() async {
        logger.debug("unmount — Cascade volume unmounted")
    }

    func synchronize(flags: FSSyncFlags) async throws {
        // The engine handles sync; nothing to flush at the FSKit layer.
    }

    // MARK: - Attribute I/O

    func attributes(
        _ desiredAttributes: FSItem.GetAttributesRequest,
        of item: FSItem
    ) async throws -> FSItem.Attributes {
        if let item = item as? CascadeFSItem {
            return item.attributes
        }
        throw posixError(.EIO)
    }

    func setAttributes(
        _ newAttributes: FSItem.SetAttributesRequest,
        on item: FSItem
    ) async throws -> FSItem.Attributes {
        guard let cascadeItem = item as? CascadeFSItem else {
            throw posixError(.EIO)
        }
        // Forward setattr to the engine.
        _ = try await engine.send(
            method: "setAttributes",
            params: [
                "id": cascadeItem.cascadeID,
            ]
        )
        // Re-fetch attributes from the engine.
        if let result = try await engine.send(
            method: "getItem",
            params: ["id": cascadeItem.cascadeID]
        ) as? [String: Any] {
            let updated = try await CascadeFSItem(json: result)
            return updated.attributes
        }
        return cascadeItem.attributes
    }

    // MARK: - Directory traversal

    func lookupItem(
        named name: FSFileName,
        inDirectory directory: FSItem
    ) async throws -> (FSItem, FSFileName) {
        guard let dir = directory as? CascadeFSItem else {
            throw posixError(.EIO)
        }

        // Check local children first (items already fetched from the engine).
        if let child = dir.children[name] {
            return (child, name)
        }

        // Ask the engine to look up the child.
        guard let result = try await engine.send(
            method: "lookupItem",
            params: [
                "parent_id": dir.cascadeID,
                "name": String(data: name.data, encoding: .utf8) ?? name.string ?? "",
            ]
        ) as? [String: Any] else {
            throw posixError(.ENOENT)
        }

        let child = try await CascadeFSItem(json: result)
        dir.addItem(child)
        return (child, name)
    }

    func contents(
        ofDirectory directory: FSItem,
        startingAt cookie: FSDirectoryCookie,
        into enumerator: FSContentEnumerator
    ) async throws {
        guard let dir = directory as? CascadeFSItem else {
            throw posixError(.EIO)
        }

        guard let result = try await engine.send(
            method: "enumerateItems",
            params: [
                "parent_id": dir.cascadeID,
                "cookie": cookie.rawValue,
            ]
        ) as? [[String: Any]] else {
            enumerator.finish()
            return
        }

        let items = try await withThrowingTaskGroup(of: CascadeFSItem.self) { group in
            for entry in result {
                group.addTask {
                    try await CascadeFSItem(json: entry)
                }
            }
            var collected: [CascadeFSItem] = []
            for try await item in group {
                collected.append(item)
            }
            return collected
        }
        for item in items {
            dir.addItem(item)
        }
        enumerator.emit(items, isLastBatch: true)
    }
}

// MARK: - Open/Close

extension CascadeFSVolume: FSVolume.OpenCloseOperations {
    func openItem(_ item: FSItem, modes: FSVolume.OpenModes) async throws {
        // No special handling — the engine fetches content on read.
    }

    func closeItem(_ item: FSItem, modes: FSVolume.OpenModes) async throws {
        // No special handling.
    }
}

// MARK: - Read/Write

extension CascadeFSVolume: FSVolume.ReadWriteOperations {

    func read(
        from item: FSItem,
        at offset: off_t,
        length: Int,
        into buffer: FSMutableFileDataBuffer
    ) async throws -> Int {
        guard let cascadeItem = item as? CascadeFSItem else {
            throw posixError(.EIO)
        }

        guard offset >= 0 else {
            throw posixError(.EINVAL)
        }

        // Ask the engine for the file contents.
        guard let result = try await engine.send(
            method: "fetchContents",
            params: ["id": cascadeItem.cascadeID]
        ) as? [String: Any],
              let path = result["path"] as? String else {
            throw posixError(.EIO)
        }

        // Read the file from the engine's cache path.
        let fileURL = URL(fileURLWithPath: path)
        let fileData = try Data(contentsOf: fileURL, options: .mappedIfSafe)

        let requested = min(length, buffer.length)
        let available = fileData.count - Int(offset)
        let bytesToWrite = max(0, min(requested, available))

        guard bytesToWrite > 0 else {
            return 0
        }

        let startIndex = fileData.index(fileData.startIndex, offsetBy: Int(offset))
        let endIndex = fileData.index(startIndex, offsetBy: bytesToWrite)
        let slice = fileData[startIndex..<endIndex]

        buffer.withUnsafeMutableBytes { raw in
            _ = slice.withUnsafeBytes { src in
                memcpy(raw.baseAddress, src.baseAddress, bytesToWrite)
            }
        }

        return bytesToWrite
    }

    func write(
        contents: Data,
        to item: FSItem,
        at offset: off_t
    ) async throws -> Int {
        guard let cascadeItem = item as? CascadeFSItem else {
            throw posixError(.EIO)
        }

        // Encode the payload as base64 so it transports cleanly inside the
        // length-prefixed JSON wire protocol alongside the metadata fields.
        let payload = contents.base64EncodedString()

        // Write-back to the engine via protocol, carrying the actual payload.
        _ = try await engine.send(
            method: "writeContents",
            params: [
                "id": cascadeItem.cascadeID,
                "offset": Int(offset),
                "length": contents.count,
                "data": payload,
            ]
        )

        cascadeItem.attributes.size = UInt64(max(
            Int(cascadeItem.attributes.size),
            Int(offset) + contents.count
        ))
        cascadeItem.attributes.allocSize = cascadeItem.attributes.size

        return contents.count
    }
}

// MARK: - Extended Attributes

extension CascadeFSVolume: FSVolume.XattrOperations {

    func xattr(named name: FSFileName, of item: FSItem) async throws -> Data {
        guard let cascadeItem = item as? CascadeFSItem else {
            throw posixError(.EIO)
        }
        return cascadeItem.xattrs[name] ?? Data()
    }

    func setXattr(
        named name: FSFileName,
        to value: Data?,
        on item: FSItem,
        policy: FSVolume.SetXattrPolicy
    ) async throws {
        guard let cascadeItem = item as? CascadeFSItem else {
            throw posixError(.EIO)
        }
        cascadeItem.xattrs[name] = value
    }

    func xattrs(of item: FSItem) async throws -> [FSFileName] {
        guard let cascadeItem = item as? CascadeFSItem else {
            throw posixError(.EIO)
        }
        return Array(cascadeItem.xattrs.keys)
    }
}
