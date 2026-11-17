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
    /**
     * Additional LAN addresses this SAME host (same [hostPubkey]) is reachable
     * on — a multi-homed host advertises one identity across every NIC (e.g.
     * a desktop on WiFi .100 and fiber .200). Folded here by [Registry.mergeByIdentity]
     * so one row represents the machine; each address is a selectable connection.
     * Same [port] as [host]; pairing is identity-scoped, so all of them inherit
     * the single paired trust.
     */
    val altHosts: List<String> = emptyList(),
    /**
     * The address the user pinned to dial FIRST ("pin one, auto-fallback"). Must
     * be [host] or one of [altHosts]; `null` = no pin (dial [host] first). The
     * transport tries the rest as ordered fallbacks, so a pinned NIC going down
     * never strands the host.
     */
    val preferredHost: String? = null,
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

    /** Every known LAN address for this host (primary + alternate NICs), deduped. */
    fun lanHosts(): List<String> = (listOf(host) + altHosts).distinct()

    /**
     * LAN addresses in DIAL order: the pinned [preferredHost] first (when it's
     * still a known address), then the rest as fallbacks — the "pin one,
     * auto-fallback" model the transport's ordered [connect_first] implements.
     */
    fun orderedLanHosts(): List<String> {
        val all = lanHosts()
        val pref = preferredHost?.takeIf { it in all } ?: return all
        return listOf(pref) + all.filter { it != pref }
    }

    /** The address actually dialed first (the pinned NIC when set), for display. */
    fun effectiveHost(): String = orderedLanHosts().first()

    /** Primary stream address (pinned NIC first); the rest ride [streamAltAddrs]. */
    fun primaryAddressLabel(): String = "${effectiveHost()}:$port"

    fun pairPortOrDefault(): Int = pairPort ?: 4432

    /** WAN stream address ("extHost:extPort"), or null if no external host set. */
    fun externalAddressLabel(): String? =
        externalHost?.takeIf { it.isNotBlank() }?.let { "$it:${externalPort ?: port}" }

    /** WAN pair address (reuses the pair port), or null if no external host set. */
    fun externalPairLabel(): String? =
        externalHost?.takeIf { it.isNotBlank() }?.let { "$it:${pairPortOrDefault()}" }

    /**
     * Stream fallbacks tried after [primaryAddressLabel], in order: the host's
     * OTHER LAN NICs (so a pinned fiber that drops falls back to wifi), then the
     * WAN/DDNS address. Passed to the FFI as the ordered candidate list.
     */
    fun streamAltAddrs(): List<String> =
        orderedLanHosts().drop(1).map { "$it:$port" } + listOfNotNull(externalAddressLabel())

    /** Pair fallbacks tried after the LAN address — the other NICs, then WAN. */
    fun pairAltAddrs(): List<String> =
        orderedLanHosts().drop(1).map { "$it:${pairPortOrDefault()}" } +
            listOfNotNull(externalPairLabel())

    fun toJson(): JSONObject = JSONObject().apply {
        put("id", id)
        put("host", host)
        put("port", port)
        alias?.let { put("alias", it) }
        hostnameCache?.let { put("hostname_cache", it) }
        pace?.let { put("pace", it) }
        put("paired", paired)
        hostPubkey?.let { put("host_pubkey", it) }
        if (altHosts.isNotEmpty()) put("alt_hosts", JSONArray(altHosts))
        preferredHost?.let { put("preferred_host", it) }
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
            altHosts = o.optJSONArray("alt_hosts")?.let { a ->
                (0 until a.length()).map { a.getString(it) }
            } ?: emptyList(),
            preferredHost = o.optString("preferred_host", "").ifEmpty { null },
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

    /**
     * Collapse entries that share a [ServerEntry.hostPubkey] into ONE host: the
     * same machine advertised on multiple NICs (wifi + fiber) is one identity,
     * so it should be one row with its extra addresses folded into
     * [ServerEntry.altHosts] and selectable as connections. Trust is
     * identity-scoped, so the merged entry is paired if ANY member was — this is
     * what makes a freshly-discovered second NIC stop showing as "Unpaired" once
     * its twin is paired. Entries with no pubkey can't be identified, so they're
     * left untouched. Stable (LinkedHashMap preserves first-seen order) and
     * idempotent, so it's safe to run on every load.
     */
    fun mergeByIdentity(): Registry {
        val groups = LinkedHashMap<String, MutableList<ServerEntry>>()
        servers.forEachIndexed { i, e ->
            // Un-keyed entries each get a unique bucket so they never merge.
            val key = e.hostPubkey?.takeIf { it.isNotBlank() } ?: " solo:$i"
            groups.getOrPut(key) { mutableListOf() }.add(e)
        }
        val merged = groups.values.map { group ->
            if (group.size == 1) return@map group[0]
            // Canonical = the paired member (keeps its id/alias/pace), else the first.
            val canonical = group.firstOrNull { it.paired } ?: group[0]
            val addrs = group.flatMap { it.lanHosts() }.distinct()
            val pref = (canonical.preferredHost
                ?: group.firstNotNullOfOrNull { it.preferredHost })
                ?.takeIf { it in addrs }
            canonical.copy(
                paired = group.any { it.paired },
                hostPubkey = group.firstNotNullOfOrNull { it.hostPubkey },
                altHosts = addrs.filter { it != canonical.host },
                preferredHost = pref,
                alias = canonical.alias ?: group.firstNotNullOfOrNull { it.alias },
                pace = canonical.pace ?: group.firstNotNullOfOrNull { it.pace },
                externalHost = canonical.externalHost
                    ?: group.firstNotNullOfOrNull { it.externalHost },
                externalPort = canonical.externalPort
                    ?: group.firstNotNullOfOrNull { it.externalPort },
                lastConnected = group.mapNotNull { it.lastConnected }.maxOrNull(),
            )
        }
        return Registry(merged)
    }

    fun toJson(): JSONArray = JSONArray().also { arr ->
        servers.forEach { arr.put(it.toJson()) }
    }

    companion object {
        fun fromJson(arr: JSONArray): Registry = Registry(
            (0 until arr.length()).map { ServerEntry.fromJson(arr.getJSONObject(it)) }
        )
    }
}
