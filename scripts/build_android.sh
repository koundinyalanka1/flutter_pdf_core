#!/usr/bin/env bash
# Build the Rust core for Android and drop the .so files where the Gradle
# library project bundles them automatically (android/src/main/jniLibs).
#
# Prerequisites:
#   rustup target add aarch64-linux-android armv7-linux-androideabi x86_64-linux-android
#   cargo install cargo-ndk
#   ANDROID_NDK_HOME pointing at an installed NDK (r25+).
#
# Usage: scripts/build_android.sh [--debug]

set -euo pipefail
cd "$(dirname "$0")/.."

PROFILE_FLAG="--release"
if [[ "${1:-}" == "--debug" ]]; then
  PROFILE_FLAG=""
fi

JNI_DIR="android/src/main/jniLibs"
mkdir -p "$JNI_DIR"

(
  cd rust
  cargo ndk \
    -t arm64-v8a \
    -t armeabi-v7a \
    -t x86_64 \
    -o "../$JNI_DIR" \
    build -p pdf_ffi $PROFILE_FLAG
)

echo "Done. Bundled libraries:"
find "$JNI_DIR" -name '*.so' -exec ls -lh {} \;
