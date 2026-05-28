import Darwin
import FileProvider
import Foundation

private let cascadeSocketPath = "\(NSHomeDirectory())/.config/cascade/fileprovider.sock"
private let rootIdentifier = "root"

enum CascadeFileProviderError: LocalizedError {
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
            throw CascadeFileProviderError.invalidResponse("top-level response was not an object")
        }
        if let responseID = response["id"] as? NSNumber, responseID.uint32Value != requestID {
            throw CascadeFileProviderError.invalidResponse("response id \(responseID) did not match request id \(requestID)")
        }
        if let error = response["error"] as? String {
            throw CascadeFileProviderError.engineError(error)
        }
        guard let result = response["result"] else {
            throw CascadeFileProviderError.invalidResponse("missing result")
        }
        return result
    }

    private func writeFrameAndReadResponse(body: Data) async throws -> Data {
        try await Task.detached(priority: .userInitiated) { [socketPath] in
            let fd = socket(AF_UNIX, SOCK_STREAM, 0)
            guard fd >= 0 else {
                throw CascadeFileProviderError.socket(String(cString: strerror(errno)))
            }
            defer { close(fd) }

            var address = sockaddr_un()
            address.sun_family = sa_family_t(AF_UNIX)
            let encodedPath = Array(socketPath.utf8)
            let capacity = MemoryLayout.size(ofValue: address.sun_path)
            guard encodedPath.count < capacity else {
                throw CascadeFileProviderError.socket("socket path is too long")
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
                throw CascadeFileProviderError.socket(String(cString: strerror(errno)))
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
                    throw CascadeFileProviderError.socket(String(cString: strerror(errno)))
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
                    throw CascadeFileProviderError.socket("unexpected end of stream")
                }
                readCount += result
            }
        }
        return data
    }
}

final class ActionHandler {
    private let engine: EngineClient

    init(engine: EngineClient = EngineClient()) {
        self.engine = engine
    }

    func item(for identifier: NSFileProviderItemIdentifier) async throws -> FileProviderItem {
        let id = engineID(for: identifier)
        let result = try await engine.send(method: "getItem", params: ["id": id])
        guard let json = result as? [String: Any] else {
            throw CascadeFileProviderError.invalidResponse("getItem result was not an object")
        }
        return try FileProviderItem(item: CascadeVfsItem(json: json))
    }

    func enumerateItems(parentIdentifier: NSFileProviderItemIdentifier, page: Data) async throws -> [NSFileProviderItem] {
        let pageString = page.isEmpty ? nil : page.base64EncodedString()
        var params: [String: Any] = ["parent_id": engineID(for: parentIdentifier)]
        if let pageString { params["page"] = pageString }

        let result = try await engine.send(method: "enumerateItems", params: params)
        guard let array = result as? [[String: Any]] else {
            throw CascadeFileProviderError.invalidResponse("enumerateItems result was not an item array")
        }
        return try array.map { try FileProviderItem(item: CascadeVfsItem(json: $0)) }
    }

    func fetchContents(for identifier: NSFileProviderItemIdentifier) async throws -> URL {
        let result = try await engine.send(method: "fetchContents", params: ["id": engineID(for: identifier)])
        guard let json = result as? [String: Any], let path = json["path"] as? String else {
            throw CascadeFileProviderError.invalidResponse("fetchContents result did not contain a path")
        }
        return URL(fileURLWithPath: path)
    }

    func uploadChangedItem(identifier: NSFileProviderItemIdentifier, fileURL: URL) async throws {
        _ = try await engine.send(method: "importDocument", params: [
            "source_url": fileURL.path,
            "parent_id": rootIdentifier,
            "existing_id": engineID(for: identifier),
        ])
    }

    func createDirectory(named name: String, parentIdentifier: NSFileProviderItemIdentifier) async throws -> FileProviderItem {
        let result = try await engine.send(method: "createDirectory", params: [
            "name": name,
            "parent_id": engineID(for: parentIdentifier),
        ])
        guard let json = result as? [String: Any] else {
            throw CascadeFileProviderError.invalidResponse("createDirectory result was not an object")
        }
        return try FileProviderItem(item: CascadeVfsItem(json: json))
    }

    func deleteItem(identifier: NSFileProviderItemIdentifier) async throws {
        _ = try await engine.send(method: "deleteItem", params: ["id": engineID(for: identifier)])
    }

    func moveItem(identifier: NSFileProviderItemIdentifier, newParentIdentifier: NSFileProviderItemIdentifier, newName: String) async throws -> FileProviderItem {
        let result = try await engine.send(method: "moveItem", params: [
            "id": engineID(for: identifier),
            "new_parent_id": engineID(for: newParentIdentifier),
            "new_name": newName,
        ])
        guard let json = result as? [String: Any] else {
            throw CascadeFileProviderError.invalidResponse("moveItem result was not an object")
        }
        return try FileProviderItem(item: CascadeVfsItem(json: json))
    }

    private func engineID(for identifier: NSFileProviderItemIdentifier) -> String {
        identifier == .rootContainer ? rootIdentifier : identifier.rawValue
    }
}
