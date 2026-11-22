// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The Pharos authors

package io.github.cursednumber.trireme

import android.content.Context
import android.net.nsd.NsdManager
import android.net.nsd.NsdServiceInfo
import android.os.Build
import android.util.Log
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import java.net.Inet4Address

/**
 * One Pharos host that announced itself on the LAN over mDNS/DNS-SD
 * (`_pharos._udp.local.`). Mirrors the desktop browser's `Discovered`
 * (`crates/trireme/src/discover.rs`): the advertised port is the host's
 * *pairing* listener; the stream port comes from the `stream` TXT record
 * (older hosts omit it → we fall back to the well-known default).
 */
data class DiscoveredHost(
    /** Service-instance name — a stable id that survives transient drops. */
    val serviceName: String,
    /** Resolved IPv4 (preferred) or first address. */
    val host: String,
    /** Advertised port = the host's pairing listener (default 4432). */
    val pairPort: Int,
    /** Stream port from the `stream` TXT, or the default when absent. */
    val streamPort: Int,
    /** `pubkey` TXT (hex), pinned on pair. Null ⇒ pair before we can pin it. */
    val pubkeyHex: String?,
    /** `alias` TXT — the host's self-declared friendly name, if any. */
    val alias: String?,
) {
    /** The host's own name minus the " (pharos)" suffix, or null if it's just the IP. */
    private fun hostnameLabel(): String? =
        serviceName.removeSuffix(" (pharos)").trim().takeIf { it.isNotEmpty() && it != host }

    /** Friendly display name: alias > instance name > IP. */
    fun displayName(): String =
        alias?.takeIf { it.isNotBlank() } ?: hostnameLabel() ?: host

    /**
     * Promote a fresh discovery into a saved [ServerEntry], auto-configuring
     * IP, stream port, pairing port, pubkey, and a friendly name from what the
     * host advertised — the user never types an address. `paired` stays false:
     * the pubkey is pinned by the SPAKE2 handshake, not by discovery.
     */
    fun toServerEntry(): ServerEntry = ServerEntry(
        id = ServerEntry.makeId(host, streamPort),
        host = host,
        port = streamPort,
        alias = alias?.takeIf { it.isNotBlank() },
        hostnameCache = hostnameLabel(),
        pairPort = pairPort,
        hostPubkey = pubkeyHex,
    )
}

/**
 * LAN host discovery via Android's [NsdManager] — the platform's mDNS daemon,
 * so no `CHANGE_WIFI_MULTICAST` permission or `MulticastLock` is needed (and
 * no native build: this is pure Kotlin). Browses `_pharos._udp.`, resolves
 * each hit, and exposes the live set as a [StateFlow] the Hosts pane collects.
 *
 * Pre-API-34 `resolveService` allows only ONE resolve in flight, so resolves
 * are serialized through a small queue; a transient "already active" is
 * retried a few times. Discovery is best-effort — if NSD is unavailable the
 * flow simply stays empty and manual Add still works.
 */
class HostDiscovery(context: Context) {
    private val appContext = context.applicationContext
    private val nsd: NsdManager? =
        try {
            appContext.getSystemService(Context.NSD_SERVICE) as? NsdManager
        } catch (e: Exception) {
            Log.w(TAG, "NSD service unavailable; LAN discovery off", e)
            null
        }

    private val _hosts = MutableStateFlow<List<DiscoveredHost>>(emptyList())
    val hosts: StateFlow<List<DiscoveredHost>> = _hosts.asStateFlow()

    private val lock = Any()
    private val found = LinkedHashMap<String, DiscoveredHost>()
    private val resolveQueue = ArrayDeque<NsdServiceInfo>()
    private val retries = HashMap<String, Int>()
    private var resolving = false
    private var listener: NsdManager.DiscoveryListener? = null

    /** Begin browsing. Idempotent — a second call while active is a no-op. */
    fun start() {
        val nsd = nsd ?: return
        synchronized(lock) {
            if (listener != null) return
        }
        val l = object : NsdManager.DiscoveryListener {
            override fun onStartDiscoveryFailed(serviceType: String, errorCode: Int) {
                Log.w(TAG, "discovery start failed: $errorCode")
                synchronized(lock) { listener = null }
            }

            override fun onStopDiscoveryFailed(serviceType: String, errorCode: Int) {
                Log.w(TAG, "discovery stop failed: $errorCode")
            }

            override fun onDiscoveryStarted(serviceType: String) {
                Log.i(TAG, "mDNS discovery started for $serviceType")
            }

            override fun onDiscoveryStopped(serviceType: String) {
                Log.i(TAG, "mDNS discovery stopped")
            }

            override fun onServiceFound(info: NsdServiceInfo) {
                enqueueResolve(info)
            }

            override fun onServiceLost(info: NsdServiceInfo) {
                val name = info.serviceName ?: return
                synchronized(lock) {
                    found.remove(name)
                    retries.remove(name)
                }
                publish()
            }
        }
        synchronized(lock) { listener = l }
        try {
            nsd.discoverServices(SERVICE_TYPE, NsdManager.PROTOCOL_DNS_SD, l)
        } catch (e: Exception) {
            Log.w(TAG, "discoverServices threw; LAN discovery off", e)
            synchronized(lock) { listener = null }
        }
    }

