//
//  CascadeConstants.swift
//  CascadeFSKit
//
//  Shared constants for the FSKit extension.
//  These UUIDs identify the container and volume to macOS.
//  In production they would be generated once and stored in the Cascade config.
//

import Foundation

enum CascadeConstants {
    static let containerIdentifier = UUID(uuidString: "A1B2C3D4-E5F6-7890-ABCD-EF1234567890")!
    static let volumeIdentifier = UUID(uuidString: "F0E1D2C3-B4A5-6789-0ABC-DEF123456789")!
    static let fileSystemName = "Cascade"
    static let fileSystemShortName = "Cascade"
    static let socketPathSuffix = ".config/cascade/fskit.sock"
}
