import Darwin
import FileProvider
import Foundation
import os

private let cascadeSocketPath = "\(NSHomeDirectory())/.config/cascade/fileprovider.sock"
private let rootIdentifier = "root"

private let actionLogger = Logger(subsystem: "com.cascade.fileprovider", category: "ActionHandler")

enum CascadeFileProviderError: LocalizedError {
    case invalidResponse(String)
    case socket(String)

    var errorDescription: String? {
        switch self {
        case .invalidResponse(let message):
            return "Invalid Cascade engine response: \(message)"
        case .socket(let message):
            return "Cascade socket error: \(message)"
        }
    }
}

// MARK: - Wire envelope

/// Structured error returned by the Rust File Provider bridge.
///
/// Mirrors `crates/presenter-fileprovider/src/wire.rs::RpcError`. `code`
/// is the machine-readable identifier the Swift side switches on;
/// `message` is a free-form human-readable string carried through for
/// logging only.
struct RpcError: Decodable {
    let code: String
    let message: String
}

/// Inbound RPC response envelope.
///
/// Mirrors `crates/presenter-fileprovider/src/wire.rs::RpcResponse`.
/// Exactly one of `result` or `error` is present in a well-formed
/// response; the decoder leaves both optional so callers can validate
/// the shape explicitly.
struct RpcResponse<Result: Decodable>: Decodable {
    let id: UInt32
    let result: Result?
    let error: RpcError?
}

// MARK: - Per-method result types

/// Wire shape of a File Provider item, mirroring
/// `crates/presenter-fileprovider/src/items.rs::FileProviderItem`.
///
/// Field names are snake_case to match the JSON the Rust side emits;
/// `JSONDecoder.keyDecodingStrategy = .convertFromSnakeCase` handles
/// the conversion centrally so this struct can keep camelCase property
/// names internally.
struct CascadeVfsItem: Decodable, Sendable {
    let id: String
    let parentID: String
    let filename: String
    let isDirectory: Bool
    let size: Int64?
    let contentType: String?
    let lastModified: Date?
    let cacheState: String

    enum CodingKeys: String, CodingKey {
        case id
        case parentID = "parent_id"
        case filename
        case isDirectory = "is_directory"
        case size
        case contentType = "content_type"
        case lastModified = "last_modified"
        case cacheState = "cache_state"
    }
}

/// Result of `getItem`.
struct GetItemResult: Decodable {
    let item: CascadeVfsItem
}

/// Result of `enumerateItems`.
struct EnumerateItemsResult: Decodable {
    let items: [CascadeVfsItem]
    let nextPage: String?

    enum CodingKeys: String, CodingKey {
        case items
        case nextPage = "next_page"
    }
}

/// Result of `fetchContents`.
struct FetchContentsResult: Decodable {
    let path: String
}

/// Result of `importDocument`.
struct ImportDocumentResult: Decodable {
    let item: CascadeVfsItem
}

/// Result of `createDirectory`.
struct CreateDirectoryResult: Decodable {
    let item: CascadeVfsItem
}

/// Result of `deleteItem` — the Rust side returns an empty object,
/// modelled here as a struct with no fields.
struct DeleteItemResult: Decodable {}

/// Result of `moveItem`.
struct MoveItemResult: Decodable {
    let item: CascadeVfsItem
}

// MARK: - Error mapping

/// Map a structured RPC error to an `NSFileProviderError`.
///
/// The mapping is the single source of truth for translating engine
/// failures into File Provider error codes the system understands.
/// Unknown codes fall through to `.serverUnreachable` with a logged
/// warning so a forward-compatible Rust release that adds a new code
/// still surfaces something visible to the user.
func makeError(from rpcError: RpcError, method: String, itemID: String?) -> NSFileProviderError {
    let userInfo: [String: Any] = [NSLocalizedDescriptionKey: rpcError.message]

    switch rpcError.code {
    case "not_found":
        actionLogger.error("RPC \(method, privacy: .public) for \(itemID ?? "-", privacy: .public) failed: not_found — \(rpcError.message, privacy: .public)")
        return NSFileProviderError(.noSuchItem, userInfo: userInfo)
    case "permission_denied":
        actionLogger.error("RPC \(method, privacy: .public) for \(itemID ?? "-", privacy: .public) failed: permission_denied — \(rpcError.message, privacy: .public)")
        return NSFileProviderError(.notAuthenticated, userInfo: userInfo)
    case "already_exists":
        actionLogger.error("RPC \(method, privacy: .public) for \(itemID ?? "-", privacy: .public) failed: already_exists — \(rpcError.message, privacy: .public)")
        return NSFileProviderError(.filenameCollision, userInfo: userInfo)
    case "internal":
        actionLogger.error("RPC \(method, privacy: .public) for \(itemID ?? "-", privacy: .public) failed: internal — \(rpcError.message, privacy: .public)")
        return NSFileProviderError(.serverUnreachable, userInfo: userInfo)
    case "not_supported":
        // `NSFileProviderError.Code` has no dedicated "operation not
        // supported" case; `.cannotSynchronize` is the documented
        // "there is no way to perform the requested operation"
        // outcome and is the best fit for a capability gap such as
        // cross-backend move. Avoids `.serverUnreachable`'s
        // misleading "the server is down" framing.
        actionLogger.error("RPC \(method, privacy: .public) for \(itemID ?? "-", privacy: .public) failed: not_supported — \(rpcError.message, privacy: .public)")
        return NSFileProviderError(.cannotSynchronize, userInfo: userInfo)
    default:
        actionLogger.warning("RPC \(method, privacy: .public) for \(itemID ?? "-", privacy: .public) returned unknown error code \(rpcError.code, privacy: .public) — \(rpcError.message, privacy: .public)")
        return NSFileProviderError(.serverUnreachable, userInfo: userInfo)
    }
}

