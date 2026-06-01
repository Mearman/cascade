# macOS File Provider smoke-test guide

This guide walks through building the Cascade File Provider host app and extension,
registering the domain with macOS, and exercising the full Finder workflow against a
local backend. All commands are copy-pasteable. No cloud account is required — the
local backend exposes an ordinary directory over the File Provider bridge so every
step is fully offline-testable.

## Prerequisites

- macOS 15.4 (Sequoia) or later — required by FSKit and the replicated File Provider API.
  Earlier releases support `NSFileProviderReplicatedExtension` but not the sync-cursor API.
- Xcode 16.0 or later with the Command Line Tools installed:

  ```bash
  xcode-select --install
  ```

- A valid local code-signing identity. Ad-hoc signing (`"-"`) is enough for local
  development and is the project default. Check what is available:

  ```bash
  security find-identity -v -p codesigning
  ```

  A "Mac Developer" or "Apple Development" certificate covers everything needed here.
  Ad-hoc signing (`-`) works for local registration but will be rejected by Gatekeeper on
  machines that did not build the binary. For CI or distribution, pass an explicit
  `CODE_SIGN_IDENTITY` override (see below).

- The Rust toolchain at the version pinned in `rust-toolchain.toml` — `rustup` installs
  it on first use.

## Step 1 — Build the Rust engine

Build the File Provider presenter alongside the rest of the workspace:

```bash
cargo build --release
```

This produces `target/release/cascade`. The presenter code lives in
`crates/presenter-fileprovider/` and is compiled into the binary as a feature; no
separate build step is needed.

## Step 2 — Build the host app and extension

```bash
xcodebuild \
    -project swift/CascadeFileProvider.xcodeproj \
    -scheme CascadeFileProviderHost \
    -configuration Debug \
    -destination "platform=macOS" \
    build
```

Or use the Makefile target:

```bash
make fileprovider-smoke
```

The `make` target runs the build and then opens the resulting host app so you can
proceed directly to registration.

The artefacts land in Xcode's standard DerivedData tree:

```
~/Library/Developer/Xcode/DerivedData/CascadeFileProvider-*/Build/Products/Debug/
    CascadeFileProviderHost.app/
        Contents/
            PlugIns/
                CascadeFileProvider.appex/
```

To use a real signing identity instead of ad-hoc:

```bash
xcodebuild \
    -project swift/CascadeFileProvider.xcodeproj \
    -scheme CascadeFileProviderHost \
    -configuration Debug \
    -destination "platform=macOS" \
    DEVELOPMENT_TEAM=<your-team-id> \
    CODE_SIGN_STYLE=Automatic \
    CODE_SIGNING_REQUIRED=YES \
    build
```

## Step 3 — Configure a local backend

Create a scratch directory to act as the backend root, then write a minimal backend
config. This avoids any OAuth flow and works entirely offline.

```bash
mkdir -p ~/CascadeSmokeTest
mkdir -p ~/.config/cascade

cat > ~/.config/cascade/local-smoke.toml <<'EOF'
type = "local"
root_path = "/Users/$USER/CascadeSmokeTest"
EOF
```

Add the backend to `config.toml` so `cascade start` picks it up:

```bash
cat > ~/.config/cascade/config.toml <<'EOF'
[backends.local-smoke]
type = "local"
EOF
```

## Step 4 — Start the Cascade daemon

```bash
./target/release/cascade start
```

Wait for the daemon to report that it is running:

```bash
./target/release/cascade status
```

The daemon opens a Unix domain socket at `~/.config/cascade/fileprovider.sock`. Confirm
it exists before continuing:

```bash
ls -l ~/.config/cascade/fileprovider.sock
```

## Step 5 — Register the File Provider domain

Open the host app built in step 2:

```bash
open ~/Library/Developer/Xcode/DerivedData/CascadeFileProvider-*/Build/Products/Debug/CascadeFileProviderHost.app
```

In the window that appears, click **Register File Provider**. The status label should
change to "Cascade domain registered. Look for it under Locations in Finder."

If the button reports an error, check the system log (see step 9 below) for the
`com.cascade.fileprovider` subsystem.

To verify registration from the command line:

```bash
pluginkit -m -A -p com.apple.fileprovider-ui | grep -i cascade
```

