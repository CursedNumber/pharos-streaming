#!/usr/bin/env bash
# Signed Trireme release APKs for a GitHub release: per-ABI splits
# (arm64-v8a for phones/new TVs, armeabi-v7a for 32-bit TVs like the Bravia
# X90J, x86_64 for emulators/x86 boxes) plus a universal fallback.
#
# Like rig/build-android.sh (the debug loop), this regenerates the UniFFI
# bindings and the per-ABI .so in lockstep — a stale binding against a fresh
# core crashes with "checksum mismatch" on the first FFI call.
#
# Signing: expects TRIREME_KEYSTORE (+ TRIREME_KEYSTORE_PASSWORD, optional
# TRIREME_KEY_ALIAS/TRIREME_KEY_PASSWORD) in the environment, or a
# ~/.config/trireme/keystore.env file to source them from. The keystore is
# the app's permanent identity: devices refuse updates signed by any other
# key, so BACK IT UP and never commit it.
set -euo pipefail
cd "$(dirname "$0")/.."

export JAVA_HOME=/usr/lib/jvm/java-17-openjdk
export ANDROID_HOME=/opt/android-sdk
export ANDROID_NDK_HOME=/opt/android-sdk/ndk/30.0.14904198
export PATH="$JAVA_HOME/bin:$PATH"
. "$HOME/.cargo/env"

if [[ -z "${TRIREME_KEYSTORE:-}" && -f "$HOME/.config/trireme/keystore.env" ]]; then
  # shellcheck source=/dev/null
  . "$HOME/.config/trireme/keystore.env"
fi
if [[ -z "${TRIREME_KEYSTORE:-}" ]]; then
  echo "TRIREME_KEYSTORE is not set and ~/.config/trireme/keystore.env is missing." >&2
  echo "Generate a keystore once (then back it up!):" >&2
  echo "  keytool -genkeypair -keystore ~/.config/trireme/release.keystore \\" >&2
  echo "    -alias trireme -keyalg RSA -keysize 4096 -validity 10000" >&2
  exit 1
fi
export TRIREME_KEYSTORE TRIREME_KEYSTORE_PASSWORD TRIREME_KEY_ALIAS TRIREME_KEY_PASSWORD

VER="$(grep -m1 '^version' Cargo.toml | cut -d'"' -f2)"

echo "==[1/5] host core-ffi build (for bindgen)=="
~/.cargo/bin/cargo build -p core-ffi

echo "==[2/5] regenerate Kotlin bindings=="
~/.cargo/bin/cargo run -p core-ffi --bin uniffi-bindgen -- \
  generate --library target/debug/libcore_ffi.so \
  --language kotlin --out-dir client/shared/generated/kotlin

echo "==[3/5] cross-compile libcore_ffi.so (all ABIs, release)=="
~/.cargo/bin/cargo ndk -o client/androidApp/src/main/jniLibs \
  -t arm64-v8a -t armeabi-v7a -t x86_64 \
  build -p core-ffi --lib --release

echo "==[4/5] gradle assembleRelease (signed, per-ABI splits)=="
(cd client && ~/.local/gradle-8.9/bin/gradle :androidApp:assembleRelease \
  -PabiSplits --console=plain)

echo "==[5/5] collect release APKs into dist/=="
mkdir -p dist
OUT=client/androidApp/build/outputs/apk/release
for abi in arm64-v8a armeabi-v7a x86_64 universal; do
  cp "$OUT/androidApp-$abi-release.apk" "dist/trireme-$VER-$abi.apk"
done

echo "==DONE=="
ls -sh dist/trireme-"$VER"-*.apk
