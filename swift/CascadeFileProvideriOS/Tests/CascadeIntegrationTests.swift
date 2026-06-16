import XCTest

/// Integration test: drives the REAL CascadeNode through the UniFFI bindings,
/// exercising the in-process FFI -> engine -> local-backend stack on the iOS
/// simulator. Unlike the host-side Rust tests, this validates the FFI marshalling
/// and static-library loading on the actual iOS target, and the local backend's
/// live `list_children` path end to end. The bindings (`cascade_ffi.swift`) are
/// compiled into this test target, so the types are referenced directly.
final class CascadeIntegrationTests: XCTestCase {
    /// A fresh writable config dir for each test, so they never share state.
    private func makeConfigDir() throws -> String {
        let url = FileManager.default.temporaryDirectory
            .appendingPathComponent("cascade-it-\(UUID().uuidString)", isDirectory: true)
        try FileManager.default.createDirectory(at: url, withIntermediateDirectories: true)
        return url.path
    }

    func testListAndReadThroughTheRealNode() async throws {
        let configDir = try makeConfigDir()
        let node = try await CascadeNode(configDir: configDir)
        try await node.start()

        // Seed a file into the local backend's `files` root, then list and read
        // it back through the FFI — the same path the File Provider enumerator
        // and content fetcher take.
        let filesDir = (configDir as NSString).appendingPathComponent("files")
        let payload = "hello from the integration test".data(using: .utf8)!
        try payload.write(to: URL(fileURLWithPath: (filesDir as NSString).appendingPathComponent("hello.txt")))

        let entries = try await node.listDir(path: "/local")
        let hello = try XCTUnwrap(entries.first { $0.name == "hello.txt" }, "hello.txt listed: \(entries)")
        XCTAssertFalse(hello.isDir, "a file is not a directory")

        let read = try await node.readFile(path: "/local/hello.txt")
        XCTAssertEqual(read, payload, "read returns the seeded contents")
    }

    func testReadMissingFileThrows() async throws {
        let configDir = try makeConfigDir()
        let node = try await CascadeNode(configDir: configDir)
        try await node.start()

        XCTAssertThrowsError(try await node.readFile(path: "/local/absent.txt")) { error in
            // The FFI maps an engine error onto CascadeException; the read of a
            // missing file must surface an error, not an empty body or a crash.
            XCTAssertTrue(error is CascadeException, "missing file is a CascadeException: \(error)")
        }
    }
}
