# Cascade FSKit Presenter

> macOS 15.4+ (Sequoia / macOS 26 Tahoe) native filesystem presenter using Apple's FSKit framework.

## Overview

FSKit is Apple's modern framework for implementing custom filesystems in user space, introduced in macOS 15.4 (Sequoia). It is the recommended replacement for both FUSE (which requires a third-party kernel extension) and File Provider (which is a content-sync surface, not a real filesystem). FSKit presents a full POSIX filesystem — `getattr`, `setattr`, `lookup`, `readdir`, `read`, `write`, `mkdir`, `rename`, `xattr`, and more — without requiring a kernel extension.

The Cascade FSKit presenter follows the same architecture as the existing File Provider presenter: a Rust crate (`cascade-presenter-fskit`) communicates with a Swift FSKit extension (`CascadeFSKit`) over a Unix domain socket using the engine's length-prefixed JSON protocol.

## Architecture

```
┌──────────────────────────────────────────────────────────────┐
│  macOS Kernel                                                 │
│  VFS layer sends filesystem requests via FSKit framework      │
└───────────────────────┬──────────────────────────────────────┘
                        │ FSKit callbacks
┌───────────────────────▼──────────────────────────────────────┐
│  CascadeFSKit Extension (Swift, sandboxed app extension)      │
│  CascadeFSKitExtension → CascadeFileSystem → CascadeFSVolume  │
│  Translates FSKit callbacks → engine protocol messages        │
└───────────────────────┬──────────────────────────────────────┘
                        │ Unix domain socket (length-prefixed JSON)
┌───────────────────────▼──────────────────────────────────────┐
│  Cascade Engine (Rust)                                        │
│  cascade-presenter-fskit crate (FSKitPresenter + FSKitBridge) │
│  Resolves paths to backends, manages cache, serves content    │
└──────────────────────────────────────────────────────────────┘
```

### Extension registration

The FSKit extension is registered with macOS via:

1. **Info.plist** `EXExtensionPointIdentifier` = `com.apple.fskit.fsmodule`
2. **Entitlements** `com.apple.developer.fskit.fsmodule` = `true`
3. **App Sandbox** enabled (`com.apple.security.app-sandbox` = `true`)
4. System Settings → General → Login Items & Extensions → File System Extensions — the user must toggle the extension on after install

### Required delegate methods

The Swift extension must implement:

| Protocol | Methods | Purpose |
|----------|---------|---------|
| `UnaryFileSystemExtension` | `fileSystem` property | Return the `FSUnaryFileSystem` instance |
| `FSUnaryFileSystemOperations` | `probeResource`, `loadResource`, `unloadResource`, `didFinishLoading` | Resource lifecycle |
| `FSVolume.Operations` | `activate`, `deactivate`, `mount`, `unmount`, `synchronize`, `attributes`, `lookupItem`, `contents` | Core filesystem ops |
| `FSVolume.PathConfOperations` | `maximumNameLength`, etc. | Pathconf values |
| `FSVolume.ReadWriteOperations` | `read`, `write` | File data I/O |
| `FSVolume.OpenCloseOperations` | `openItem`, `closeItem` | File handle tracking |
| `FSVolume.XattrOperations` | `xattr`, `setXattr`, `xattrs` | Extended attributes |

### Mount behaviour

FSKit volumes are mounted via the system `mount` command:

```bash
# Create a dummy block device (FSKit V1 requires one)
mkfile -n 100m /tmp/cascade-dummy.raw
hdiutil attach -imagekey diskimage-class=CRawDiskImage -nomount /tmp/cascade-dummy.raw
# Returns e.g. /dev/disk18

# Mount the Cascade volume
mkdir -p /tmp/cascade
mount -F -t Cascade /dev/disk18 /tmp/cascade

# Unmount
umount /tmp/cascade
```

The mount must NOT be inside macOS privacy-protected directories (Desktop, Documents, Downloads, iCloud Drive, Pictures, Movies, Music). The default `/Volumes/Cascade` and any fresh directory under `$HOME` are safe.

## Language requirements

FSKit is **Swift-only**. The framework provides Swift APIs (`FSUnaryFileSystem`, `FSVolume`, `FSItem`) with no C or Objective-C bridging layer. This is why Cascade uses the same Rust+Swift bridge pattern as the File Provider presenter:

- **Rust side** (`crates/presenter-fskit/`): Implements `VfsPresenter`, talks to the extension over a Unix socket.
- **Swift side** (`swift/CascadeFSKit/`): Receives FSKit callbacks from the kernel, translates to protocol messages.

## Entitlements and distribution

The FSKit extension requires:

| Entitlement | Value | Purpose |
|-------------|-------|---------|
| `com.apple.security.app-sandbox` | `true` | Required for App Store / notarised distribution |
| `com.apple.developer.fskit.fsmodule` | `true` | Grants FSKit filesystem module privileges |

