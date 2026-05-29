//
//  CascadeFileSystem.swift
//  CascadeFSKit
//
//  The FSUnaryFileSystem subclass that probes and loads resources.
//  When macOS asks "can you handle this block device / URL?", we say yes
//  and return a CascadeFSVolume that handles actual filesystem operations.
//

import Foundation
import FSKit
import os

final class CascadeFileSystem: FSUnaryFileSystem, FSUnaryFileSystemOperations {

    private let logger = Logger(subsystem: "com.cascade.fskit", category: "CascadeFileSystem")

    func probeResource(
        resource: FSResource,
        replyHandler: @escaping (FSProbeResult?, (any Error)?) -> Void
    ) {
        logger.debug("probeResource: \(resource, privacy: .public)")

        // Cascade always accepts — we don't need a real block device.
        // The container UUID ties this probe to a specific Cascade mount.
        replyHandler(
            FSProbeResult.usable(
                name: "Cascade",
                containerID: FSContainerIdentifier(uuid: CascadeConstants.containerIdentifier)
            ),
            nil
        )
    }

    func loadResource(
        resource: FSResource,
        options: FSTaskOptions,
        replyHandler: @escaping (FSVolume?, (any Error)?) -> Void
    ) {
        logger.debug("loadResource: \(resource, privacy: .public)")

        let volume = CascadeFSVolume(resource: resource)
        replyHandler(volume, nil)
    }

    func unloadResource(
        resource: FSResource,
        options: FSTaskOptions,
        replyHandler reply: @escaping ((any Error)?) -> Void
    ) {
        logger.debug("unloadResource: \(resource, privacy: .public)")
        reply(nil)
    }

    func didFinishLoading() {
        logger.debug("Cascade FSKit extension finished loading")
    }
}
