package dev.pharos.trireme.ui.panes

import android.os.Build
import androidx.compose.foundation.ExperimentalFoundationApi
import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.combinedClickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.Add
import androidx.compose.material.icons.filled.MoreVert
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.DropdownMenu
import androidx.compose.material3.DropdownMenuItem
import androidx.compose.material3.Icon
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.focus.onFocusChanged
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.semantics.Role
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.liveRegion
import androidx.compose.ui.semantics.LiveRegionMode
import androidx.compose.ui.semantics.role
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.compose.runtime.collectAsState
import dev.pharos.trireme.DiscoveredHost
import dev.pharos.trireme.HostDiscovery
import dev.pharos.trireme.R
import dev.pharos.trireme.Registry
import dev.pharos.trireme.ServerEntry
import dev.pharos.trireme.ServerStore
import dev.pharos.trireme.stream.ConnStatus
import dev.pharos.trireme.stream.HostConn
import dev.pharos.trireme.stream.StreamHub
import dev.pharos.trireme.ui.AddOrEditDialog
import dev.pharos.trireme.ui.PairDialog
import dev.pharos.trireme.ui.TriremeTokens
import dev.pharos.trireme.ui.triremeFocus
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import uniffi.core_ffi.PairResult
import uniffi.core_ffi.clientPair

/**
 * Hosts pane — the host picker migrated from `ServerSelectorActivity`. Lists the remembered
 * [ServerStore] hosts as focusable ROWS (never clickable cards — see lesson-compose-tv-focus:
 * a clickable container traps the D-pad so inner controls are unreachable by the remote). Each
 * row's primary OK action is connect-and-make-active via [StreamHub]; per-row overflow exposes
 * set-active / disconnect / pair / edit / delete. Live [StreamHub.connections] status drives a
 * per-row badge (a polite live region so TalkBack announces Connecting→Live→Reconnecting).
 *
 * The pane owns the registry mutation locally (add/edit/delete) and persists through
 * [ServerStore]; [StreamHub] owns the live sessions.
 */
