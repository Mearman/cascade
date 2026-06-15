#!/usr/bin/env bash
# Bootstrap the Android app: cross-compile the cascade-ffi shared library for
# each Android ABI and drop it into app/src/main/jniLibs, so gradle can package
# it. The .so files are gitignored build artefacts; this script reproduces them.
#
# Requires the Android NDK. Set ANDROID_NDK to your NDK path, or edit the
# default below. The Rust targets are added automatically.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo="$(cd "$here/.." && pwd)"

ndk="${ANDROID_NDK:-/opt/homebrew/share/android-commandlinetools/ndk/28.2.13676358}"
host_tag="darwin-x86_64"
[ -d "$ndk/toolchains/llvm/prebuilt/linux-x86_64" ] && host_tag="linux-x86_64"
tc="$ndk/toolchains/llvm/prebuilt/$host_tag/bin"
api=24

if [ ! -d "$tc" ]; then
  echo "NDK toolchain not found at $tc — set ANDROID_NDK to your NDK install" >&2
  exit 1
fi

# ABI -> (rust target, clang prefix) pairs.
build() {
  local abi="$1" target="$2" clang="$3"
  echo "==> Building cascade-ffi for $abi ($target)"
  rustup target add "$target" >/dev/null 2>&1 || true
  local upper
  upper="$(echo "$target" | tr 'a-z-' 'A-Z_')"
  env \
    "CARGO_TARGET_${upper}_LINKER=$tc/${clang}${api}-clang" \
    "CC_${target//-/_}=$tc/${clang}${api}-clang" \
    "AR_${target//-/_}=$tc/llvm-ar" \
    cargo build -p cascade-ffi --target "$target" --release --manifest-path "$repo/Cargo.toml"
  mkdir -p "$here/app/src/main/jniLibs/$abi"
  cp "$repo/target/$target/release/libcascade_ffi.so" "$here/app/src/main/jniLibs/$abi/libcascade_ffi.so"
}

build "arm64-v8a" "aarch64-linux-android" "aarch64-linux-android"
build "x86_64" "x86_64-linux-android" "x86_64-linux-android"

echo "==> Done. Build the APK with:"
echo "    cd $here && ./gradlew :app:assembleDebug"
