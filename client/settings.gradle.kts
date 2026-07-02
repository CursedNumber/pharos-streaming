pluginManagement {
    repositories {
        google()
        mavenCentral()
        gradlePluginPortal()
    }
}
dependencyResolutionManagement {
    repositories {
        google()
        mavenCentral()
    }
}

rootProject.name = "trireme"
include(":androidApp")
// NOTE: a :shared Kotlin Multiplatform module (desktop + Android shared logic)
// is added at M1. For the M0 "hello core" round-trip, androidApp consumes the
// UniFFI bindings directly.
