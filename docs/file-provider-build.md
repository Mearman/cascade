# macOS File Provider extension build

## Status

**partial** — the Xcode project builds clean on the canonical host and produces both a `.app` and an embedded `.appex`. The Rust ↔ Swift bridge protocol matches between sides at the JSON-method level but has not been exercised end-to-end; loading the extension into a real Finder session and watching it round-trip a directory listing is still a follow-up. The bridge today supports lookup, enumeration, fetch, import, create-directory, delete, and move; the replicated File Provider API also wants a sync-anchor enumeration cursor and an attribute round-trip, neither of which the Rust side has yet.

The roadmap in the top-level `README.md` (aliased as `CLAUDE.md` and `AGENTS.md`) lists "macOS File Provider presenter (Swift extension)" at v5. With the rewrite in place the Swift target is no longer broken-as-found; it is a buildable scaffold that needs the remaining bridge methods and a real signing identity before it can be shipped.

## Build environment

Verified on:

- macOS 26.1 (build 25B78)
- Xcode 26.2 (build 17C52)
- Apple Swift 6.2.3 (`swiftlang-6.2.3.3.21`), targeting `arm64-apple-macosx12.0` for both the host app and the extension
- Rust 1.95.0 (Homebrew), workspace edition 2024

## Build steps

The Rust presenter builds with the workspace:

```bash
cargo build --release -p cascade-presenter-fileprovider
```

The Swift target builds via the standalone Xcode project at `swift/CascadeFileProvider.xcodeproj`:

```bash
xcodebuild \
    -project swift/CascadeFileProvider.xcodeproj \
    -scheme CascadeFileProviderHost \
    -configuration Debug \
    -destination "platform=macOS" \
    build
```

`Release` works identically — substitute `-configuration Release`.

The scheme `CascadeFileProviderHost` builds the host application; the extension target `CascadeFileProvider` is an explicit dependency of the host and is built and embedded automatically. There is no separate scheme for the extension on its own (the system only loads an extension through its containing app).

## Output artefacts

```
~/Library/Developer/Xcode/DerivedData/CascadeFileProvider-*/Build/Products/<config>/
    CascadeFileProvider.appex/
    CascadeFileProviderHost.app/
        Contents/
            MacOS/CascadeFileProviderHost
            PlugIns/CascadeFileProvider.appex/
                Contents/
                    MacOS/CascadeFileProvider
                    Info.plist
                    Resources/
            Resources/
            Info.plist
```

The embedded `.appex` is what macOS loads after `NSFileProviderManager.add(_:completionHandler:)` succeeds. The host app's `ContentView` exposes "Register File Provider" and "Remove" buttons that wrap that API.

## Project layout

```
swift/
  CascadeFileProvider.xcodeproj/      # hand-authored project (no XcodeGen)
    project.pbxproj
    xcshareddata/xcschemes/CascadeFileProviderHost.xcscheme
  CascadeFileProvider/
    Sources/                          # File Provider extension target
      CascadeFileProvider.swift       # NSFileProviderReplicatedExtension subclass
      FileProviderItem.swift          # NSFileProviderItem + UTType + itemVersion
      FileProviderEnumerator.swift    # NSFileProviderEnumerator
      ActionHandler.swift             # JSON-over-Unix-socket bridge to the engine
    Host/                             # SwiftUI host application target
      CascadeFileProviderHostApp.swift
      ContentView.swift
    Resources/                        # Info.plists + entitlements for both targets
      CascadeFileProvider-Info.plist
      CascadeFileProvider.entitlements
      CascadeFileProviderHost-Info.plist
      CascadeFileProviderHost.entitlements
```

The old `Package.swift` is deleted. SwiftPM cannot describe the host/extension topology macOS needs to load a File Provider, so committing one would have been a trap; the Xcode project is the authority.

## Bundle identifiers, deployment target, signing

- Host:        `io.cascade.CascadeFileProviderHost`        (`.app`)
- Extension:   `io.cascade.CascadeFileProviderHost.FileProvider` (`.appex`)
- App Group:   `group.io.cascade.shared` (declared in entitlements; not yet consumed by the bridge — the wire is still the Unix domain socket)
- Deployment:  macOS 12.0 for both targets
- Swift:       `SWIFT_VERSION = 5.0` (the language mode, not the toolchain version — Swift 6.2.3 builds Swift 5 mode by default)
- Signing:     `CODE_SIGN_IDENTITY = "-"`, `CODE_SIGN_STYLE = Manual`, `DEVELOPMENT_TEAM = ""`, `CODE_SIGNING_ALLOWED = NO`

