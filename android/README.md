# Cascade Android app (DocumentsProvider)

An Android app that exposes the Cascade VFS through the Storage Access
Framework, running the Cascade engine **in-process** via the `cascade-ffi`
UniFFI Kotlin bindings (loaded with JNA). This is the Android counterpart to
the iOS File Provider extension in `../swift/CascadeFileProvideriOS`.

## Layout

- `app/src/main/java/co/mearman/cascade/CascadeDocumentsProvider.kt` — the
  `DocumentsProvider`: `queryRoots`, `queryChildDocuments` (calls
  `node.listDir`), `queryDocument`, and `openDocument` (streams `node.readFile`
  through a `ParcelFileDescriptor` pipe). Read-only.
- `CascadeNodeHolder.kt` — process-singleton that builds
  `CascadeNode(configDir = filesDir)` and `start()`s it.
- `app/src/main/java/uniffi/cascade_ffi/CascadeNodeFactory.kt` — a small shim
  in the bindings' package. UniFFI 0.31's Kotlin bindgen does not emit a
  wrapper for `CascadeNode`'s async constructor (it does for Swift); the shim
  drives the real `uniffi_cascade_ffi_fn_constructor_cascadenode_new` future
  through the generated internal plumbing.
- `AndroidManifest.xml` — declares the `<provider>` (authority
  `co.mearman.cascade.documents`, `MANAGE_DOCUMENTS`, the `DOCUMENTS_PROVIDER`
  intent filter).

The generated UniFFI Kotlin bindings are consumed directly from
`crates/cascade-ffi/bindings/kotlin` via a `srcDirs` entry — not copied.

## Build

```bash
./bootstrap.sh                 # cross-compile the .so for arm64-v8a + x86_64 into jniLibs
./gradlew :app:assembleDebug   # -> app/build/outputs/apk/debug/app-debug.apk
```

`bootstrap.sh` needs the Android NDK (set `ANDROID_NDK` if it is not at the
default Homebrew path). Gradle reads the SDK location from `local.properties`
(`sdk.dir=...`), which is machine-specific and gitignored — create it locally.
The wrapper pins Gradle 8.7 (AGP 8.5.2), JDK 17.

The built APK bundles `lib/<abi>/libcascade_ffi.so` (the engine) alongside
JNA's `libjnidispatch.so`.

## Known limits

- **Not run on a device/emulator here** — verified to build and package only.
  The x86_64 slice is included so it can run on a standard emulator.
- **`MANAGE_DOCUMENTS` is a system/signature permission**, so a normal debug
  install will not be granted it; registering as a system DocumentsProvider
  needs a privileged/system install.
- **Read-only** — `openDocument` refuses non-`"r"` modes, matching the FFI
  surface (list/read/pin, no write).
