import FileProvider
import XCTest

/// On-Simulator e2e for the File Provider extension: registers a domain through
/// `NSFileProviderManager`, enumerates the root through the OS-routed enumerator
/// (which the OS proxies to the installed extension process), and asserts the
/// `local` mount point surfaces — proving the OS successfully instantiated our
/// extension and routed enumeration to it. Runs ad-hoc signed (no Apple
/// identity); the `local` mount point is structural, so this needs no shared
/// storage or seeded file.
final class FileProviderRegistrationE2ETests: XCTestCase {

    private final class EnumerationObserver: NSObject, NSFileProviderEnumerationObserver {
        var items: [NSFileProviderItem] = []
        let onFinish: (Error?) -> Void

        init(onFinish: @escaping (Error?) -> Void) { self.onFinish = onFinish }

        func didEnumerate(_ updatedItems: [NSFileProviderItem]) { items.append(contentsOf: updatedItems) }
        func finishEnumerating(upTo page: NSFileProviderPage?) { onFinish(nil) }
        func finishEnumeratingWithError(_ error: Error) { onFinish(error) }
    }

    func testRegistersAndEnumeratesTheLocalMountViaOS() async throws {
        let domain = NSFileProviderDomain(
            identifier: "co.uk.mearman.cascade.itest",
            displayName: "Cascade ITest"
        )
        // Register the domain; the OS associates it with the app's embedded File
        // Provider extension and (lazily) launches the extension process.
        try await withCheckedThrowingContinuation { (cont: CheckedContinuation<Void, Error>) in
            NSFileProviderManager.add(domain) { error in
                if let error { cont.resume(throwing: error) } else { cont.resume() }
            }
        }
        defer { NSFileProviderManager.remove(domain) { _ in } }

        guard let manager = NSFileProviderManager(for: domain) else {
            XCTFail("no manager for the registered domain")
            return
        }

        // Give the OS a moment to activate the domain before enumerating.
        try await Task.sleep(nanoseconds: 3_000_000_000)

        let expectation = self.expectation(description: "root enumeration completed")
        let observer = EnumerationObserver { _ in expectation.fulfill() }
        let enumerator = try manager.enumerator(for: .rootContainer)
        enumerator.enumerateItems(for: observer, startingAt: .initialPageSortedByName)

        await fulfillment(of: [expectation], timeout: 90)

        let names = observer.items.map(\.filename)
        XCTAssertTrue(
            names.contains("local"),
            "the OS enumerated the local mount point through the extension: \(names)"
        )
    }
}
