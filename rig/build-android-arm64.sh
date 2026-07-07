#!/usr/bin/env bash
# Trireme Android APK build for arm64-v8a (e.g. Xiaomi 14T Pro, Android 16).
# Mirrors build-android.sh but targets arm64-v8a (the 14T Pro is 64-bit only).
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

echo "==[3/4] cross-compile libcore_ffi.so (arm64-v8a release)=="
~/.cargo/bin/cargo ndk -o client/androidApp/src/main/jniLibs \
  -t arm64-v8a \
  build -p core-ffi --lib --release

echo "==[4/4] gradle assembleDebug=="
cd client
~/.local/gradle-8.9/bin/gradle :androidApp:assembleDebug --console=plain

echo "==DONE=="
ls -la androidApp/build/outputs/apk/debug/androidApp-debug.apk
