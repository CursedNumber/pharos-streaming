package dev.pharos.trireme

/**
 * Per-controller display-name overrides relayed by the host (proto v4 alias).
 *
 * The host owns the alias once the panel writes one; clients pick the new name
 * up via the same `ControllerTuning` channel they already use for swap/gyro
 * pushes. The relay wiring itself is still TODO (the wire today is one-way
 * client→host), so this map stays empty in practice — but threading it through
 * the title-fallback chain means a future host→client relay lights up the
 * rename everywhere without touching the per-controller card.
 *
 * Keyed `"vvvv:pppp"` (lowercase hex), same as [GamepadPrefs.controllerKey].
 *
 * (Lifted out of the deleted `ControllersActivity` so the in-shell
 * Controllers pane can still consult it.)
 */
internal object HostAliases {
    private val map = mutableMapOf<String, String>()

    @Synchronized fun get(key: String): String? = map[key]?.takeIf { it.isNotBlank() }

    @Synchronized fun put(key: String, alias: String?) {
        val v = alias?.trim()
        if (v.isNullOrEmpty()) map.remove(key) else map[key] = v
    }
}
