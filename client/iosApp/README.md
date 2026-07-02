# Trireme — iOS baseline

iOS shell for Trireme, driven by the **same Rust core** as Android via UniFFI's
Swift bindings. This is a *baseline* so iPhone support isn't an afterthought —
the architecture (Rust core → generated bindings → thin native UI) is identical
to Android, just Swift instead of Kotlin.

> ⚠️ **Builds on macOS only.** iOS cross-compilation needs Xcode's iOS SDK +
> linker, which don't exist on the Linux box this was scaffolded on. The Swift
> bindings (`client/shared/generated/swift/`) and the `.a`/XCFramework are build
> outputs produced by `build-rust-ios.sh` on a Mac (git-ignored, like the Android
> jniLibs) — only the hand-written sources and this script are committed.

## How it fits the plan

At **M1** a Kotlin Multiplatform `:shared` module unifies the app-level logic
(pairing, session orchestration) across Android + iOS + desktop. Until then, each
platform consumes the Rust core's generated bindings directly — Android via the
Kotlin bindings, iOS via these Swift ones. The Rust core is the single source of
truth either way.

## Build (on macOS)

```sh
rustup target add aarch64-apple-ios aarch64-apple-ios-sim x86_64-apple-ios
./client/iosApp/build-rust-ios.sh      # -> CoreFFI.xcframework + Swift bindings
```

Then, in Xcode:

1. New iOS App (SwiftUI), product name **Trireme**, bundle id `dev.pharos.trireme`.
2. Add the sources in `Sources/Trireme/` (`TriremeApp.swift`, `ContentView.swift`).
3. Add `client/shared/generated/swift/core_ffi.swift` to the target.
4. Drag `CoreFFI.xcframework` into *Frameworks, Libraries, and Embedded Content*.
5. Fullscreen: SwiftUI is fullscreen by default. Orientation: set supported
   orientations in the target's *Deployment Info* (landscape-first for streaming),
   the iOS analogue of the Android `screenOrientation`/`configChanges` handling.

Running shows the core agent, version, protocol version, and greeting — the same
round-trip proven on Android, now on iOS.