`CODE_SIGNING_ALLOWED = NO` lets the project build out of the box on any machine without a paid developer account. App Group entitlements and App Sandbox both require a real provisioning profile under ad-hoc signing, which a public clone cannot get; disabling signing for unsigned local builds is the conventional escape valve.

To produce a signed, distributable build, override these at the command line:

```bash
xcodebuild \
    -project swift/CascadeFileProvider.xcodeproj \
    -scheme CascadeFileProviderHost \
    -configuration Release \
    -destination "platform=macOS" \
    DEVELOPMENT_TEAM=<your-team-id> \
    CODE_SIGN_STYLE=Automatic \
    CODE_SIGNING_ALLOWED=YES \
    CODE_SIGNING_REQUIRED=YES \
    build
```

The App Group `group.io.cascade.shared` must be provisioned for the team beforehand. The bundle identifiers can be renamed by editing the four `PRODUCT_BUNDLE_IDENTIFIER` / `extensionBundleIdentifier` references — the host's `ContentView` carries the extension identifier so the host can refer to it.

## Rust to Swift bridge

There is no C ABI / FFI surface. The bridge is a Unix domain socket using Cascade's length-prefixed JSON protocol (`cascade_engine::protocol::{Request, Response}` with `encode_message`). Source: `crates/presenter-fileprovider/src/bridge.rs`.

The default socket path resolves to `$HOME/.config/cascade/fileprovider.sock`. The Rust `FileProviderPresenter` pushes these notifications to Swift:

- `upsertItem({ item })`
- `deleteItem({ id })`
- `updateState({ id, state })`
- `fetchContents({ id })` returning `{ path }`
- `evictItem({ id })`
- `startPresenter({ mount_point })`
- `stopPresenter({})`

The Swift `ActionHandler` issues these RPCs to the engine:

- `getItem({ id })`
- `enumerateItems({ parent_id, page? })`
- `fetchContents({ id })`
- `importDocument({ source_url, parent_id, name?, existing_id? })`
- `createDirectory({ name, parent_id })`
- `deleteItem({ id })`
- `moveItem({ id, new_parent_id, new_name })`

Both sets coexist on the same socket; the engine routes incoming requests to its handler and outgoing notifications to the Swift extension. The Rust crate exposes outbound notifiers but does not yet host the inbound RPC handlers — that integration is the next concrete step.

## Follow-ups

- Wire the inbound RPC handlers on the Rust side so the Swift extension's `getItem`, `enumerateItems`, `importDocument`, `createDirectory`, `deleteItem`, and `moveItem` calls actually reach the engine. The Swift TODO comments name each gap (`Sources/ActionHandler.swift`, `Sources/CascadeFileProvider.swift`, `Sources/FileProviderEnumerator.swift`).
- Add a real sync anchor cursor to the bridge so `FileProviderEnumerator.enumerateChanges` can return deltas instead of always reporting "no changes since". The replicated API will fall back to full enumeration without it, so this is an optimisation rather than a correctness issue.
- Add a `setAttributes` RPC for capability and metadata round-trips; `ActionHandler.modifyItem` currently re-reads the item rather than persisting attribute-only changes.
- Replace `CODE_SIGNING_ALLOWED = NO` with a real signing identity once the project has a team and a provisioned App Group. The four bundle identifiers must match what the team has registered.
- Decide a distribution mechanism for the host app (Sparkle, App Store, signed `.zip`, embedded in the daemon installer). The host is only useful to register the domain; once registered, it can be quit.
- Smoke-test by running the host app, clicking "Register File Provider", and observing the Cascade domain appear under Locations in Finder. Listing a directory should round-trip through the engine over the bridge.

## Sources

- Apple, *NSFileProviderReplicatedExtension* — <https://developer.apple.com/documentation/fileprovider/nsfileproviderreplicatedextension> (Wayback: <https://web.archive.org/web/2026/https://developer.apple.com/documentation/fileprovider/nsfileproviderreplicatedextension>)
- Apple, *NSFileProviderManager.add(_:completionHandler:)* — <https://developer.apple.com/documentation/fileprovider/nsfileprovidermanager/2882126-add> (Wayback: <https://web.archive.org/web/2026/https://developer.apple.com/documentation/fileprovider/nsfileprovidermanager/2882126-add>)
- Apple, *NSFileProviderItem* — <https://developer.apple.com/documentation/fileprovider/nsfileprovideritem> (Wayback: <https://web.archive.org/web/2026/https://developer.apple.com/documentation/fileprovider/nsfileprovideritem>)
- SDK headers shipped with Xcode 26.2: `/Applications/Xcode-26.2.0.app/Contents/Developer/Platforms/MacOSX.platform/Developer/SDKs/MacOSX26.2.sdk/System/Library/Frameworks/FileProvider.framework/Headers/`.