/// Build the shared `JSONDecoder` used for every response.
///
/// `dateDecodingStrategy = .iso8601` matches the RFC 3339 strings the
/// Rust side emits via `chrono::DateTime::to_rfc3339()`. Key conversion
/// is per-struct via explicit `CodingKeys` rather than global
/// `convertFromSnakeCase` so any per-field renames stay obvious at the
/// definition site.
private func makeResponseDecoder() -> JSONDecoder {
    let decoder = JSONDecoder()
    decoder.dateDecodingStrategy = .iso8601
    return decoder
}

// MARK: - Engine client

actor EngineClient {
    private let socketPath: String
    private var nextID: UInt32 = 1
    private let decoder = makeResponseDecoder()

    init(socketPath: String = cascadeSocketPath) {
        self.socketPath = socketPath
    }

    /// Send an RPC and decode the response into `Result`.
    ///
    /// Throws an `NSFileProviderError` directly when the Rust side
    /// returns a structured `error`; throws a `CascadeFileProviderError`
    /// for transport or decode failures. The split lets the File
    /// Provider extension surface engine errors verbatim while still
    /// translating its own socket/decode issues at the boundary.
    func send<Result: Decodable>(
        method: String,
        params: [String: Any],
        itemID: String? = nil,
        as resultType: Result.Type = Result.self
    ) async throws -> Result {
        let requestID = nextID
        nextID += 1

        let request: [String: Any] = [
            "id": requestID,
            "method": method,
            "params": params,
        ]
        let requestBody = try JSONSerialization.data(withJSONObject: request)
        let responseBody = try await writeFrameAndReadResponse(body: requestBody)

        let envelope: RpcResponse<Result>
        do {
            envelope = try decoder.decode(RpcResponse<Result>.self, from: responseBody)
        } catch {
            throw CascadeFileProviderError.invalidResponse("could not decode response envelope: \(error.localizedDescription)")
        }

        if envelope.id != requestID {
            throw CascadeFileProviderError.invalidResponse("response id \(envelope.id) did not match request id \(requestID)")
        }
        if let rpcError = envelope.error {
            throw makeError(from: rpcError, method: method, itemID: itemID)
        }
        guard let result = envelope.result else {
            throw CascadeFileProviderError.invalidResponse("response carried neither result nor error")
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

// MARK: - Action handler

final class ActionHandler {
    private let engine: EngineClient

    init(engine: EngineClient = EngineClient()) {
        self.engine = engine
    }

    func item(for identifier: NSFileProviderItemIdentifier) async throws -> FileProviderItem {
        let id = engineID(for: identifier)
        let result: GetItemResult = try await engine.send(
            method: "getItem",
            params: ["id": id],
            itemID: id
        )
        return FileProviderItem(item: result.item)
    }

    func enumerateItems(parentIdentifier: NSFileProviderItemIdentifier, page: Data) async throws -> [NSFileProviderItem] {
        let pageString = page.isEmpty ? nil : page.base64EncodedString()
        let parentID = engineID(for: parentIdentifier)
        var params: [String: Any] = ["parent_id": parentID]
        if let pageString { params["page"] = pageString }

        let result: EnumerateItemsResult = try await engine.send(
            method: "enumerateItems",
            params: params,
            itemID: parentID
        )
        return result.items.map(FileProviderItem.init(item:))
    }

    func fetchContents(for identifier: NSFileProviderItemIdentifier) async throws -> URL {
        let id = engineID(for: identifier)
        let result: FetchContentsResult = try await engine.send(
            method: "fetchContents",
            params: ["id": id],
            itemID: id
        )
        return URL(fileURLWithPath: result.path)
    }

    func uploadChangedItem(identifier: NSFileProviderItemIdentifier, fileURL: URL) async throws {
        let id = engineID(for: identifier)
        let _: ImportDocumentResult = try await engine.send(
            method: "importDocument",
            params: [
                "source_url": fileURL.path,
                "parent_id": rootIdentifier,
                "existing_id": id,
            ],
            itemID: id
        )
    }

    func createDirectory(named name: String, parentIdentifier: NSFileProviderItemIdentifier) async throws -> FileProviderItem {
        let parentID = engineID(for: parentIdentifier)
        let result: CreateDirectoryResult = try await engine.send(
            method: "createDirectory",
            params: [
                "name": name,
                "parent_id": parentID,
            ],
            itemID: parentID
        )
        return FileProviderItem(item: result.item)
    }

    func deleteItem(identifier: NSFileProviderItemIdentifier) async throws {
        let id = engineID(for: identifier)
        let _: DeleteItemResult = try await engine.send(
            method: "deleteItem",
            params: ["id": id],
            itemID: id
        )
    }

    func moveItem(identifier: NSFileProviderItemIdentifier, newParentIdentifier: NSFileProviderItemIdentifier, newName: String) async throws -> FileProviderItem {
        let id = engineID(for: identifier)
        let result: MoveItemResult = try await engine.send(
            method: "moveItem",
            params: [
                "id": id,
                "new_parent_id": engineID(for: newParentIdentifier),
                "new_name": newName,
            ],
            itemID: id
        )
        return FileProviderItem(item: result.item)
    }

    /// Replicated File Provider create-item entry point.
    ///
    /// The system calls this for both new files (with `contents` set
    /// to a sandboxed source URL) and new folders (with `contents`
    /// nil). The Rust bridge already exposes `importDocument` and
    /// `createDirectory`; we choose between them on whether the
    /// system handed us a content URL.
    func createItem(
        template: NSFileProviderItem,
        contents url: URL?
    ) async throws -> FileProviderItem {
        let parent = template.parentItemIdentifier
        let parentID = engineID(for: parent)
        let name = template.filename

        if let url {
            let result: ImportDocumentResult = try await engine.send(
                method: "importDocument",
                params: [
                    "source_url": url.path,
                    "parent_id": parentID,
                    "name": name,
                ],
                itemID: parentID
            )
            return FileProviderItem(item: result.item)
        }

        return try await createDirectory(named: name, parentIdentifier: parent)
    }

    /// Replicated File Provider modify-item entry point.
    ///
    /// The Rust bridge does not yet expose a single `modifyItem` RPC;
    /// instead it has `moveItem` for renames/reparents and
    /// `importDocument` for content updates. We decompose the
    /// `changedFields` set into the bridge calls that match, applying
    /// them in a deterministic order so the engine sees the same end
    /// state regardless of which fields the system bundled together.
    func modifyItem(
        item: NSFileProviderItem,
        changedFields: NSFileProviderItemFields,
        newContents: URL?
    ) async throws -> FileProviderItem {
        let identifier = item.itemIdentifier
        var latest: FileProviderItem?

        let renameOrReparent = changedFields.contains(.filename) || changedFields.contains(.parentItemIdentifier)
        if renameOrReparent {
            latest = try await moveItem(
                identifier: identifier,
                newParentIdentifier: item.parentItemIdentifier,
                newName: item.filename
            )
        }

        if changedFields.contains(.contents), let url = newContents {
            let id = engineID(for: identifier)
            let _: ImportDocumentResult = try await engine.send(
                method: "importDocument",
                params: [
                    "source_url": url.path,
                    "parent_id": engineID(for: item.parentItemIdentifier),
                    "existing_id": id,
                ],
                itemID: id
            )
            latest = try await self.item(for: identifier)
        }

        if let latest {
            return latest
        }

        // TODO(fileprovider): the Rust bridge needs an explicit
        //   `setAttributes` RPC to round-trip changes to capabilities,
        //   favourite rank, or last-used date. Until then we re-read
        //   the item so the system gets a fresh metadata snapshot
        //   even when no field we recognise was actually changed.
        return try await self.item(for: identifier)
    }

    private func engineID(for identifier: NSFileProviderItemIdentifier) -> String {
        identifier == .rootContainer ? rootIdentifier : identifier.rawValue
    }
}
