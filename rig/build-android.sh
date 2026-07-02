#!/usr/bin/env bash
# Fresh Trireme Android APK build for the Bravia X90J (armeabi-v7a, 32-bit).
# NOTE: the X90J is a 32-bit Android TV — its only ABI is armeabi-v7a. An
# arm64-v8a .so will not load there, so cross-compile the 32-bit target.
# Regenerates UniFFI bindings from the current core-ffi, cross-compiles the
# Rust core, and assembles a debug APK.
set -euo pipefail
cd "$(dirname "$0")/.."

export JAVA_HOME=/usr/lib/jvm/java-17-openjdk
export ANDROID_HOME=/opt/android-sdk
export ANDROID_NDK_HOME=/opt/android-sdk/ndk/30.0.14904198
export PATH="$JAVA_HOME/bin:$PATH"
. "$HOME/.cargo/env"

echo "==[1/4] host core-ffi build (for bindgen)=="
~/.cargo/bin/cargo build -p core-ffi

echo "==[2/4] regenerate Kotlin bindings=="
~/.cargo/bin/cargo run -p core-ffi --bin uniffi-bindgen -- \
  generate --library target/debug/libcore_ffi.so \
  --language kotlin --out-dir client/shared/generated/kotlin

echo "==[3/4] cross-compile libcore_ffi.so (armeabi-v7a release)=="
~/.cargo/bin/cargo ndk -o client/androidApp/src/main/jniLibs \
  -t armeabi-v7a \
  build -p core-ffi --lib --release

echo "==[4/4] gradle assembleDebug=="
cd client
~/.local/gradle-8.9/bin/gradle :androidApp:assembleDebug --console=plain

echo "==DONE=="
ls -la androidApp/build/outputs/apk/debug/androidApp-debug.apk
