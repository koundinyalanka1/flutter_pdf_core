#!/usr/bin/env bash
# Build the Rust core as an XCFramework for iOS (device + simulator).
#
# Prerequisites (macOS only):
#   rustup target add aarch64-apple-ios aarch64-apple-ios-sim x86_64-apple-ios
#
# Usage: scripts/build_ios.sh

set -euo pipefail
cd "$(dirname "$0")/.."

OUT="ios/Frameworks"
BUILD="rust/target"

(
  cd rust
  cargo build -p pdf_ffi --release --target aarch64-apple-ios
  cargo build -p pdf_ffi --release --target aarch64-apple-ios-sim
  cargo build -p pdf_ffi --release --target x86_64-apple-ios
)

# Fat simulator library (arm64 + x86_64).
SIM_DIR="$BUILD/ios-sim-universal"
mkdir -p "$SIM_DIR"
lipo -create \
  "$BUILD/aarch64-apple-ios-sim/release/libpdf_ffi.a" \
  "$BUILD/x86_64-apple-ios/release/libpdf_ffi.a" \
  -output "$SIM_DIR/libpdf_ffi.a"

rm -rf "$OUT/PdfFfi.xcframework"
mkdir -p "$OUT"
xcodebuild -create-xcframework \
  -library "$BUILD/aarch64-apple-ios/release/libpdf_ffi.a" \
  -library "$SIM_DIR/libpdf_ffi.a" \
  -output "$OUT/PdfFfi.xcframework"

echo "Done: $OUT/PdfFfi.xcframework"
