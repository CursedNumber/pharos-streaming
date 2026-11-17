// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The Pharos/Trireme authors

package io.github.cursednumber.trireme.stream

import android.app.Application

/**
 * Process-lifetime [Application] that wires up [StreamHub] before any Activity
 * runs. The hub is a process-scoped `object`; it needs the application [Context]
 * (for the identity file + ServerStore) handed in exactly once, so we do it here
 * rather than from an Activity (which can be recreated and torn down repeatedly).
 *
 * The integration agent registers this in AndroidManifest via
 * `android:name=".stream.TriremeApp"` (fully qualified
 * `io.github.cursednumber.trireme.stream.TriremeApp`).
 */
class TriremeApp : Application() {
    override fun onCreate() {
        super.onCreate()
        StreamHub.init(this)
    }
}
