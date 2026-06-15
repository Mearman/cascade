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
./bootstrap.sh   # builds the device + simulator Rust slices, generates bindings, gens the project
```

**Simulator (ad-hoc signed, no Apple account needed):**

```bash
xcodebuild -project CascadeFileProvideriOS.xcodeproj \
  -target CascadeHostApp -sdk iphonesimulator -configuration Debug \
  ARCHS=arm64 ONLY_ACTIVE_ARCH=NO build
```

The project ad-hoc signs (`CODE_SIGN_IDENTITY = "-"`), which needs no developer
identity or provisioning profile, so this works on any machine and in CI
(`codesign -dv` shows `Signature=adhoc`).

**Device (compile/link check, unsigned):**

```bash
xcodebuild ... -sdk iphoneos ARCHS=arm64 CODE_SIGNING_ALLOWED=NO build
```

The extension statically links `libcascade_ffi.a` via `-force_load`, per SDK
(the crate emits both a `.a` and a `.dylib`; a bare `-lcascade_ffi` would pick
the dylib and bake in an un-shippable absolute load path). The result is a
self-contained appex with the Rust engine compiled in — `otool -L` shows no
`libcascade_ffi` dependency.

## Release artifacts

CI uploads the ad-hoc-signed Simulator app as a per-run artifact
(`cascade-ios-simulator-app`) — unzip and drag into a booted simulator.

A signed `.ipa` for real devices is built and attached to GitHub releases by
the `release-ios-ipa` CI job, but only when these repository secrets are set
(it skips without failing otherwise):

- `IOS_DIST_CERT_P12` — base64 of the signing certificate `.p12`
- `IOS_DIST_CERT_PASSWORD` — its export password
- `IOS_KEYCHAIN_PASSWORD` — any password for the temporary CI keychain
- `IOS_PROVISIONING_PROFILE` — base64 of the `.mobileprovision`
- `IOS_TEAM_ID` — the Apple Developer team id
- `IOS_EXPORT_METHOD` — `development`, `ad-hoc`, or `app-store`

The profile must cover the bundle ids `co.uk.mearman.cascade.ios` and
`co.uk.mearman.cascade.ios.fileprovider`; re-add the shared app group (see
`Extension/Resources/Extension.entitlements`) if the profile authorises it.

## Known limits

- **Real device needs a provisioning profile.** iOS requires a profile to
  install *any* app on a device, so ad-hoc signing only covers the Simulator
  (and macOS). To run on a real iPhone, open the project in Xcode and let
  automatic signing provision it with your team — a free personal Apple ID
  works (a 7-day development profile), which also lets you re-add the shared
  app group (see `Extension/Resources/Extension.entitlements`).
- **Read-only.** The `cascade-ffi` surface exposes enumeration and content
  fetch (`list_dir`/`read_file`), so the extension is read-only; write
  operations through the Files app return `noSuchItem` rather than faking
  success.
