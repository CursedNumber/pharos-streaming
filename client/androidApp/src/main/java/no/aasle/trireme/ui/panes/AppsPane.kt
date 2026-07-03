// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The Pharos/Trireme authors

package no.aasle.trireme.ui.panes

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.lazy.grid.GridCells
import androidx.compose.foundation.lazy.grid.GridItemSpan
import androidx.compose.foundation.lazy.grid.LazyVerticalGrid
import androidx.compose.foundation.lazy.grid.items
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.semantics.LiveRegionMode
import androidx.compose.ui.semantics.liveRegion
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import androidx.compose.runtime.collectAsState
import no.aasle.trireme.R
import no.aasle.trireme.stream.StreamHub
import no.aasle.trireme.ui.AppTile
import uniffi.core_ffi.AppInfo

/**
 * Apps pane — the POOLED box-art launcher. Renders [StreamHub.pooledApps] grouped by host: a
 * host header (the host's friendly name) followed by that host's box-art grid. Selecting a tile
 * routes the launch to that host's session ([StreamHub.launchApp], which also makes the host the
 * active stream) and then closes the navbar back to the stream via [onCloseToStream].
 *
 * One adaptive grid spans all hosts so D-pad traversal flows naturally header → tiles → next
 * header; per-host headers occupy a full grid line (max-line span).
 */
@Composable
fun AppsPane(onCloseToStream: () -> Unit) {
    val pooled by StreamHub.pooledApps.collectAsState()
    // Per-app last-used time → recently-used apps float (and stay floated even
    // after they stop running, below the currently-running ones).
    val recency by StreamHub.appRecency.collectAsState()

    // Horizontal inset lives inside the header padding + grid contentPadding (not on this
    // outer Column) so a focused tile's 1.04x grow + gold border has room within the grid's
    // clip and isn't cut off at the left/right edges.
    Column(Modifier.fillMaxSize().padding(vertical = 20.dp)) {
        Text(
            stringResource(R.string.apps_title),
            style = MaterialTheme.typography.headlineSmall,
            fontWeight = FontWeight.Bold,
            color = MaterialTheme.colorScheme.onSurface,
            modifier = Modifier.padding(horizontal = 20.dp),
        )
        Text(
            stringResource(R.string.apps_subtitle),
            style = MaterialTheme.typography.bodySmall,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
            modifier = Modifier.padding(horizontal = 20.dp),
        )
        Spacer(Modifier.height(16.dp))

        // Nothing connected (or no catalog yet anywhere): a single waiting line that updates
        // live as catalogs arrive (polite live region so TalkBack notes the change).
        val anyApps = pooled.any { it.apps.isNotEmpty() }
        if (pooled.isEmpty() || !anyApps) {
            Text(
                stringResource(R.string.apps_waiting),
                style = MaterialTheme.typography.bodyMedium,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
                modifier = Modifier
                    .padding(horizontal = 20.dp)
                    .semantics { liveRegion = LiveRegionMode.Polite },
            )
            return@Column
        }

        LazyVerticalGrid(
            columns = GridCells.Adaptive(minSize = 136.dp),
            // Horizontal padding inside the grid (vs the outer Column) gives edge tiles'
            // focus grow/border room within the clip; aligns with the header at 20dp.
            contentPadding = PaddingValues(start = 20.dp, end = 20.dp, top = 4.dp, bottom = 24.dp),
            horizontalArrangement = Arrangement.spacedBy(14.dp),
            verticalArrangement = Arrangement.spacedBy(14.dp),
            modifier = Modifier.fillMaxSize(),
        ) {
            pooled.forEach { group ->
                // Host header occupies a full grid line.
                item(
                    key = "header_${group.hostId}",
                    span = { GridItemSpan(maxLineSpan) },
                ) {
                    HostHeader(group.hostName)
                }
                if (group.apps.isEmpty()) {
                    item(
                        key = "empty_${group.hostId}",
                        span = { GridItemSpan(maxLineSpan) },
                    ) {
                        Text(
                            stringResource(R.string.apps_host_empty),
                            style = MaterialTheme.typography.bodySmall,
                            color = MaterialTheme.colorScheme.onSurfaceVariant,
                            modifier = Modifier.padding(vertical = 4.dp),
                        )
                    }
                } else {
                    // Order: running first, then most-recently-used, then catalog
                    // order (stable sort keeps ties in place). So a game you just
                    // played stays floated near the top after it stops, instead of
                    // dropping back down — and the reorder is stable enough not to
                    // yank D-pad focus on every running-flip.
                    val ordered = group.apps.sortedWith(
                        compareByDescending<AppInfo> { it.running }
                            .thenByDescending { recency[it.id] ?: 0L },
                    )
                    items(
                        count = ordered.size,
                        // Stable, host-scoped key so two hosts exposing the same app id
                        // never collide.
                        key = { idx -> "${group.hostId}/${ordered[idx].id}" },
                    ) { idx ->
                        val app = ordered[idx]
                        AppTile(
                            app = app,
                            onLaunch = {
                                StreamHub.launchApp(group.hostId, app.id)
                                onCloseToStream()
                            },
                            onSwitchTo = {
                                StreamHub.switchTo(group.hostId, app.id)
                                onCloseToStream()
                            },
                            onEndProcess = { StreamHub.endProcess(group.hostId, app.id) },
                        )
                    }
                }
            }
        }
    }
}

@Composable
private fun HostHeader(name: String) {
    Column(Modifier.fillMaxWidth().padding(top = 4.dp, bottom = 2.dp)) {
        Text(
            // The header is purely interpolation today, but goes through a resource so a
            // future "%1$s (%2$d apps)" form is a string-only change.
            stringResource(R.string.apps_host_header, name),
            style = MaterialTheme.typography.titleMedium,
            fontWeight = FontWeight.SemiBold,
            color = MaterialTheme.colorScheme.primary,
        )
        Spacer(Modifier.height(2.dp))
    }
}
