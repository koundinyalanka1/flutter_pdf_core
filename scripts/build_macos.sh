#!/usr/bin/env bash
# Build the Rust core as a universal dylib for macOS.
#
# Prerequisites (macOS only):
#   rustup target add aarch64-apple-darwin x86_64-apple-darwin
#
# Usage: scripts/build_macos.sh

set -euo pipefail
cd "$(dirname "$0")/.."

OUT="macos/Frameworks"
BUILD="rust/target"

(
  cd rust
  cargo build -p pdf_ffi --release --target aarch64-apple-darwin
  cargo build -p pdf_ffi --release --target x86_64-apple-darwin
)

mkdir -p "$OUT"
lipo -create \
  "$BUILD/aarch64-apple-darwin/release/libpdf_ffi.dylib" \
  "$BUILD/x86_64-apple-darwin/release/libpdf_ffi.dylib" \
  -output "$OUT/libpdf_ffi.dylib"

# Make the install name loader-relative so app bundles can find it.
install_name_tool -id "@rpath/libpdf_ffi.dylib" "$OUT/libpdf_ffi.dylib"

echo "Done: $OUT/libpdf_ffi.dylib"