    /** Stop browsing and clear the discovered set. */
    fun stop() {
        val nsd = nsd ?: return
        val l = synchronized(lock) {
            val cur = listener
            listener = null
            found.clear()
            resolveQueue.clear()
            retries.clear()
            resolving = false
            cur
        } ?: return
        try {
            nsd.stopServiceDiscovery(l)
        } catch (e: IllegalArgumentException) {
            // Listener already removed (e.g. start failed) — fine.
        }
        publish()
    }

    private fun enqueueResolve(info: NsdServiceInfo) {
        synchronized(lock) {
            resolveQueue.addLast(info)
        }
        pump()
    }

    /** Drain the resolve queue one at a time (pre-34 allows a single resolve). */
    // resolveService is deprecated in API 34 (favouring registerServiceInfoCallback)
    // but still works and is the only path on our minSdk-26 → API-33 devices.
    @Suppress("DEPRECATION")
    private fun pump() {
        val nsd = nsd ?: return
        val next = synchronized(lock) {
            if (resolving) return
            val n = resolveQueue.removeFirstOrNull() ?: return
            resolving = true
            n
        }
        try {
            nsd.resolveService(next, resolveListener(next))
        } catch (e: Exception) {
            Log.w(TAG, "resolveService threw", e)
            synchronized(lock) { resolving = false }
            pump()
        }
    }

    private fun resolveListener(requested: NsdServiceInfo) = object : NsdManager.ResolveListener {
        override fun onResolveFailed(info: NsdServiceInfo, errorCode: Int) {
            val name = info.serviceName ?: requested.serviceName
            // FAILURE_ALREADY_ACTIVE (3) is the classic pre-34 race — retry a few times.
            if (errorCode == NsdManager.FAILURE_ALREADY_ACTIVE && name != null) {
                val attempt = synchronized(lock) {
                    val a = (retries[name] ?: 0) + 1
                    retries[name] = a
                    a
                }
                if (attempt <= MAX_RESOLVE_RETRIES) {
                    synchronized(lock) {
                        resolveQueue.addLast(info)
                        resolving = false
                    }
                    pump()
                    return
                }
            }
            Log.w(TAG, "resolve failed for ${name ?: "?"}: $errorCode")
            synchronized(lock) { resolving = false }
            pump()
        }

        override fun onServiceResolved(info: NsdServiceInfo) {
            val host = pickAddress(info)
            val name = info.serviceName
            if (host != null && name != null) {
                val txt = info.attributes ?: emptyMap()
                fun txtStr(key: String): String? =
                    txt[key]?.let { String(it, Charsets.UTF_8) }?.takeIf { it.isNotEmpty() }
                val streamPort = txtStr("stream")?.toIntOrNull() ?: DEFAULT_STREAM_PORT
                val discovered = DiscoveredHost(
                    serviceName = name,
                    host = host,
                    pairPort = info.port,
                    streamPort = streamPort,
                    pubkeyHex = txtStr("pubkey"),
                    alias = txtStr("alias"),
                )
                synchronized(lock) {
                    found[name] = discovered
                    retries.remove(name)
                }
                publish()
            }
            synchronized(lock) { resolving = false }
            pump()
        }
    }

    private fun publish() {
        val snapshot = synchronized(lock) { found.values.toList() }
        _hosts.value = snapshot
    }

    /** Prefer an IPv4 address (friendlier for the QUIC bind), else the first. */
    private fun pickAddress(info: NsdServiceInfo): String? {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.UPSIDE_DOWN_CAKE) {
            val addrs = info.hostAddresses
            val v4 = addrs.firstOrNull { it is Inet4Address }
            return (v4 ?: addrs.firstOrNull())?.hostAddress
        }
        @Suppress("DEPRECATION")
        return info.host?.hostAddress
    }

    companion object {
        private const val TAG = "HostDiscovery"
        private const val SERVICE_TYPE = "_pharos._udp."
        // Mirrors core_proto::ports::STREAM — the single deliberate connect port.
        private const val DEFAULT_STREAM_PORT = 15777
        private const val MAX_RESOLVE_RETRIES = 4
    }
}
