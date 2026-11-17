plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
    id("org.jetbrains.kotlin.plugin.compose")
}

// Release signing comes from the environment (rig/build-android-release.sh
// sources ~/.config/trireme/keystore.env on the maintainer machine). Unset,
// release builds come out unsigned and dev debug builds are untouched.
val releaseKeystore: String? = System.getenv("TRIREME_KEYSTORE")

android {
    namespace = "io.github.cursednumber.trireme"
    compileSdk = 35

    defaultConfig {
        applicationId = "io.github.cursednumber.trireme"
        minSdk = 26
        targetSdk = 35
        // Bump both alongside [workspace.package] version in the root
        // Cargo.toml when cutting a release; an APK update only installs
        // over the old one if versionCode increased.
        versionCode = 3
        versionName = "0.2.0"
    }

    sourceSets["main"].apply {
        // UniFFI-generated Kotlin (regenerated from the Rust core via uniffi-bindgen).
        java.srcDir("../shared/generated/kotlin")
        // Cross-compiled Rust core: drop libcore_ffi.so per-ABI here (see README).
        jniLibs.srcDir("src/main/jniLibs")
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }
    kotlinOptions { jvmTarget = "17" }
    // With Kotlin 2.0 the Compose compiler comes from the kotlin.plugin.compose
    // plugin above — no composeOptions/kotlinCompilerExtensionVersion needed.
    // buildConfig=true so the on-screen debug HUD can gate on BuildConfig.DEBUG /
    // show BuildConfig.VERSION_NAME (AGP 8 generates BuildConfig only when asked).
    buildFeatures {
        compose = true
        buildConfig = true
    }

    // The debug variant installs SIDE-BY-SIDE with release: applicationIdSuffix
    // gives it its own package id (io.github.cursednumber.trireme.debug) + data dir, so an
    // e2e test build coexists with a "daily driver" release and has its own
    // pairing/identity (pair it once; it won't clash with release). The
    // src/debug/res override renames it "Trireme Debug" in the launcher. The
    // on-screen metrics HUD (DebugHud) is gated on BuildConfig.DEBUG, so it only
    // ships in this variant.
    signingConfigs {
        if (releaseKeystore != null) {
            create("release") {
                storeFile = file(releaseKeystore)
                storePassword = System.getenv("TRIREME_KEYSTORE_PASSWORD")
                keyAlias = System.getenv("TRIREME_KEY_ALIAS") ?: "trireme"
                keyPassword = System.getenv("TRIREME_KEY_PASSWORD")
                    ?: System.getenv("TRIREME_KEYSTORE_PASSWORD")
            }
        }
    }

    buildTypes {
        getByName("debug") {
            applicationIdSuffix = ".debug"
            versionNameSuffix = "-debug"
            isDebuggable = true
        }
        getByName("release") {
            isMinifyEnabled = false
            if (releaseKeystore != null) {
                signingConfig = signingConfigs.getByName("release")
            }
        }
    }

    // Per-ABI release APKs (plus a universal fallback), opt-in via
    // -PabiSplits so everyday assembleDebug still yields one APK. Only
    // meaningful when the matching libcore_ffi.so files are in jniLibs —
    // rig/build-android-release.sh cross-compiles all three ABIs first.
    if (project.hasProperty("abiSplits")) {
        splits {
            abi {
                isEnable = true
                reset()
                include("arm64-v8a", "armeabi-v7a", "x86_64")
                isUniversalApk = true
            }
        }
    }
}

dependencies {
    // UniFFI's generated Kotlin uses JNA to load the native core.
    implementation("net.java.dev.jna:jna:5.14.0@aar")

    implementation("androidx.core:core-ktx:1.13.1")
    implementation("androidx.activity:activity-compose:1.9.0")
    implementation(platform("androidx.compose:compose-bom:2024.06.00"))
    implementation("androidx.compose.ui:ui")
    implementation("androidx.compose.foundation:foundation")
    implementation("androidx.compose.material3:material3")
    implementation("androidx.compose.material:material-icons-core")
    implementation("androidx.compose.material:material-icons-extended")
    implementation("androidx.lifecycle:lifecycle-runtime-ktx:2.8.0")
    implementation("androidx.lifecycle:lifecycle-runtime-compose:2.8.0")
}