Distribution requirements:

- **Notarisation**: The app containing the extension must be notarised by Apple.
- **Developer ID**: A paid Apple Developer account is required for Developer ID signing.
- **Hardened runtime**: Enabled automatically for notarised builds.
- **App Store**: FSKit extensions are accepted on the Mac App Store with sandbox enabled.

## Minimum macOS version

| macOS version | FSKit support |
|---------------|---------------|
| macOS 15.0–15.3 | No FSKit |
| macOS 15.4 (Sequoia) | FSKit introduced, basic operations |
| macOS 26 (Tahoe) | Full FSKit with improved stability |

The Cascade FSKit presenter targets macOS 15.4 as a minimum.

## Build instructions

### Rust crate

```bash
cargo build -p cascade-presenter-fskit
cargo test -p cascade-presenter-fskit
```

### Swift extension (requires macOS 15.4+ with Xcode)

The Swift Package can be built standalone:

```bash
cd swift/CascadeFSKit
swift build
```

For a full app extension bundle, use Xcode:

```bash
# Create or open an Xcode project that embeds the extension target
xcodebuild -project Cascade.xcodeproj \
    -scheme CascadeFSKit \
    -configuration Debug \
    -destination 'platform=macOS' \
    build
```

The extension must be embedded in a host application. The host app registers the extension at install time.

## Comparison with other presenters

| Feature | FSKit | File Provider | FUSE | NFS |
|---------|-------|---------------|------|-----|
| **Minimum macOS** | 15.4 | 11.0 | 10.15 (with macFUSE) | 10.0 |
| **POSIX semantics** | Full | Limited | Full | Full |
| **Kernel extension** | No | No | Yes (macFUSE) | No |
| **Finder integration** | Native volume | Cloud storage pane | Volume | Volume |
| **Write support** | Full | Sync-based | Full | Full |
| **App Store** | Yes | Yes | No | N/A |
| **Distribution** | Notarised app | Notarised app | macFUSE install | Built-in |
| **On-demand fetch** | Via read | Via startProviding | Via read | Via read |
| **Cache eviction** | Manual unpin | Automatic | Manual | Manual |
| **Case sensitivity** | Configurable | No | Configurable | Configurable |
| **Performance** | Direct VFS | Indirect (sync) | Direct VFS | Network hop |

### When to use FSKit

- **Recommended** for macOS 15.4+ deployments where POSIX semantics matter.
- The best choice for new macOS installations that don't need to support older OS versions.

### When to use File Provider instead

- Targeting macOS 11.0–15.3 where FSKit is unavailable.
- Cloud storage sync scenarios where shallow content placeholders are sufficient.

### When to use NFS

- Cross-platform fallback (Linux, Windows).
- Environments where installing an app extension is impractical.

### When to use FUSE

- Linux-only deployments.
- When macFUSE is already installed on macOS systems.

## Known limitations

1. **Block device requirement (FSKit V1)**: Current FSKit requires attaching a dummy raw disk image via `hdiutil` before mounting. This is a limitation of the V1 API — Apple may lift it in a future release.

2. **No kernel extension fallback**: FSKit is user-space only. If the FSKit daemon (`fskitd`) crashes or is unavailable, the volume disappears. The engine must handle reconnection.

3. **Sandbox restrictions**: The extension runs in a sandbox. It can only access its own container and the socket path. The engine daemon (outside the sandbox) must place the socket where the extension can reach it.

4. **Privacy-protected directories**: Mount points inside Desktop, Documents, Downloads, iCloud Drive, Pictures, Movies, and Music are rejected by `fskitd`. Use `/Volumes/` or a custom directory under `$HOME`.

5. **Single-user mounts**: FSKit volumes are per-user. Other users on the same machine cannot access the mount without their own mount.

6. **No async I/O on the socket from within the extension**: The FSKit extension process is managed by launchd. Socket communication uses synchronous POSIX I/O (`read`/`write` syscalls) wrapped in `Task.detached` to avoid blocking the FSKit actor executor.

7. **Swift-only API**: No C bridge — the extension must be written in Swift. The Rust side cannot call FSKit directly.

## References

- **FSKit framework** — Apple Developer Documentation: https://developer.apple.com/documentation/fskit
- **WWDC 2025 "What's new in filesystems"** — Apple's introduction to FSKit at WWDC 2025.
- **KhaosT/FSKitSample** — Minimal FSKit example by Khaos Tian: https://github.com/KhaosT/FSKitSample
- **sohonetlabs/testfs** — Production-quality read-only FSKit filesystem: https://github.com/sohonetlabs/testfs
- **Merkost/FreeDroid** — MTP filesystem via FSKit: https://github.com/Merkost/FreeDroid
- **Cascade design specification** — `docs/design.md`
- **File Provider presenter** — `crates/presenter-fileprovider/`