You should see `io.cascade.CascadeFileProviderHost.FileProvider` in the output.

## Step 6 — Finder operations

Open a Finder window (`⌘N`). Under **Locations** in the sidebar you should see a
**Cascade** entry. Click it to open the domain root. The engine enumerates the contents
of `~/CascadeSmokeTest` through the bridge.

### Create a file

```bash
echo "smoke test content" > ~/CascadeSmokeTest/hello.txt
```

Switch to the Finder window and press `⌘R` to refresh. `hello.txt` should appear.

### Rename

Select `hello.txt` in Finder, press Return to enter rename mode, and type
`hello-renamed.txt`. Confirm. The engine receives a `moveItem` RPC with the new name.

Check the log stream for the engine signal:

```bash
log stream \
    --predicate 'subsystem == "com.cascade.fileprovider"' \
    --style compact \
    2>&1 | grep -E "moveItem|modifyItem"
```

### Move

Create a subdirectory and drag the file into it:

```bash
mkdir ~/CascadeSmokeTest/subdir
```

In Finder, drag `hello-renamed.txt` into the `subdir` folder. The engine receives a
`moveItem` RPC with `new_parent_id` set to the subdir's identifier.

### Delete

Select `hello-renamed.txt` inside `subdir` in Finder and press `⌘Delete`. The engine
receives a `deleteItem` RPC. Confirm the file no longer exists in the backend:

```bash
ls ~/CascadeSmokeTest/subdir/
```

### Create a directory

In Finder, use `⇧⌘N` (New Folder) while inside the Cascade location to create a
directory. The engine receives a `createDirectory` RPC.

### Nested directory move (cross-backend)

If a second backend is configured (e.g. a second local backend at a different
`root_path`), you can test a cross-backend move by dragging a directory from one
backend's mount path to the other. The engine decomposes the move into a recursive
copy followed by a delete of the source tree. The File Provider extension sees a
standard `moveItem` — the cross-backend decomposition happens inside the VFS layer.

To add a second local backend for this test:

```bash
mkdir -p ~/CascadeSmokeTest2

cat > ~/.config/cascade/local-smoke2.toml <<'EOF'
type = "local"
root_path = "/Users/$USER/CascadeSmokeTest2"
mount_path = "Second"
EOF
```

Append to `config.toml`:

```toml
[backends.local-smoke2]
type = "local"
```

Restart the daemon (`./target/release/cascade stop && ./target/release/cascade start`) and
then drag a directory from the Cascade root into the `Second/` folder in Finder.

### Large-directory enumeration (pagination)

Create a directory with enough entries to cross the engine's page boundary (currently
the engine returns 100 items per page):

```bash
mkdir ~/CascadeSmokeTest/large
for i in $(seq 1 250); do touch ~/CascadeSmokeTest/large/file-$i.txt; done
```

Navigate into `large/` in Finder. As you scroll, the File Provider framework calls
`enumerateItems(for:startingAt:)` repeatedly with the page cursor the engine returned
in the previous call. You can observe the page-cursor round-trips in the log:

```bash
log stream \
    --predicate 'subsystem == "com.cascade.fileprovider"' \
    --style compact \
    2>&1 | grep -i "enumerate"
```

You should see multiple calls, each carrying a different `page` parameter.

### Offline-then-reconnect — enumerateChanges delta

This exercises the `enumerateChanges` path and the sync-anchor cursor.

1. Stop the daemon while Finder still has the domain open:

   ```bash
   ./target/release/cascade stop
   ```

2. While offline, add a file directly to the backend root:

   ```bash
   echo "added offline" > ~/CascadeSmokeTest/new-offline.txt
   ```

3. Restart the daemon:

   ```bash
   ./target/release/cascade start
   ```

4. In Finder, navigate away from the Cascade location and then back to it. The system
   detects that its cached sync anchor no longer matches the engine's current cursor
   and calls `enumerateChanges(from:)`. The extension fetches the delta — `new-offline.txt`
   appears as added — and reports the new anchor.

   Confirm the engine signal in the log:

   ```bash
   log stream \
       --predicate 'subsystem == "com.cascade.fileprovider"' \
       --style compact \
       2>&1 | grep -E "enumerateChanges|syncAnchor"
   ```

   You should see a `enumerateChanges RPC` entry and, shortly after, `new-offline.txt`
   appearing in the Finder window without a full re-enumeration.

