// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The Pharos/Trireme authors

package io.github.cursednumber.trireme

import android.content.Context
import org.json.JSONArray
import org.json.JSONException

/**
 * SharedPreferences-backed persistence for the [Registry]. The Android shell
 * uses this instead of a TOML file; the storage format is JSON keyed by
 * `SERVERS_KEY` so a future migration to DataStore or Room is mechanical.
 *
 * Thread-safety: all SharedPreferences mutations go through apply(), which
 * updates the in-memory state synchronously (so subsequent reads see the new
 * state) and flushes to disk asynchronously. Callers from the UI thread should
 * still wrap heavy reads in a coroutine.
 */
class ServerStore(context: Context) {
    private val prefs = context
        .applicationContext
        .getSharedPreferences("trireme.servers", Context.MODE_PRIVATE)

    fun load(): Registry {
        val raw = prefs.getString(SERVERS_KEY, null) ?: return Registry()
        return try {
            Registry.fromJson(JSONArray(raw))
        } catch (e: JSONException) {
            Registry()
        }
    }

    fun save(registry: Registry) {
        prefs.edit()
            .putString(SERVERS_KEY, registry.toJson().toString())
            .apply()
    }

    fun upsert(entry: ServerEntry): Registry {
        val next = load().upsert(entry)
        save(next)
        return next
    }

    fun remove(id: String): Registry {
        val next = load().remove(id)
        save(next)
        return next
    }

    /** Absolute path to the long-term Ed25519 identity file (created at first use). */
    fun identityPath(context: Context): String =
        context.applicationContext.filesDir.resolve("identity.toml").absolutePath

    companion object {
        private const val SERVERS_KEY = "servers_json"
    }
}
