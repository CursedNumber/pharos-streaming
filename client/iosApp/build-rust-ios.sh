#!/usr/bin/env bash
#
# Build the Pharos Rust core for iOS and assemble an XCFramework + Swift
# bindings for the Trireme iOS app.
#
# MUST run on macOS with Xcode. One-time setup:
#   rustup target add aarch64-apple-ios aarch64-apple-ios-sim x86_64-apple-ios
#
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"   # repo root
cd "$ROOT"

OUT="client/shared/generated/swift"
XCF="client/iosApp/CoreFFI.xcframework"

echo "==> building static libs (device + simulator)"
cargo build -p core-ffi --release --target aarch64-apple-ios
cargo build -p core-ffi --release --target aarch64-apple-ios-sim
cargo build -p core-ffi --release --target x86_64-apple-ios

echo "==> fat simulator lib (arm64 + x86_64)"
mkdir -p target/ios-sim
lipo -create \
  target/aarch64-apple-ios-sim/release/libcore_ffi.a \
  target/x86_64-apple-ios/release/libcore_ffi.a \
  -output target/ios-sim/libcore_ffi.a

echo "==> swift bindings"
cargo run -p core-ffi --bin uniffi-bindgen -- generate \
  --library target/aarch64-apple-ios/release/libcore_ffi.a \
  --language swift --out-dir "$OUT"
# uniffi emits a `*FFI.modulemap`; XCFramework wants it named `module.modulemap`.
HEADERS="$(mktemp -d)"
cp "$OUT"/core_ffiFFI.h "$HEADERS"/
cp "$OUT"/core_ffiFFI.modulemap "$HEADERS"/module.modulemap

echo "==> XCFramework"
rm -rf "$XCF"
xcodebuild -create-xcframework \
  -library target/aarch64-apple-ios/release/libcore_ffi.a -headers "$HEADERS" \
  -library target/ios-sim/libcore_ffi.a -headers "$HEADERS" \
  -output "$XCF"

echo "done:"
echo "  $XCF"
echo "  $OUT/core_ffi.swift   (add to the Xcode target alongside the XCFramework)"
