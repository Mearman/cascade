import FileProvider
import XCTest

/// Hostless logic tests for the path <-> identifier mapping. Compiles
/// standalone (FileProviderPath.swift + this file) with no dependency on the
/// Rust FFI or a host app, so they run on the simulator in CI.
final class FileProviderPathTests: XCTestCase {
    func testRootPathMapsToRootContainer() {
        XCTAssertEqual(FileProviderPath.identifier(forPath: "/"), .rootContainer)
        XCTAssertEqual(FileProviderPath.identifier(forPath: ""), .rootContainer)
    }

    func testNonRootPathCarriedVerbatim() {
        XCTAssertEqual(
            FileProviderPath.identifier(forPath: "/local/Documents/report.txt").rawValue,
            "/local/Documents/report.txt"
        )
    }

    func testRootContainerRoundTripsToRoot() {
        XCTAssertEqual(FileProviderPath.path(forIdentifier: .rootContainer), "/")
    }

    func testIdentifierRoundTrips() {
        let path = "/local/Documents/report.txt"
        XCTAssertEqual(FileProviderPath.path(forIdentifier: FileProviderPath.identifier(forPath: path)), path)
    }

    func testParentOfRootIsRoot() {
        XCTAssertEqual(FileProviderPath.parent(of: "/"), "/")
    }

    func testParentOfTopLevelIsRoot() {
        XCTAssertEqual(FileProviderPath.parent(of: "/local"), "/")
    }

    func testParentStripsLastSegment() {
        XCTAssertEqual(FileProviderPath.parent(of: "/local/Documents/report.txt"), "/local/Documents")
    }

    func testNameOfRootIsRoot() {
        XCTAssertEqual(FileProviderPath.name(of: "/"), "/")
    }

    func testNameReturnsLastSegment() {
        XCTAssertEqual(FileProviderPath.name(of: "/local/Documents/report.txt"), "report.txt")
    }
}
