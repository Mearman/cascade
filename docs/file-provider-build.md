# macOS File Provider extension build

## Status

**broken-as-found** — the Swift sources do not compile on any current macOS SDK. The extension is written against `NSFileProviderExtension`, an API the FileProvider framework marks `API_UNAVAILABLE(macos)`. macOS has only ever supported `NSFileProviderReplicatedExtension`. The Rust side (`crates/presenter-fileprovider`) builds cleanly; the bridge protocol is in place. The Swift target needs to be rewritten against the supported API before it can build, ship, or load.

The roadmap in the top-level `CLAUDE.md` lists "macOS File Provider presenter (Swift extension)" at v5 and the architecture diagram labels it "File Provider planned — see roadmap", so this state matches the documented project status: the Rust scaffold is real, the Swift target is a draft.

## Build environment

This was verified on:

- macOS 26.1 (build 25B78)
- Xcode 26.2 (build 17C52)
- Apple Swift 6.2.3 (`swiftlang-6.2.3.3.21`), targeting `arm64-apple-macosx26.0`
- Rust 1.95.0 (Homebrew), workspace edition 2024
- Sources at base commit `e57365f7`

## Build steps

The Rust presenter builds with the workspace:

```bash
cargo build --release -p cascade-presenter-fileprovider
```

That succeeds on this host. The Swift target does not. The top-level `CLAUDE.md` documents the build as `cargo build --release && cd swift/CascadeFileProvider && xcodebuild`, but `swift/CascadeFileProvider/` contains only a `Package.swift` — there is no `.xcodeproj`, no `Info.plist`, no entitlements file, and no app or extension target wrapper. The supported invocations against the current sources are:

```bash
cd swift/CascadeFileProvider
swift build -c release
# or, equivalently
xcodebuild -scheme CascadeFileProvider -configuration Release -destination "platform=macOS" build
```

Both fail with the same compiler errors (see below). The bare `xcodebuild` form in `CLAUDE.md` fails earlier with `error: Building a Swift package requires that a destination is provided`; that one is a documentation defect rather than a code defect.

## Output artefacts

None produced. A successful build of an `NSFileProviderReplicatedExtension` target would produce a `.appex` bundle inside a containing host app, which is then registered with the system via `NSFileProviderManager.add(_:completionHandler:)` from the host app at runtime. The current sources do not configure either an extension bundle or a containing app — `Package.swift` declares a plain library product.

## Rust to Swift bridge

There is no C ABI / FFI surface. The bridge is a Unix domain socket using Cascade's length-prefixed JSON protocol (`cascade_engine::protocol::{Request, Response}` with `encode_message`). Source: `crates/presenter-fileprovider/src/bridge.rs`.

The default socket path resolves to `$HOME/.config/cascade/fileprovider.sock`. The Rust `FileProviderPresenter` issues these methods to Swift:

- `upsertItem({ item })`
- `deleteItem({ id })`
- `updateState({ id, state })`
- `fetchContents({ id })` returning `{ path }`
- `evictItem({ id })`
- `startPresenter({ mount_point })`
- `stopPresenter({})`

The Swift `ActionHandler` (`swift/CascadeFileProvider/ActionHandler.swift`) opens the same socket and issues the inverse methods to the engine:

- `getItem({ id })`
- `enumerateItems({ parent_id, page? })`
- `fetchContents({ id })`
- `importDocument({ source_url, parent_id, existing_id })`
- `createDirectory({ name, parent_id })`
- `deleteItem({ id })`
- `moveItem({ id, new_parent_id, new_name })`

These two sets do not currently match — the Swift extension calls methods like `getItem` and `enumerateItems` that the Rust presenter does not expose, and the Rust presenter pushes `upsertItem` / `updateState` notifications that the Swift side has no handler for. Bringing them into agreement is itself a piece of work, separate from the API rewrite below.

## Known issues

Six distinct compile errors in two source files, all rooted in the same problem: the extension targets the wrong API.

