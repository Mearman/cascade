//
//  EngineClient.swift
//  CascadeFSKit
//
//  Communicates with the Cascade Rust engine over a Unix domain socket.
//  Uses the same length-prefixed JSON protocol as the File Provider bridge:
//  4-byte big-endian length prefix + JSON body.
//
//  This is the Swift counterpart to the Rust FSKitBridge — they speak the
//  same wire protocol so either side can initiate requests.
//

import Darwin
import Foundation
import os

private let cascadeSocketPath: String = {
    if let home = getenv("HOME") {
        return String(cString: home) + "/" + CascadeConstants.socketPathSuffix
    }
    return "/tmp/cascade-fskit.sock"
}()

enum CascadeEngineError: LocalizedError {
    case engineError(String)
    case invalidResponse(String)
    case socket(String)

    var errorDescription: String? {
        switch self {
        case .engineError(let message):
            return message
        case .invalidResponse(let message):
            return "Invalid Cascade engine response: \(message)"
        case .socket(let message):
            return "Cascade socket error: \(message)"
        }
    }
}

actor EngineClient {
    private let socketPath: String
    private var nextID: UInt32 = 1

    init(socketPath: String = cascadeSocketPath) {
        self.socketPath = socketPath
    }

    func send(method: String, params: [String: Any]) async throws -> Any {
        let requestID = nextID
        nextID += 1

        let request: [String: Any] = [
            "id": requestID,
            "method": method,
            "params": params,
        ]
        let requestBody = try JSONSerialization.data(withJSONObject: request)
        let responseBody = try await writeFrameAndReadResponse(body: requestBody)
        let decoded = try JSONSerialization.jsonObject(with: responseBody)

        guard let response = decoded as? [String: Any] else {
            throw CascadeEngineError.invalidResponse("top-level response was not an object")
        }
        if let responseID = response["id"] as? NSNumber, responseID.uint32Value != requestID {
            throw CascadeEngineError.invalidResponse(
                "response id \(responseID) did not match request id \(requestID)")
        }
        if let error = response["error"] as? String {
            throw CascadeEngineError.engineError(error)
        }
        guard let result = response["result"] else {
            throw CascadeEngineError.invalidResponse("missing result")
        }
        return result
    }

    private func writeFrameAndReadResponse(body: Data) async throws -> Data {
        try await Task.detached(priority: .userInitiated) { [socketPath] in
            let fd = socket(AF_UNIX, SOCK_STREAM, 0)
            guard fd >= 0 else {
                throw CascadeEngineError.socket(String(cString: strerror(errno)))
            }
            defer { close(fd) }

            var address = sockaddr_un()
            address.sun_family = sa_family_t(AF_UNIX)
            let encodedPath = Array(socketPath.utf8)
            let capacity = MemoryLayout.size(ofValue: address.sun_path)
            guard encodedPath.count < capacity else {
                throw CascadeEngineError.socket("socket path is too long")
            }
            withUnsafeMutableBytes(of: &address.sun_path) { buffer in
                for (index, byte) in encodedPath.enumerated() {
                    buffer[index] = byte
                }
            }

            let addressSize = socklen_t(MemoryLayout<sockaddr_un>.size)
            let connected = withUnsafePointer(to: &address) { pointer in
                pointer.withMemoryRebound(to: sockaddr.self, capacity: 1) { sockaddrPointer in
                    connect(fd, sockaddrPointer, addressSize)
                }
            }
            guard connected == 0 else {
                throw CascadeEngineError.socket(String(cString: strerror(errno)))
            }

            var length = UInt32(body.count).bigEndian
            try Self.writeAll(fd: fd, data: Data(bytes: &length, count: MemoryLayout<UInt32>.size))
            try Self.writeAll(fd: fd, data: body)

            let lengthData = try Self.readExact(fd: fd, byteCount: MemoryLayout<UInt32>.size)
            let responseLength = lengthData.withUnsafeBytes { bytes in
                bytes.load(as: UInt32.self).bigEndian
            }
            return try Self.readExact(fd: fd, byteCount: Int(responseLength))
        }.value
    }

    private static func writeAll(fd: Int32, data: Data) throws {
        try data.withUnsafeBytes { rawBuffer in
            guard let baseAddress = rawBuffer.baseAddress else { return }
            var written = 0
            while written < data.count {
                let result = Darwin.write(fd, baseAddress.advanced(by: written), data.count - written)
                guard result > 0 else {
                    throw CascadeEngineError.socket(String(cString: strerror(errno)))
                }
                written += result
            }
        }
    }

    private static func readExact(fd: Int32, byteCount: Int) throws -> Data {
        var data = Data(count: byteCount)
        try data.withUnsafeMutableBytes { rawBuffer in
            guard let baseAddress = rawBuffer.baseAddress else { return }
            var readCount = 0
            while readCount < byteCount {
                let result = Darwin.read(fd, baseAddress.advanced(by: readCount), byteCount - readCount)
                guard result > 0 else {
                    throw CascadeEngineError.socket("unexpected end of stream")
                }
                readCount += result
            }
        }
        return data
    }
}
