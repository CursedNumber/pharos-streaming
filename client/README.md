# Trireme — Kotlin client

Kotlin Multiplatform client (Android first; desktop shares the core at M1). At
**M0** this is a minimal Android app that performs the "hello core" round-trip
against the shared Rust core through UniFFI.

> ✅ **Builds.** A debug APK assembles with the cross-compiled Rust core bundled
> (arm64-v8a + x86_64). Verified locally with the toolchain below. The only
> remaining M0 step is running it on a device/emulator to see the round-trip live.

## Toolchain used (Arch / CachyOS)

- JDK **17** (`jdk17-openjdk`) — newer JDKs (e.g. 26) are rejected by Gradle/AGP.
  The build sets `JAVA_HOME=/usr/lib/jvm/java-17-openjdk` without changing the
  system default.
- Android SDK at `/opt/android-sdk` (`android-sdk-cmdline-tools-latest`), with
  `platform-tools`, `platforms;android-35`, `build-tools;35.0.0`, NDK r30.
- **rustup** (userland, via rustup.rs) for the Android Rust targets — installed
  alongside the Arch `rust` package, not replacing it. Use `~/.cargo/bin/cargo`
  for cross-compiles (it has the Android std; the Arch `cargo` does not).
- `cargo-ndk`, Gradle **8.9**, AGP **8.6**, Kotlin **2.0.20**.

## 1. Generate Kotlin bindings (from repo root)

```sh
cargo build -p core-ffi
cargo run -p core-ffi --bin uniffi-bindgen -- \
  generate --library target/debug/libcore_ffi.so \
  --language kotlin --out-dir client/shared/generated/kotlin
```

Produces `client/shared/generated/kotlin/uniffi/core_ffi/core_ffi.kt`
(package `uniffi.core_ffi`), already wired into `androidApp`'s source set.

## 2. Cross-compile the Rust core into the app's jniLibs

```sh
. "$HOME/.cargo/env"                       # use rustup's cargo
export ANDROID_HOME=/opt/android-sdk
export ANDROID_NDK_HOME=/opt/android-sdk/ndk/30.0.14904198
cargo ndk -o client/androidApp/src/main/jniLibs \
  -t arm64-v8a -t x86_64 \
  build -p core-ffi --lib --release
```

Places `libcore_ffi.so` per-ABI where Gradle expects it. The generated Kotlin
loads it by name `core_ffi` (via JNA). Add `-t armeabi-v7a` for 32-bit devices.

## 3. Build / install the app

```sh
cd client
export JAVA_HOME=/usr/lib/jvm/java-17-openjdk
export ANDROID_HOME=/opt/android-sdk
~/.local/gradle-8.9/bin/gradle :androidApp:assembleDebug   # -> debug APK
# with a device/emulator attached:
~/.local/gradle-8.9/bin/gradle :androidApp:installDebug
```

APK: `androidApp/build/outputs/apk/debug/androidApp-debug.apk`.
Launching shows the core agent, version, protocol version, and a greeting — all
returned across the FFI boundary from Rust, completing the M0 exit criterion
"UniFFI→Kotlin hello core round-trips on Android".

## Notes

- Generated bindings + `*.so` + `local.properties` are git-ignored (reproducible
  / machine-local). CI regenerates the Kotlin bindings as an artifact.
- A `:shared` Kotlin Multiplatform module (Android + desktop shared logic) is
  added at M1; for M0, `androidApp` consumes the bindings directly.
