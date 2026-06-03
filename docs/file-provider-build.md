# macOS File Provider extension build

## Status

**wired** — the Xcode project builds clean and produces both a `.app` and an embedded `.appex`. The inbound RPC handlers on the Rust side are wired: the engine dispatches `getItem`, `enumerateItems`, `fetchContents`, `importDocument`, `createDirectory`, `deleteItem`, and `moveItem` to its VFS and `StateDb` handlers, and `currentSyncCursor` / `enumerateChanges` are connected so the replicated File Provider enumeration protocol carries live cursors. What remains before a production-signed release is a real signing identity, an App Group provisioning profile, and a distribution mechanism for the host app.

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
- App Group:   `group.io.cascade.shared` is *not* declared on the entitlements files at rest. The wire is still the Unix domain socket so an App Group adds no functional value yet; declaring it would require a provisioning profile and break `xcodebuild` on clones without an Apple developer account. The entitlements files carry an inline commented-out block to restore once a team identity is configured.
- Deployment:  macOS 12.0 for both targets
- Swift:       `SWIFT_VERSION = 5.0` (the language mode, not the toolchain version — Swift 6.2.3 builds Swift 5 mode by default)
- Signing:     `CODE_SIGN_IDENTITY = "-"`, `CODE_SIGN_STYLE = Manual`, `DEVELOPMENT_TEAM = ""`, `CODE_SIGNING_ALLOWED = YES`, `CODE_SIGNING_REQUIRED = NO`

Ad-hoc signing with `CODE_SIGNING_ALLOWED = YES` produces a real (locally-signed) `.appex` so `NSFileProviderManager.add(_:completionHandler:)` will accept it — `CODE_SIGNING_ALLOWED = NO` would emit an unsigned binary that macOS refuses to register. `CODE_SIGNING_REQUIRED = NO` keeps the project building on any clone without a developer account; combined with `CODE_SIGN_IDENTITY = "-"`, every clone gets a signed-but-not-trusted artefact, which is enough for local development and CI but not for distribution.

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

Both sets coexist on the same socket; the engine routes incoming requests to its handler and outgoing notifications to the Swift extension. The Rust crate hosts both the outbound notifiers and the inbound RPC handlers; the VFS and `StateDb` dispatch paths are wired for all methods listed above.

## Follow-ups

- Add a `setAttributes` RPC for capability and metadata round-trips; `ActionHandler.modifyItem` currently re-reads the item rather than persisting attribute-only changes.
- Replace ad-hoc signing with a real signing identity once the project has a team and a provisioned App Group: flip `CODE_SIGN_STYLE` to `Automatic`, set `DEVELOPMENT_TEAM`, set `CODE_SIGNING_REQUIRED = YES`, and uncomment the `com.apple.security.application-groups` block in both `*.entitlements` files. The four bundle identifiers must match what the team has registered.
- Decide a distribution mechanism for the host app (Sparkle, App Store, signed `.zip`, embedded in the daemon installer). The host is only useful to register the domain; once registered, it can be quit.

## Sources

- Apple, *NSFileProviderReplicatedExtension* — <https://developer.apple.com/documentation/fileprovider/nsfileproviderreplicatedextension> (Wayback: <https://web.archive.org/web/2026/https://developer.apple.com/documentation/fileprovider/nsfileproviderreplicatedextension>)
- Apple, *NSFileProviderManager.add(_:completionHandler:)* — <https://developer.apple.com/documentation/fileprovider/nsfileprovidermanager/2882126-add> (Wayback: <https://web.archive.org/web/2026/https://developer.apple.com/documentation/fileprovider/nsfileprovidermanager/2882126-add>)
- Apple, *NSFileProviderItem* — <https://developer.apple.com/documentation/fileprovider/nsfileprovideritem> (Wayback: <https://web.archive.org/web/2026/https://developer.apple.com/documentation/fileprovider/nsfileprovideritem>)
- SDK headers shipped with Xcode 26.2: `/Applications/Xcode-26.2.0.app/Contents/Developer/Platforms/MacOSX.platform/Developer/SDKs/MacOSX26.2.sdk/System/Library/Frameworks/FileProvider.framework/Headers/`.
