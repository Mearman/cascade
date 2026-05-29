//
//  CascadeFSVolume.swift
//  CascadeFSKit
//
//  The FSVolume subclass that represents a mounted Cascade filesystem.
//  Handles volume lifecycle (activate, mount, unmount, deactivate) and
//  delegates file operations to the Cascade engine via EngineClient.
//

import Foundation
import FSKit
import os

final class CascadeFSVolume: FSVolume {

    private let resource: FSResource
    private let logger = Logger(subsystem: "com.cascade.fskit", category: "CascadeFSVolume")
    let engine: EngineClient

    private let root: CascadeFSItem = {
        let item = CascadeFSItem(cascadeID: "root", name: FSFileName(string: "/"))
        item.attributes.parentID = .parentOfRoot
        item.attributes.fileID = .rootDirectory
        item.attributes.uid = 0
        item.attributes.gid = 0
        item.attributes.linkCount = 1
        item.attributes.type = .directory
        item.attributes.mode = UInt32(S_IFDIR | 0o755)
        item.attributes.allocSize = 1
        item.attributes.size = 1
        return item
    }()

    init(resource: FSResource) {
        self.resource = resource
        self.engine = EngineClient()

        super.init(
            volumeID: FSVolume.Identifier(uuid: CascadeConstants.volumeIdentifier),
            volumeName: FSFileName(string: CascadeConstants.fileSystemName)
        )
    }
}
