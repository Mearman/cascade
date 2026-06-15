#!/usr/bin/env bash
# Bootstrap the iOS File Provider extension: build the Rust static library for
# the iOS device architecture, generate the UniFFI Swift bindings, vendor them
# into the extension, and generate the Xcode project. After running this, open
# the project in Xcode or build it with xcodebuild (see README.md).
#
# Regenerate whenever the cascade-ffi API changes. The generated artefacts
# (the .a, the Swift bindings, the FFI header, the .xcodeproj) are gitignored;
# this script is the single source of truth for producing them.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo="$(cd "$here/../.." && pwd)"

# Device slice (arm64 iOS) and simulator slice (arm64 sim). The project links
# the matching one per SDK; the simulator slice is what supports an ad-hoc
# signed, profile-free build (a device build needs a real provisioning profile).
for target in aarch64-apple-ios aarch64-apple-ios-sim; do
  echo "==> Building cascade-ffi static library for $target (release)"
  rustup target add "$target" >/dev/null 2>&1 || true
  ( cd "$repo" && cargo build -p cascade-ffi --target "$target" --release )
done

echo "==> Generating Swift bindings from the built library"
lib="$repo/target/aarch64-apple-ios/release/libcascade_ffi.dylib"
( cd "$repo" && cargo run -p cascade-ffi --bin uniffi-bindgen -- generate \
    --library "$lib" --language swift \
    --out-dir "$here/Extension/_bindings" )

echo "==> Vendoring bindings into the extension"
cp "$here/Extension/_bindings/cascade_ffi.swift" "$here/Extension/Sources/cascade_ffi.swift"
cp "$here/Extension/_bindings/cascade_ffiFFI.h" "$here/Extension/FFI/cascade_ffiFFI.h"
rm -rf "$here/Extension/_bindings"

echo "==> Generating Xcode project"
( cd "$here" && xcodegen generate )

echo "==> Done. Build with:"
echo "    xcodebuild -project $here/CascadeFileProvideriOS.xcodeproj \\"
echo "      -target CascadeHostApp -sdk iphoneos -configuration Debug \\"
echo "      ARCHS=arm64 ONLY_ACTIVE_ARCH=NO CODE_SIGNING_ALLOWED=NO build"