@Composable
fun HostsPane(store: ServerStore) {
    val context = LocalContext.current
    val scope = rememberCoroutineScope()

    // Registry is the persisted host list; StreamHub holds the live session status.
    var registry by remember { mutableStateOf(store.load()) }
    val connections by StreamHub.connections.collectAsState()
    val activeHostId by StreamHub.activeHostId.collectAsState()

    // LAN auto-discovery (mDNS via NsdManager). Scoped to this pane: browse while
    // the host picker is on-screen, stop when it leaves so we don't keep the
    // radio busy app-wide. Degrades to an empty list if NSD is unavailable.
    val discovery = remember { HostDiscovery(context) }
    DisposableEffect(discovery) {
        discovery.start()
        onDispose { discovery.stop() }
    }
    val discovered by discovery.hosts.collectAsState()

    fun commit(reg: Registry) {
        registry = reg
        store.save(reg)
    }

    // Dialog targets.
    var addOpen by rememberSaveable { mutableStateOf(false) }
    var editTarget by remember { mutableStateOf<ServerEntry?>(null) }
    var deleteTarget by remember { mutableStateOf<String?>(null) }
    var pairTarget by remember { mutableStateOf<ServerEntry?>(null) }

    // Discovered hosts not already saved (dedup by the stream-port id we'd
    // persist). These render below the saved list; tapping one auto-configures
    // the entry from what the host advertised, then opens the Pair dialog.
    val newDiscoveries = discovered.filter { d ->
        registry.find(ServerEntry.makeId(d.host, d.streamPort)) == null
    }

    fun addAndPair(d: DiscoveredHost) {
        val entry = d.toServerEntry()
        commit(registry.upsert(entry))
        pairTarget = entry
    }

    // Horizontal inset lives INSIDE the scrollers (header padding + LazyColumn
    // contentPadding) — not on this outer Column — so a focused row's 1.04x grow +
    // gold border has room within the LazyColumn's clip and isn't cut off left/right.
    Column(Modifier.fillMaxSize().padding(vertical = 20.dp)) {
        // --- Header + Add row -------------------------------------------------
        Row(
            verticalAlignment = Alignment.CenterVertically,
            modifier = Modifier.fillMaxWidth().padding(horizontal = 20.dp),
        ) {
            Column(Modifier.weight(1f)) {
                Text(
                    stringResource(R.string.hosts_title),
                    style = MaterialTheme.typography.headlineSmall,
                    fontWeight = FontWeight.Bold,
                    color = MaterialTheme.colorScheme.onSurface,
                )
                Text(
                    stringResource(R.string.hosts_subtitle),
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
            }
            AddHostButton(onClick = { addOpen = true })
        }
        Spacer(Modifier.height(16.dp))

        if (registry.servers.isEmpty() && newDiscoveries.isEmpty()) {
            Column(
                Modifier.fillMaxSize().padding(horizontal = 20.dp),
                horizontalAlignment = Alignment.CenterHorizontally,
                verticalArrangement = Arrangement.Center,
            ) {
                Text(
                    stringResource(R.string.hosts_empty_title),
                    style = MaterialTheme.typography.titleMedium,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
                Spacer(Modifier.height(8.dp))
                Text(
                    stringResource(R.string.hosts_empty_hint),
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
            }
        } else {
            LazyColumn(
                // Horizontal padding here (not on the outer Column) keeps rows aligned with
                // the header while giving the focus grow/border room inside the clip.
                contentPadding = PaddingValues(horizontal = 20.dp, vertical = 8.dp),
                verticalArrangement = Arrangement.spacedBy(8.dp),
                modifier = Modifier.fillMaxSize(),
            ) {
                items(registry.servers, key = { it.id }) { server ->
                    val conn = connections.firstOrNull { it.hostId == server.id }
                    HostRow(
                        server = server,
                        conn = conn,
                        isActive = activeHostId == server.id,
                        onConnect = { StreamHub.connect(server, makeActive = true) },
                        onSetActive = { StreamHub.setActive(server.id) },
                        onDisconnect = { StreamHub.disconnect(server.id) },
                        onPair = { pairTarget = server },
                        onEdit = { editTarget = server },
                        onDelete = { deleteTarget = server.id },
                    )
                }

                // Auto-discovered hosts (mDNS) not yet saved. A header separates
                // them from the remembered list; each row adds + opens Pair on tap.
                if (newDiscoveries.isNotEmpty()) {
                    item(key = "discovered-header") { DiscoveredSectionHeader() }
                    items(newDiscoveries, key = { "disc:${it.serviceName}" }) { d ->
                        DiscoveredRow(host = d, onAdd = { addAndPair(d) })
                    }
                }
            }
        }
    }

    // --- Dialogs --------------------------------------------------------------
    if (addOpen) {
        AddOrEditDialog(
            title = stringResource(R.string.hosts_dialog_add_title),
            initialHost = "",
            initialPort = "15777", // the single deliberate Pharos connect port
            initialAlias = "",
            confirmLabel = stringResource(R.string.hosts_dialog_add_confirm),
            onDismiss = { addOpen = false },
            onConfirm = { host, port, alias, extHost, extPort ->
                val id = ServerEntry.makeId(host, port)
                val entry = registry.find(id)?.copy(
                    alias = alias.ifEmpty { null },
                    externalHost = extHost.ifEmpty { null },
                    externalPort = extPort,
                ) ?: ServerEntry(
                    id = id,
                    host = host,
                    port = port,
                    alias = alias.ifEmpty { null },
                    externalHost = extHost.ifEmpty { null },
                    externalPort = extPort,
                )
                commit(registry.upsert(entry))
                addOpen = false
            },
        )
    }

    editTarget?.let { target ->
        AddOrEditDialog(
            title = stringResource(R.string.hosts_dialog_edit_title),
            initialHost = target.host,
            initialPort = target.port.toString(),
            initialAlias = target.alias.orEmpty(),
            initialExternalHost = target.externalHost.orEmpty(),
            initialExternalPort = target.externalPort?.toString().orEmpty(),
            confirmLabel = stringResource(R.string.action_save),
            onDismiss = { editTarget = null },
            onConfirm = { host, port, alias, extHost, extPort ->
                // host/port change ⇒ id changes; preserve paired/cache.
                val newId = ServerEntry.makeId(host, port)
                val merged = target.copy(
                    id = newId,
                    host = host,
                    port = port,
                    alias = alias.ifEmpty { null },
                    externalHost = extHost.ifEmpty { null },
                    externalPort = extPort,
                )
                val without = registry.remove(target.id)
                commit(without.upsert(merged))
                editTarget = null
            },
        )
    }

    deleteTarget?.let { id ->
        val name = registry.find(id)?.friendlyName().orEmpty()
        AlertDialog(
            onDismissRequest = { deleteTarget = null },
            title = { Text(stringResource(R.string.hosts_delete_title)) },
            text = { Text(stringResource(R.string.hosts_delete_body, name)) },
            confirmButton = {
                TextButton(onClick = {
                    // Drop any live session before forgetting the host.
                    StreamHub.disconnect(id)
                    commit(registry.remove(id))
                    deleteTarget = null
                }) { Text(stringResource(R.string.hosts_delete)) }
            },
            dismissButton = {
                TextButton(onClick = { deleteTarget = null }) {
                    Text(stringResource(R.string.action_cancel))
                }
            },
        )
    }

    pairTarget?.let { target ->
        PairDialog(
            target = target,
            onDismiss = { pairTarget = null },
            onPair = { pin, cb ->
                // Run clientPair off the main thread (same handshake as the desktop), then
                // persist the pinned pubkey on success.
                scope.launch {
                    val res: Result<PairResult> = withContext(Dispatchers.IO) {
                        runCatching {
                            clientPair(
                                addr = "${target.host}:${target.pairPortOrDefault()}",
                                altAddrs = target.pairAltAddrs(),
                                pin = pin,
                                identityPath = store.identityPath(context),
                                ourAlias = Build.MODEL,
                            )
                        }
                    }
                    cb(res)
                    res.getOrNull()?.let { ok ->
                        commit(
                            registry.upsert(
                                target.copy(
                                    paired = true,
                                    hostPubkey = ok.hostPubkeyHex,
                                    pairPort = target.pairPortOrDefault(),
                                    alias = target.alias ?: ok.hostAlias,
                                ),
                            ),
                        )
                    }
                }
            },
        )
    }
}

/**
 * One host row: TWO focusable siblings inside a NON-clickable container — the card body and the
 * trailing ⋮ button. D-pad LEFT/RIGHT moves between them, UP/DOWN between rows. This is the
 * correct TV pattern for a row with two distinct actions, and is NOT the focus trap from
 * lesson-compose-tv-focus: that trap is a focusable *nested inside* a clickable parent, which
 * makes the inner control unreachable. Two focusable *siblings* navigate fine.
 *   - Body: OK / tap connects + makes active; an OK-hold / touch long-press is a shortcut to the
 *     overflow (so touch users keep the long-press affordance they had before).
 *   - ⋮ button: focus it + OK to open the per-host overflow (set-active / disconnect / pair /
 *     edit / delete) — now reachable by remote without having to discover the long-press.
 * Once the menu opens, its `DropdownMenuItem`s are focusable, so every action is remote-navigable.
 */
@OptIn(ExperimentalFoundationApi::class)
@Composable
private fun HostRow(
    server: ServerEntry,
    conn: HostConn?,
    isActive: Boolean,
    onConnect: () -> Unit,
    onSetActive: () -> Unit,
    onDisconnect: () -> Unit,
    onPair: () -> Unit,
    onEdit: () -> Unit,
    onDelete: () -> Unit,
) {
    var bodyFocused by remember { mutableStateOf(false) }
    var moreFocused by remember { mutableStateOf(false) }
    var menuOpen by remember { mutableStateOf(false) }
    val moreDesc = stringResource(R.string.hosts_more_desc, server.friendlyName())

    Row(
        modifier = Modifier
            .fillMaxWidth()
            .heightIn(min = TriremeTokens.minTarget)
            .clip(RoundedCornerShape(TriremeTokens.tileCorner))
            .background(MaterialTheme.colorScheme.surfaceContainer),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        // Card BODY = primary focusable: OK / tap connects + makes active; an OK-hold / touch
        // long-press is a shortcut to the overflow. Sibling of the trailing ⋮ button (below),
        // NOT a container wrapping it — so neither traps the D-pad (see the [HostRow] note).
        Column(
            Modifier
                .weight(1f)
                .onFocusChanged { bodyFocused = it.isFocused }
                .combinedClickable(
                    onClick = onConnect,
                    onClickLabel = stringResource(R.string.hosts_connect),
                    onLongClick = { menuOpen = true },
                    onLongClickLabel = moreDesc,
                )
                .triremeFocus(bodyFocused)
                .semantics { role = Role.Button }
                .padding(horizontal = 14.dp, vertical = 10.dp),
        ) {
            Row(verticalAlignment = Alignment.CenterVertically) {
                Text(
                    server.friendlyName(),
                    fontWeight = FontWeight.SemiBold,
                    style = MaterialTheme.typography.titleMedium,
                    color = MaterialTheme.colorScheme.onSurface,
                    maxLines = 1,
                    overflow = TextOverflow.Ellipsis,
                )
                if (isActive) {
                    Spacer(Modifier.width(8.dp))
                    Badge(stringResource(R.string.hosts_status_active), MaterialTheme.colorScheme.primary)
                }
            }
            Spacer(Modifier.height(2.dp))
            // Overt card hides the port (it's a single deliberate value, shown in
            // Edit) — host only. WAN fallback host shown when configured.
            Text(
                server.host,
                style = MaterialTheme.typography.bodySmall,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )
            server.externalHost?.takeIf { it.isNotBlank() }?.let { wanHost ->
                Text(
                    "WAN: $wanHost",
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
            }
            Spacer(Modifier.height(6.dp))
            Row(verticalAlignment = Alignment.CenterVertically) {
                // Pairing badge.
                val pairLabel = if (server.paired) {
                    stringResource(R.string.hosts_paired)
                } else {
                    stringResource(R.string.hosts_unpaired)
                }
                val pairColor =
                    if (server.paired) MaterialTheme.colorScheme.tertiary
                    else MaterialTheme.colorScheme.secondary
                Badge(pairLabel, pairColor)

                // Live connection status — a polite live region for screen readers.
                conn?.let {
                    Spacer(Modifier.width(8.dp))
                    val (label, color) = statusLabelColor(it.status)
                    val announce = stringResource(
                        R.string.hosts_status_announce, server.friendlyName(), label,
                    )
                    Text(
                        label,
                        style = MaterialTheme.typography.bodySmall,
                        color = color,
                        modifier = Modifier.semantics {
                            liveRegion = LiveRegionMode.Polite
                            contentDescription = announce
                        },
                    )
                }
                server.pace?.let {
                    Spacer(Modifier.width(8.dp))
                    Text(
                        it,
                        style = MaterialTheme.typography.bodySmall,
                        color = MaterialTheme.colorScheme.onSurfaceVariant,
                    )
                }
            }
        }

        // Trailing overflow — its OWN focusable button, reachable by D-pad RIGHT from the body,
        // opening the per-host menu. (Sibling of the body, not nested in a clickable, so no
        // D-pad trap — see the [HostRow] note.) The Box anchors the DropdownMenu it drops from.
        Box {
            OverflowButton(
                desc = moreDesc,
                focused = moreFocused,
                onFocusChanged = { moreFocused = it },
                onClick = { menuOpen = true },
            )
            DropdownMenu(expanded = menuOpen, onDismissRequest = { menuOpen = false }) {
                val connected = conn != null && conn.status != ConnStatus.Disconnected &&
                    conn.status != ConnStatus.Failed && conn.status != ConnStatus.Preempted
                if (connected && !isActive) {
                    DropdownMenuItem(
                        text = { Text(stringResource(R.string.hosts_set_active)) },
                        onClick = { menuOpen = false; onSetActive() },
                    )
                }
                if (connected) {
                    DropdownMenuItem(
                        text = { Text(stringResource(R.string.hosts_disconnect)) },
                        onClick = { menuOpen = false; onDisconnect() },
                    )
                }
                DropdownMenuItem(
                    text = {
                        Text(
                            if (server.paired) stringResource(R.string.hosts_repair)
                            else stringResource(R.string.hosts_pair),
                        )
                    },
                    onClick = { menuOpen = false; onPair() },
                )
                DropdownMenuItem(
                    text = { Text(stringResource(R.string.hosts_edit)) },
                    onClick = { menuOpen = false; onEdit() },
                )
                DropdownMenuItem(
                    text = { Text(stringResource(R.string.hosts_delete)) },
                    onClick = { menuOpen = false; onDelete() },
                )
            }
        }
    }
}

/**
 * The trailing ⋮ overflow button — its OWN focusable, reachable by D-pad RIGHT from the card
 * body (they are focusable siblings, not nested, so no D-pad trap — see [HostRow]). Focus it and
 * press OK to open the per-host menu. [desc] names the affordance for TalkBack; the gold focus
 * ring ([Modifier.triremeFocus]) shows when it holds focus.
 */
@Composable
private fun OverflowButton(
    desc: String,
    focused: Boolean,
    onFocusChanged: (Boolean) -> Unit,
    onClick: () -> Unit,
) {
    Box(
        Modifier
            .size(TriremeTokens.minTarget)
            .clip(RoundedCornerShape(TriremeTokens.tileCorner))
            .onFocusChanged { onFocusChanged(it.isFocused) }
            .clickable(onClick = onClick)
            .triremeFocus(focused)
            .semantics {
                role = Role.Button
                contentDescription = desc
            },
        contentAlignment = Alignment.Center,
    ) {
        Icon(
            Icons.Filled.MoreVert,
            contentDescription = null, // described by the Box's semantics
            tint = MaterialTheme.colorScheme.onSurfaceVariant,
        )
    }
}

@Composable
private fun AddHostButton(onClick: () -> Unit) {
    var focused by remember { mutableStateOf(false) }
    val desc = stringResource(R.string.hosts_add_desc)
    Row(
        Modifier
            .heightIn(min = TriremeTokens.minTarget)
            .clip(RoundedCornerShape(TriremeTokens.tileCorner))
            .background(MaterialTheme.colorScheme.primaryContainer)
            .onFocusChanged { focused = it.isFocused }
            .clickable(onClick = onClick)
            .triremeFocus(focused)
            .semantics {
                role = Role.Button
                contentDescription = desc
            }
            .padding(horizontal = 14.dp, vertical = 8.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Icon(
            Icons.Filled.Add,
            contentDescription = null, // labelled by the row's contentDescription
            tint = MaterialTheme.colorScheme.onPrimaryContainer,
        )
        Spacer(Modifier.width(6.dp))
        Text(
            stringResource(R.string.hosts_add),
            color = MaterialTheme.colorScheme.onPrimaryContainer,
            style = MaterialTheme.typography.labelLarge,
        )
    }
}

/** Section label above the auto-discovered hosts. */
@Composable
private fun DiscoveredSectionHeader() {
    Text(
        stringResource(R.string.hosts_discovered_title),
        style = MaterialTheme.typography.labelLarge,
        color = MaterialTheme.colorScheme.onSurfaceVariant,
        modifier = Modifier.padding(top = 12.dp, bottom = 2.dp),
    )
}

/**
 * One mDNS-discovered host: a single focusable, clickable ROW (not a card
 * wrapping inner controls — see lesson-compose-tv-focus). OK / tap adds it to
 * the registry (auto-configured from the advert) and opens the Pair dialog.
 * The "Discovered" badge distinguishes it from saved hosts; the IP is shown so
 * the user can tell two hosts apart before adding.
 */
@Composable
private fun DiscoveredRow(host: DiscoveredHost, onAdd: () -> Unit) {
    var focused by remember { mutableStateOf(false) }
    val desc = stringResource(R.string.hosts_discovered_desc, host.displayName())
    Row(
        modifier = Modifier
            .fillMaxWidth()
            .heightIn(min = TriremeTokens.minTarget)
            .clip(RoundedCornerShape(TriremeTokens.tileCorner))
            .background(MaterialTheme.colorScheme.surfaceContainer)
            .onFocusChanged { focused = it.isFocused }
            .clickable(onClick = onAdd)
            .triremeFocus(focused)
            .semantics {
                role = Role.Button
                contentDescription = desc
            }
            .padding(horizontal = 14.dp, vertical = 10.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Column(Modifier.weight(1f)) {
            Text(
                host.displayName(),
                fontWeight = FontWeight.SemiBold,
                style = MaterialTheme.typography.titleMedium,
                color = MaterialTheme.colorScheme.onSurface,
                maxLines = 1,
                overflow = TextOverflow.Ellipsis,
            )
            Spacer(Modifier.height(2.dp))
            Text(
                host.host,
                style = MaterialTheme.typography.bodySmall,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )
        }
        Badge(stringResource(R.string.hosts_discovered_badge), MaterialTheme.colorScheme.primary)
    }
}

/** A small pill badge — text on a faint surface, coloured by [color]. */
@Composable
private fun Badge(text: String, color: Color) {
    Box(
        Modifier
            .clip(RoundedCornerShape(6.dp))
            .background(MaterialTheme.colorScheme.surfaceVariant)
            .padding(horizontal = 6.dp, vertical = 2.dp),
    ) {
        Text(text, color = color, style = MaterialTheme.typography.labelSmall)
    }
}

/** Map a [ConnStatus] to its label + a themed colour. */
@Composable
private fun statusLabelColor(status: ConnStatus): Pair<String, Color> = when (status) {
    ConnStatus.Connecting -> stringResource(R.string.hosts_status_connecting) to
        MaterialTheme.colorScheme.onSurfaceVariant
    ConnStatus.Live -> stringResource(R.string.hosts_status_live) to
        MaterialTheme.colorScheme.tertiary
    ConnStatus.Reconnecting -> stringResource(R.string.hosts_status_reconnecting) to
        MaterialTheme.colorScheme.primary
    ConnStatus.Failed -> stringResource(R.string.hosts_status_failed) to
        MaterialTheme.colorScheme.error
    ConnStatus.Disconnected -> stringResource(R.string.hosts_status_disconnected) to
        MaterialTheme.colorScheme.onSurfaceVariant
    ConnStatus.Preempted -> stringResource(R.string.hosts_status_preempted) to
        MaterialTheme.colorScheme.onSurfaceVariant
}
