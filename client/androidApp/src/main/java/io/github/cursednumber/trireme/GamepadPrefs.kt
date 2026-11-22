// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The Pharos authors

package io.github.cursednumber.trireme

import android.content.Context
import org.json.JSONObject
import uniffi.core_ffi.ControllerProfile
import uniffi.core_ffi.GyroTuning
import uniffi.core_ffi.defaultControllerProfile

/**
 * Persisted gamepad settings shared by the player and the controller selector.
 *
 * Each controller has a full [ControllerProfile] (face swaps + gyro feel) keyed
 * by a stable `"vvvv:pppp"` id, so its tuning survives reconnects and reboots.
 * The host is the source of truth that *applies* gyro (it reads the IMU), so the
 * player relays a controller's profile to the host on connect; this class is the
 * client-side editor + persistence. Defaults come from the host's single source
 * of truth via [defaultControllerProfile], so client and host agree out of the box.
 */
class GamepadPrefs(context: Context) {
    private val p = context.applicationContext
        .getSharedPreferences("trireme.gamepad", Context.MODE_PRIVATE)

    // Per-controller face-button swaps (kept as discrete getters for the input
    // hot path in ControllerManager; the full profile below wraps the same keys).
    fun swapAb(key: String): Boolean = p.getBoolean(swapAbKey(key), defaultControllerProfile().swapAb)
    fun setSwapAb(key: String, v: Boolean) = p.edit().putBoolean(swapAbKey(key), v).apply()
    fun swapXy(key: String): Boolean = p.getBoolean(swapXyKey(key), defaultControllerProfile().swapXy)
    fun setSwapXy(key: String, v: Boolean) = p.edit().putBoolean(swapXyKey(key), v).apply()

    // Per-controller feature toggles (default on when the pad supports them).
    // Stored independently of the relayed profile; the host honours rumble/paddle
    // forwarding where it can. Keyed per controller like the swaps.
    fun rumble(key: String): Boolean = p.getBoolean("rumble_$key", true)
    fun setRumble(key: String, v: Boolean) = p.edit().putBoolean("rumble_$key", v).apply()
    fun paddles(key: String): Boolean = p.getBoolean("paddles_$key", true)
    fun setPaddles(key: String, v: Boolean) = p.edit().putBoolean("paddles_$key", v).apply()

    /** This controller's full saved profile, defaulting to the host's defaults. */
    fun profile(key: String): ControllerProfile {
        val d = defaultControllerProfile()
        val g = d.gyro
        return ControllerProfile(
            swapAb = p.getBoolean(swapAbKey(key), d.swapAb),
            swapXy = p.getBoolean(swapXyKey(key), d.swapXy),
            gyro = GyroTuning(
                enabled = p.getBoolean("gyro_en_$key", g.enabled),
                sensX = p.getFloat("gsx_$key", g.sensX),
                sensY = p.getFloat("gsy_$key", g.sensY),
                accel = p.getFloat("gac_$key", g.accel),
                accelPow = p.getFloat("gap_$key", g.accelPow),
                accelRef = p.getFloat("gar_$key", g.accelRef),
                accelMax = p.getFloat("gam_$key", g.accelMax),
                invertX = p.getBoolean("ginvx_$key", g.invertX),
                invertY = p.getBoolean("ginvy_$key", g.invertY),
                axisH = p.getInt("gah_$key", g.axisH.toInt()).toUByte(),
                axisV = p.getInt("gav_$key", g.axisV.toInt()).toUByte(),
            ),
        )
    }

    /** Persist this controller's full profile (one atomic edit). */
    fun setProfile(key: String, prof: ControllerProfile) {
        val g = prof.gyro
        p.edit()
            .putBoolean(swapAbKey(key), prof.swapAb)
            .putBoolean(swapXyKey(key), prof.swapXy)
            .putBoolean("gyro_en_$key", g.enabled)
            .putFloat("gsx_$key", g.sensX)
            .putFloat("gsy_$key", g.sensY)
            .putFloat("gac_$key", g.accel)
            .putFloat("gap_$key", g.accelPow)
            .putFloat("gar_$key", g.accelRef)
            .putFloat("gam_$key", g.accelMax)
            .putBoolean("ginvx_$key", g.invertX)
            .putBoolean("ginvy_$key", g.invertY)
            .putInt("gah_$key", g.axisH.toInt())
            .putInt("gav_$key", g.axisV.toInt())
            .apply()
    }

    // --- Named presets: save a profile once, apply it to any controller -------

    /** Sorted names of saved presets. */
    fun savedProfileNames(): List<String> = readPresets().keys().asSequence().toList().sorted()

    /** Save (or overwrite) a named preset from a profile. */
    fun saveNamedProfile(name: String, prof: ControllerProfile) {
        val obj = readPresets()
        obj.put(name.trim(), profileToJson(prof))
        p.edit().putString(K_PRESETS, obj.toString()).apply()
    }

    /** Load a named preset, or null if it's gone. */
    fun loadNamedProfile(name: String): ControllerProfile? {
        val obj = readPresets()
        val o = obj.optJSONObject(name) ?: return null
        return jsonToProfile(o)
    }

    fun deleteNamedProfile(name: String) {
        val obj = readPresets()
        obj.remove(name)
        p.edit().putString(K_PRESETS, obj.toString()).apply()
    }

