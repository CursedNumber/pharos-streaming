// Root build file. Versions are indicative (scaffold) — pin them when the
// Android toolchain is set up. See client/README.md.
plugins {
    id("com.android.application") version "8.6.0" apply false
    id("org.jetbrains.kotlin.android") version "2.0.20" apply false
    // Kotlin 2.0+ moves the Compose compiler into its own plugin.
    id("org.jetbrains.kotlin.plugin.compose") version "2.0.20" apply false
}