1. `CascadeFileProvider.swift:5` — `'NSFileProviderExtension' is unavailable in macOS`. The class is the base type for the entire extension; it has never been available on macOS. The Apple SDK header (`FileProvider.framework/Headers/NSFileProviderExtension.h`) marks the class `API_UNAVAILABLE(macos)`.
2. `CascadeFileProvider.swift:8` — `method does not override any method from its superclass` for `item(for:completionHandler:)`. Cascade falls out of point 1.
3. `CascadeFileProvider.swift:21` — `cannot find 'storageURL' in scope`. `storageURL` is an instance property on `NSFileProviderExtension` only; the replicated API uses domain manifests and per-document URLs.
4. `CascadeFileProvider.swift:36` and `:38` — `placeholderURL(for:)` and `writePlaceholder(at:withMetadata:)` are `API_UNAVAILABLE(macos)`. The replicated API stores its metadata in the system-managed database; the extension never writes placeholders.
5. `FileProviderItem.swift:86` — `cannot override 'typeIdentifier' which has been marked unavailable`. The replicated API uses `contentType: UTType` instead.

What to do about it (follow-up, not addressed in this commit):

- Rewrite `CascadeFileProvider.swift` as an `NSFileProviderReplicatedExtension` subclass. The required methods become `item(for:request:completionHandler:)`, `fetchContents(for:version:request:completionHandler:)`, `createItem(basedOn:fields:contents:options:request:completionHandler:)`, `modifyItem(...)`, `deleteItem(identifier:baseVersion:options:request:completionHandler:)`, `enumerator(for:request:)`, plus optional `materializedItemsDidChange` and `pendingItemsDidChange`. The presenter starts and stops via `NSFileProviderManager.add(_:completionHandler:)` from a containing host app rather than via an `NSFileProviderExtension` lifecycle.
- Replace `FileProviderItem.typeIdentifier: String` with `contentType: UTType`. `documentSize` becomes `NSNumber?` of the file size as before. Add `itemVersion: NSFileProviderItemVersion` (required by the replicated API to detect content and metadata changes).
- Decide and document how the Swift extension is packaged. Options: (a) standalone Xcode project under `swift/CascadeFileProvider.xcodeproj` with an `.appex` target embedded in a tiny host `.app`; (b) generate the project from `Package.swift` via XcodeGen / Tuist; (c) ship the extension only as part of a FSKit-equivalent host the daemon already builds. The current `Package.swift` produces a plain library and cannot register the extension with the system.
- Provide an `Info.plist` and entitlements for the extension. At minimum `NSExtensionPointIdentifier = com.apple.fileprovider-nonui` and `NSExtension.NSExtensionPrincipalClass`. Code signing identity is a separate decision (developer ID, team ID, App Group identifier for the shared container with the host app).
- Reconcile the bridge method names. Either rename the Swift methods to match the Rust presenter's outbound calls, or add the inverse methods on the Rust side. Pick one direction in the contract and apply it consistently.
- Decide a deployment target. `Package.swift` currently says `.macOS(.v11)` but `NSFileProviderReplicatedExtension` is macOS 11+ in name and macOS 12+ in practice for most modern affordances; FSKit (the project's preferred presenter) needs 15.4. A single floor of 12.0 for File Provider is reasonable and does not conflict with the FSKit target.
- Update the top-level `CLAUDE.md` build instruction. The current line `cargo build --release && cd swift/CascadeFileProvider && xcodebuild` cannot succeed against any state of the sources — there is no Xcode project, and `xcodebuild` against a Swift package requires `-destination`. Once the extension actually builds, replace with the working invocation.

## Sources

- Apple, *NSFileProviderReplicatedExtension* — <https://developer.apple.com/documentation/fileprovider/nsfileproviderreplicatedextension> (Wayback: <https://web.archive.org/web/2026/https://developer.apple.com/documentation/fileprovider/nsfileproviderreplicatedextension>)
- Apple, *NSFileProviderExtension* (marked unavailable on macOS) — <https://developer.apple.com/documentation/fileprovider/nsfileproviderextension> (Wayback: <https://web.archive.org/web/2026/https://developer.apple.com/documentation/fileprovider/nsfileproviderextension>)
- Apple, *NSFileProviderItem* — <https://developer.apple.com/documentation/fileprovider/nsfileprovideritem> (Wayback: <https://web.archive.org/web/2026/https://developer.apple.com/documentation/fileprovider/nsfileprovideritem>)
- SDK headers shipped with Xcode 26.2 confirm `API_UNAVAILABLE(macos)` on `NSFileProviderExtension`, `placeholderURL(for:)`, `writePlaceholder(at:withMetadata:)`, and the deprecated `typeIdentifier` property on `NSFileProviderItem`. Path: `/Applications/Xcode-26.2.0.app/Contents/Developer/Platforms/MacOSX.platform/Developer/SDKs/MacOSX26.2.sdk/System/Library/Frameworks/FileProvider.framework/Headers/`.
