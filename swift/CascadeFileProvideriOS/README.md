# Cascade iOS File Provider extension

An iOS File Provider extension that presents the Cascade VFS to the Files app
and other apps, running the Cascade engine **in-process** through the
`cascade-ffi` UniFFI bridge. This is the iOS analogue of the macOS File
Provider in `../CascadeFileProvider`, but where that one talks to a separate
daemon over a Unix socket, the iOS extension links the engine directly — iOS
extensions cannot reach a background daemon, so the engine runs inside the
extension process.

## Layout

- `project.yml` — xcodegen spec: a minimal host app (`CascadeHostApp`) that
  embeds the `CascadeFileProviderExt` app extension.
- `Extension/Sources/` — the extension: `FileProviderExtension`
  (`NSFileProviderReplicatedExtension`), `FileProviderEnumerator`,
  `FileProviderItem`, and `CascadeEngine` (an actor owning the `CascadeNode`).
- `Extension/FFI/module.modulemap` — exposes the generated C header as the
  `cascade_ffiFFI` module that the UniFFI Swift bindings import.
- `Host/` — the trivial SwiftUI host app.

The Rust static library, the UniFFI Swift bindings (`cascade_ffi.swift`), the
generated header (`cascade_ffiFFI.h`), and the `.xcodeproj` are all generated —
they are gitignored and produced by `bootstrap.sh`.

## Build

```bash
./bootstrap.sh                 # build the Rust lib, generate bindings, gen project
# then, for the iOS device slice (arm64), unsigned:
xcodebuild -project CascadeFileProvideriOS.xcodeproj \
  -target CascadeHostApp -sdk iphoneos -configuration Debug \
  ARCHS=arm64 ONLY_ACTIVE_ARCH=NO CODE_SIGNING_ALLOWED=NO build
```

The extension statically links `libcascade_ffi.a` via `-force_load` (the crate
emits both a `.a` and a `.dylib`; a bare `-lcascade_ffi` would pick the dylib
and bake in an un-shippable absolute load path). The result is a self-contained
appex with the Rust engine compiled in — `otool -L` shows no `libcascade_ffi`
dependency.

## Known limits

- **Device run needs signing.** The build here is unsigned
  (`CODE_SIGNING_ALLOWED=NO`) for compile/link verification. Running on a real
  device needs a development team, signing, and a provisioned
  `group.co.uk.mearman.cascade` app-group entitlement (declared but not
  provisioned). `CascadeEngine` falls back to the extension's caches directory
  when the app group is unavailable.
- **Simulator slice not built.** Only the `aarch64-apple-ios` (device) library
  is built. Running in the Simulator additionally needs the
  `aarch64-apple-ios-sim` target and a matching library build.
- **Read-only.** The `cascade-ffi` surface exposes enumeration and content
  fetch (`list_dir`/`read_file`), so the extension is read-only; write
  operations through the Files app return `noSuchItem` rather than faking
  success.