## Step 7 — Remove the domain

When finished testing, remove the domain so macOS does not retain a stale registration:

In the host app, click **Remove**. Or from the command line:

```bash
pluginkit -r \
    -p com.apple.fileprovider-ui \
    ~/Library/Developer/Xcode/DerivedData/CascadeFileProvider-*/Build/Products/Debug/CascadeFileProviderHost.app
```

Then stop the daemon:

```bash
./target/release/cascade stop
```

## Step 8 — Clean up test data

```bash
rm -rf ~/CascadeSmokeTest ~/CascadeSmokeTest2
rm ~/.config/cascade/local-smoke.toml ~/.config/cascade/local-smoke2.toml
```

## Step 9 — Reading the log stream

All extension log entries use `subsystem = "com.cascade.fileprovider"`. Two
categories are useful:

- `ActionHandler` — logs every RPC sent to the engine, with method name, item
  identifier, and any error code the engine returns.
- `FileProviderEnumerator` — logs sync-anchor decode failures and `enumerateChanges`
  RPC errors.

Live stream, compact format:

```bash
log stream \
    --predicate 'subsystem == "com.cascade.fileprovider"' \
    --style compact
```

Filter to a specific method:

```bash
log stream \
    --predicate 'subsystem == "com.cascade.fileprovider" AND eventMessage CONTAINS "moveItem"' \
    --style compact
```

Show the last minute of stored entries (useful after the fact):

```bash
log show \
    --last 1m \
    --predicate 'subsystem == "com.cascade.fileprovider"' \
    --style compact
```

Errors the system surfaces in Finder (the "Unable to…" dialogues) correspond to
`NSFileProviderError` codes. The mapping from engine error codes to File Provider
errors is in `swift/CascadeFileProvider/Sources/ActionHandler.swift` under
`makeError(from:method:itemID:)`.

## Expected outcomes summary

| Operation | Engine RPC | Log signal |
|-----------|-----------|-----------|
| Directory listing | `enumerateItems` | `FileProviderEnumerator: enumerateItems` |
| File open / download | `fetchContents` | `ActionHandler: fetchContents` |
| Create file | `importDocument` | `ActionHandler: importDocument` |
| New folder | `createDirectory` | `ActionHandler: createDirectory` |
| Rename | `moveItem` (same parent) | `ActionHandler: moveItem` |
| Move | `moveItem` (new parent) | `ActionHandler: moveItem` |
| Delete | `deleteItem` | `ActionHandler: deleteItem` |
| Pagination | `enumerateItems` (repeated) | multiple `enumerateItems` entries |
| Reconnect delta | `enumerateChanges` | `enumerateChanges RPC` |

## Troubleshooting

**Domain does not appear in Finder sidebar.** The extension process may not have
started. Check Activity Monitor for `CascadeFileProvider` (it should appear as a
child of `fileproviderd`). If absent, check the system log for
`subsystem == "com.apple.fileprovider"` for load errors.

**"Register File Provider" returns an error.** The most common cause is a
code-signing problem — the `.appex` must be signed for `NSFileProviderManager.add`
to accept it. Rebuild with a real signing identity (step 2) or verify that ad-hoc
signing produced a signed binary:

```bash
codesign -dv \
    ~/Library/Developer/Xcode/DerivedData/CascadeFileProvider-*/Build/Products/Debug/\
CascadeFileProviderHost.app/Contents/PlugIns/CascadeFileProvider.appex
```

**Finder shows "Unable to connect"** or the extension crashes immediately after
registration. The daemon is not running or the socket does not exist. Run
`./target/release/cascade start` and confirm `~/.config/cascade/fileprovider.sock`
exists before clicking Register.

**enumerateChanges never fires.** The system only calls `enumerateChanges` when it
believes the sync anchor may be stale. Navigate away from the Cascade location and
back, or trigger a full sync via Finder → Go → Connect to Server (this reloads the
domain). If the engine restarted between calls the anchor it held in memory is gone
and the extension falls back to a full re-enumeration — this is correct behaviour,
not a bug.
