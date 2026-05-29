//
//  CascadeFSKitExtension.swift
//  CascadeFSKit
//
//  Entry point for the Cascade FSKit extension.
//  Conforms to UnaryFileSystemExtension so FSKit discovers our filesystem.
//  Requires macOS 15.4+ (Sequoia) with FSKit framework.
//

import Foundation
import FSKit

@main
struct CascadeFSKitExtension: UnaryFileSystemExtension {
    var fileSystem: FSUnaryFileSystem & FSUnaryFileSystemOperations {
        CascadeFileSystem()
    }
}
