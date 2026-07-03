# Trireme — Kotlin client

Kotlin Multiplatform client (Android first; a Rust desktop client lives in
`crates/trireme`). The Android app is a full streaming client: host discovery +
pairing, hardware HEVC/AV1 video decode → render, gamepad/keyboard/touch input
back to the host, gyro-capable controller profiles, and USB device export over
USB/IP. It runs on phones and Android TV (validated on a Sony Bravia and a Xiaomi
phone). The app's `applicationId` is `no.aasle.trireme` (`.debug` suffix for the
side-by-side debug variant).

The app talks to the host through the shared Rust core via **UniFFI** (a
cross-compiled `.so` + generated Kotlin bindings).

## Canonical build

Use the repo's build script — it regenerates the UniFFI bindings and the
per-ABI `.so` **in lockstep**, which avoids the "checksum mismatch" crash you get
on the first FFI call when a stale binding meets a freshly built core:

```sh
rig/build-android.sh          # generates bindings + .so, assembles the debug APK
```

Then install on an attached device/emulator:

```sh
adb install -r client/androidApp/build/outputs/apk/debug/androidApp-debug.apk
```

For a Kotlin-only change (no Rust core change) you can skip regeneration and just
assemble: `gradle :androidApp:assembleDebug`.

### ABI note

Add `armeabi-v7a` for **32-bit** devices (some older Android TVs, e.g. the Bravia
X90J) — an `arm64-v8a` `.so` will not load there. `rig/build-android.sh` documents
the target list it builds.

## Toolchain

- **JDK 17** — newer JDKs are rejected by Gradle/AGP. Point `JAVA_HOME` at a
  JDK 17 without changing your system default.
- **Android SDK** with `platform-tools`, `platforms;android-35`,
  `build-tools;35.0.0`, and an NDK (set `ANDROID_HOME` / `ANDROID_NDK_HOME`).
- **rustup** for the Android Rust targets (installs the Android `std`; a distro
  `cargo` may not have it). Use rustup's `cargo` for the cross-compile.
- `cargo-ndk`, Gradle **8.9**, AGP **8.6**, Kotlin **2.0.20**.

## Manual build (if you're not using the script)

1. **Generate Kotlin bindings** (from repo root):

   ```sh
   cargo build -p core-ffi
   cargo run -p core-ffi --bin uniffi-bindgen -- \
     generate --library target/debug/libcore_ffi.so \
     --language kotlin --out-dir client/shared/generated/kotlin
   ```

   Produces `client/shared/generated/kotlin/uniffi/core_ffi/core_ffi.kt`
   (package `uniffi.core_ffi`), already wired into `androidApp`'s source set.

2. **Cross-compile the core** into the app's jniLibs:

   ```sh
   cargo ndk -o client/androidApp/src/main/jniLibs \
     -t arm64-v8a -t x86_64 \      # add -t armeabi-v7a for 32-bit devices
     build -p core-ffi --lib --release
   ```

3. **Assemble / install** (from `client/`, with `JAVA_HOME`/`ANDROID_HOME` set):

   ```sh
   gradle :androidApp:assembleDebug     # -> androidApp/build/outputs/apk/debug/…
   gradle :androidApp:installDebug      # with a device/emulator attached
   ```

## Notes

- Generated bindings, `*.so`, and `local.properties` are git-ignored (they're
  reproducible / machine-local). CI regenerates the Kotlin bindings as an artifact.
- The generated Kotlin loads the core by name `core_ffi` (via JNA).
