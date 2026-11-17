// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The Pharos/Trireme authors

package io.github.cursednumber.trireme.ui

import android.graphics.BitmapFactory
import androidx.compose.foundation.Image
import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.border
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.DropdownMenu
import androidx.compose.material3.DropdownMenuItem
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.focus.onFocusChanged
import androidx.compose.ui.graphics.ImageBitmap
import androidx.compose.ui.graphics.asImageBitmap
import androidx.compose.ui.layout.ContentScale
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.semantics.Role
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.role
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import io.github.cursednumber.trireme.R
import uniffi.core_ffi.AppInfo

/**
 * A single box-art launcher tile. Lifted from the old [io.github.cursednumber.trireme.AppsOverlay]
 * and **re-themed**: the hardcoded near-black surface / parchment hex are gone — every
 * colour now flows from [MaterialTheme.colorScheme] / [TriremeTokens] so the tile recolors
 * with the system dark/light setting (the navbar floats over live video, so a fixed scheme
 * would clash in light mode).
 *
 * TV-focus rule (hard-won — see lesson-compose-tv-focus): the **tile itself** is the single
 * focusable + clickable target; we never nest a Card/clickable inside, which would trap the
 * D-pad. Focus is styled by the shared [triremeFocus] so every focusable in the shell matches.
 */
@Composable
fun AppTile(
    app: AppInfo,
    modifier: Modifier = Modifier,
    onLaunch: () -> Unit,
    onSwitchTo: () -> Unit = {},
    onEndProcess: () -> Unit = {},
) {
    var focused by remember { mutableStateOf(false) }
    // Running tiles open a manage menu (Switch To / End process) instead of
    // launching a duplicate; idle tiles launch on click.
    var menuOpen by remember { mutableStateOf(false) }
    val running = app.running
    // If the app stops running (exited / killed) while its menu is open, close it —
    // else the menu lingers on a now-idle tile and its actions become host no-ops.
    LaunchedEffect(running) { if (!running) menuOpen = false }
    // a11y: box art carries the meaning, so the tile is described by what it does.
    val launchDesc = stringResource(R.string.apps_tile_desc, app.name)
    val desc = if (running) stringResource(R.string.apps_tile_running_desc, app.name) else launchDesc
    Column(
        modifier
            .fillMaxWidth()
            .clip(RoundedCornerShape(TriremeTokens.tileCorner))
            .background(
                // Subtle elevated container; brighter when focused (the gold ring from
                // triremeFocus does the heavy lifting, this just lifts the fill).
                if (focused) MaterialTheme.colorScheme.surfaceContainerHighest
                else MaterialTheme.colorScheme.surfaceContainer,
            )
            // Running apps wear a primary-tinted border so they read as "live" at a glance.
            .then(
                if (running) {
                    Modifier.border(
                        2.dp,
                        MaterialTheme.colorScheme.primary,
                        RoundedCornerShape(TriremeTokens.tileCorner),
                    )
                } else {
                    Modifier
                },
            )
            .onFocusChanged { focused = it.isFocused }
            // `.clickable` is itself focusable — the tile is the single D-pad target, so the
            // remote never lands on an inner element.
            .clickable { if (running) menuOpen = true else onLaunch() }
            .triremeFocus(focused)
            .semantics {
                role = Role.Button
                contentDescription = desc
            }
            .padding(8.dp),
        horizontalAlignment = Alignment.CenterHorizontally,
    ) {
        Box(
            Modifier
                .fillMaxWidth()
                .height(190.dp)
                .clip(RoundedCornerShape(8.dp))
                .background(MaterialTheme.colorScheme.surfaceVariant),
            contentAlignment = Alignment.Center,
        ) {
            val art = remember(app.art) { decodeArt(app.art) }
            if (art != null) {
                Image(
                    bitmap = art,
                    // The Column already carries the launch description; the inner image is
                    // decorative relative to that node.
                    contentDescription = null,
                    contentScale = ContentScale.Crop,
                    modifier = Modifier.fillMaxSize().clip(RoundedCornerShape(8.dp)),
                )
            } else {
                Text(
                    kindGlyph(app.kind),
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                    style = MaterialTheme.typography.headlineLarge,
                )
            }
            // "RUNNING" pill, top-start over the art.
            if (running) {
                Text(
                    stringResource(R.string.apps_tile_running_badge),
                    color = MaterialTheme.colorScheme.onPrimary,
                    style = MaterialTheme.typography.labelSmall,
                    fontWeight = FontWeight.Bold,
                    modifier = Modifier
                        .align(Alignment.TopStart)
                        .padding(6.dp)
                        .clip(RoundedCornerShape(4.dp))
                        .background(MaterialTheme.colorScheme.primary)
                        .padding(horizontal = 6.dp, vertical = 2.dp),
                )
            }
        }
        Spacer(Modifier.height(6.dp))
        Text(
            app.name,
            color = MaterialTheme.colorScheme.onSurface,
            maxLines = 1,
            overflow = TextOverflow.Ellipsis,
            fontWeight = if (focused) FontWeight.SemiBold else FontWeight.Normal,
            style = MaterialTheme.typography.bodyMedium,
        )
        // Manage menu for a running app, anchored to the tile. D-pad navigable
        // (it's a focusable Popup); dismiss returns focus to the tile.
        DropdownMenu(expanded = menuOpen, onDismissRequest = { menuOpen = false }) {
            DropdownMenuItem(
                text = { Text(stringResource(R.string.apps_action_switch_to)) },
                onClick = {
                    menuOpen = false
                    onSwitchTo()
                },
            )
            // The Desktop tile is "running" while the host mirrors its real desktop;
            // relaunching it means nothing, and End returns to the virtual display.
            val desktop = app.kind.toInt() == APP_KIND_DESKTOP
            if (!desktop) {
                DropdownMenuItem(
                    text = { Text(stringResource(R.string.apps_action_relaunch)) },
                    onClick = {
                        menuOpen = false
                        onLaunch()
                    },
                )
            }
            DropdownMenuItem(
                text = {
                    Text(
                        stringResource(
                            if (desktop) R.string.apps_action_end_desktop else R.string.apps_action_end_process,
                        ),
                    )
                },
                onClick = {
                    menuOpen = false
                    onEndProcess()
                },
            )
        }
    }
}

/**
 * Decode box-art bytes (PNG/JPEG) to an [ImageBitmap], or `null` for a placeholder
 * (empty/undecodable). Unchanged from the old overlay.
 */
fun decodeArt(art: ByteArray): ImageBitmap? {
    if (art.isEmpty()) return null
    return runCatching {
        BitmapFactory.decodeByteArray(art, 0, art.size)?.asImageBitmap()
    }.getOrNull()
}

/** core_proto::apps::app_kind::DESKTOP. */
private const val APP_KIND_DESKTOP = 2

/** Placeholder glyph by kind tag (mirrors core_proto::apps::app_kind). */
fun kindGlyph(kind: UByte): String = when (kind.toInt()) {
    1 -> "🎮" // game 🎮
    2 -> "🖥️" // desktop 🖥️
    3 -> "🎬" // media 🎬
    else -> "▢" // app ▢
}
