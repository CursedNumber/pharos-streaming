// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The Pharos/Trireme authors

package no.aasle.trireme

import org.json.JSONArray
import org.json.JSONObject

/**
 * One remembered Pharos host. Mirrors the desktop's `ServerEntry` schema in
 * `crates/trireme/src/servers.rs` so the two stay shape-compatible — the
 * Android shell persists into SharedPreferences instead of a TOML file.
 */
data class ServerEntry(
    val id: String,
    val host: String,
    val port: Int,
    /** User-provided friendly name; beats every other naming source. */
    val alias: String? = null,
    /** Best-effort cached hostname for an IP-only `host`. */
    val hostnameCache: String? = null,
    /** Per-server pace ("low-latency" | "balanced" | "smooth" | "vrr"). */
    val pace: String? = null,
    /** True after a successful SPAKE2 handshake; drives the tile badge. */
    val paired: Boolean = false,
    /** Hex 32-byte Ed25519 pubkey pinned at pairing time. */
    val hostPubkey: String? = null,
    /** Pair listener port; defaults to 4432 on the host side. */
    val pairPort: Int? = null,
    /**
     * Optional WAN/external hostname (e.g. a DDNS domain) tried as a *fallback*
     * after the LAN [host] — the Home-Assistant internal/external pattern. May be
     * an IDN such as `bücher.example` (the Rust resolver ToASCII-encodes it). Complements
     * the desktop's `alt_addrs` concept for the single most-common case.
     */
    val externalHost: String? = null,
    /** External stream port; defaults to [port] (same port-forward) when null. */
    val externalPort: Int? = null,
    /** ISO-8601 timestamp of the last successful connect, informational. */
    val lastConnected: String? = null,
) {
    /** Friendly name per the resolution rules: alias > hostnameCache > host. */
    fun friendlyName(): String {
        alias?.takeIf { it.trim().isNotEmpty() }?.let { return it }
        hostnameCache?.takeIf { it.isNotEmpty() && it != host }?.let { return it }
        return host
    }

    fun addressLabel(): String = "$host:$port"

    fun pairPortOrDefault(): Int = pairPort ?: 4432

    /** WAN stream address ("extHost:extPort"), or null if no external host set. */
    fun externalAddressLabel(): String? =
        externalHost?.takeIf { it.isNotBlank() }?.let { "$it:${externalPort ?: port}" }

    /** WAN pair address (reuses the pair port), or null if no external host set. */
    fun externalPairLabel(): String? =
        externalHost?.takeIf { it.isNotBlank() }?.let { "$it:${pairPortOrDefault()}" }

    /** Stream fallbacks tried after the LAN address (WAN, etc.) — passed to FFI. */
    fun streamAltAddrs(): List<String> = listOfNotNull(externalAddressLabel())

    /** Pair fallbacks tried after the LAN address — passed to FFI. */
    fun pairAltAddrs(): List<String> = listOfNotNull(externalPairLabel())

    fun toJson(): JSONObject = JSONObject().apply {
        put("id", id)
        put("host", host)
        put("port", port)
        alias?.let { put("alias", it) }
        hostnameCache?.let { put("hostname_cache", it) }
        pace?.let { put("pace", it) }
        put("paired", paired)
        hostPubkey?.let { put("host_pubkey", it) }
        pairPort?.let { put("pair_port", it) }
        externalHost?.let { put("external_host", it) }
        externalPort?.let { put("external_port", it) }
        lastConnected?.let { put("last_connected", it) }
    }

    companion object {
        fun fromJson(o: JSONObject): ServerEntry = ServerEntry(
            id = o.getString("id"),
            host = o.getString("host"),
            port = o.getInt("port"),
            alias = o.optString("alias", "").ifEmpty { null },
            hostnameCache = o.optString("hostname_cache", "").ifEmpty { null },
            pace = o.optString("pace", "").ifEmpty { null },
            paired = o.optBoolean("paired", false),
            hostPubkey = o.optString("host_pubkey", "").ifEmpty { null },
            pairPort = if (o.has("pair_port")) o.getInt("pair_port") else null,
            externalHost = o.optString("external_host", "").ifEmpty { null },
            externalPort = if (o.has("external_port")) o.getInt("external_port") else null,
            lastConnected = o.optString("last_connected", "").ifEmpty { null },
        )

        /** Stable id derived from host:port (same algorithm as the Rust side). */
        fun makeId(host: String, port: Int): String =
            "${host}_$port".filter { it.isLetterOrDigit() || it in "._-" }
    }
}

/** Wraps a list of [ServerEntry]; trivial conversion to/from JSON for storage. */
data class Registry(val servers: List<ServerEntry> = emptyList()) {
    fun find(id: String): ServerEntry? = servers.firstOrNull { it.id == id }

    /** Replace any entry with the same id (or append if new). */
    fun upsert(entry: ServerEntry): Registry {
        val without = servers.filter { it.id != entry.id }
        return Registry(without + entry)
    }

    fun remove(id: String): Registry = Registry(servers.filter { it.id != id })

    fun toJson(): JSONArray = JSONArray().also { arr ->
        servers.forEach { arr.put(it.toJson()) }
    }

    companion object {
        fun fromJson(arr: JSONArray): Registry = Registry(
            (0 until arr.length()).map { ServerEntry.fromJson(arr.getJSONObject(it)) }
        )
    }
}