    /** Rename a preset, preserving its profile. No-op if [old] is gone or the
     *  name is unchanged; overwrites any existing preset called [new]. */
    fun renameNamedProfile(old: String, new: String) {
        val n = new.trim()
        if (n.isEmpty() || n == old) return
        val obj = readPresets()
        val o = obj.optJSONObject(old) ?: return
        obj.remove(old)
        obj.put(n, o)
        p.edit().putString(K_PRESETS, obj.toString()).apply()
    }

    // --- Per-controller assigned profile + custom name ------------------------

    /** The preset name currently applied to this controller, for the dropdown.
     *  [PROFILE_DEFAULT] when untouched, [PROFILE_CUSTOM] once hand-edited, or a
     *  saved preset's name. A name whose preset no longer exists reads as Custom. */
    fun assignedProfile(key: String): String {
        val a = p.getString("assigned_$key", PROFILE_DEFAULT) ?: PROFILE_DEFAULT
        if (a == PROFILE_DEFAULT || a == PROFILE_CUSTOM) return a
        return if (savedProfileNames().contains(a)) a else PROFILE_CUSTOM
    }

    fun setAssignedProfile(key: String, name: String) =
        p.edit().putString("assigned_$key", name).apply()

    /** A user-chosen display name for this controller, or null if unset. */
    fun customName(key: String): String? = p.getString("name_$key", null)?.ifBlank { null }

    fun setCustomName(key: String, name: String) {
        val n = name.trim()
        if (n.isEmpty()) p.edit().remove("name_$key").apply()
        else p.edit().putString("name_$key", n).apply()
    }

    // --- Composite-grouping overrides ----------------------------------------
    // Auto-grouping folds a controller's phantom/bridge interfaces into one card.
    // When that mis-fires (a genuinely separate pad wrongly absorbed), the user can
    // force an interface back out via the Details dialog; the choice persists here.

    fun isSplit(key: String): Boolean = readSplits().contains(key)

    fun setSplit(key: String, split: Boolean) {
        val set = readSplits()
        if (split) set.add(key) else set.remove(key)
        p.edit().putString(K_SPLITS, org.json.JSONArray(set.toList()).toString()).apply()
    }

    private fun readSplits(): MutableSet<String> {
        val arr = runCatching { org.json.JSONArray(p.getString(K_SPLITS, "[]")) }
            .getOrDefault(org.json.JSONArray())
        return (0 until arr.length()).mapTo(mutableSetOf()) { arr.getString(it) }
    }

    private fun readPresets(): JSONObject =
        runCatching { JSONObject(p.getString(K_PRESETS, "{}") ?: "{}") }.getOrDefault(JSONObject())

    private fun profileToJson(prof: ControllerProfile): JSONObject {
        val g = prof.gyro
        return JSONObject()
            .put("swapAb", prof.swapAb).put("swapXy", prof.swapXy)
            .put("enabled", g.enabled)
            .put("sensX", g.sensX.toDouble()).put("sensY", g.sensY.toDouble())
            .put("accel", g.accel.toDouble()).put("accelPow", g.accelPow.toDouble())
            .put("accelRef", g.accelRef.toDouble()).put("accelMax", g.accelMax.toDouble())
            .put("invertX", g.invertX).put("invertY", g.invertY)
            .put("axisH", g.axisH.toInt()).put("axisV", g.axisV.toInt())
    }

    private fun jsonToProfile(o: JSONObject): ControllerProfile {
        val d = defaultControllerProfile()
        val g = d.gyro
        return ControllerProfile(
            swapAb = o.optBoolean("swapAb", d.swapAb),
            swapXy = o.optBoolean("swapXy", d.swapXy),
            gyro = GyroTuning(
                enabled = o.optBoolean("enabled", g.enabled),
                sensX = o.optDouble("sensX", g.sensX.toDouble()).toFloat(),
                sensY = o.optDouble("sensY", g.sensY.toDouble()).toFloat(),
                accel = o.optDouble("accel", g.accel.toDouble()).toFloat(),
                accelPow = o.optDouble("accelPow", g.accelPow.toDouble()).toFloat(),
                accelRef = o.optDouble("accelRef", g.accelRef.toDouble()).toFloat(),
                accelMax = o.optDouble("accelMax", g.accelMax.toDouble()).toFloat(),
                invertX = o.optBoolean("invertX", g.invertX),
                invertY = o.optBoolean("invertY", g.invertY),
                axisH = o.optInt("axisH", g.axisH.toInt()).toUByte(),
                axisV = o.optInt("axisV", g.axisV.toInt()).toUByte(),
            ),
        )
    }

    companion object {
        private const val K_PRESETS = "named_profiles"
        private const val K_SPLITS = "split_interfaces"

        /** Dropdown sentinels — not real preset names (a preset can't be saved as these). */
        const val PROFILE_DEFAULT = "Default"
        const val PROFILE_CUSTOM = "Custom"

        private fun swapAbKey(key: String) = "swap_ab_$key"
        private fun swapXyKey(key: String) = "swap_xy_$key"

        /** Stable per-controller key for the per-pad prefs. */
        fun controllerKey(vendor: Int, product: Int): String =
            "%04x:%04x".format(vendor and 0xffff, product and 0xffff)
    }
}
