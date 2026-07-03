// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The Pharos/Trireme authors

import SwiftUI

// Trireme iOS shell. Mirrors the Android app's M0 "hello core" round-trip,
// driven by the SAME Rust core via UniFFI-generated Swift bindings.
@main
struct TriremeApp: App {
    var body: some Scene {
        WindowGroup {
            ContentView()
        }
    }
}
