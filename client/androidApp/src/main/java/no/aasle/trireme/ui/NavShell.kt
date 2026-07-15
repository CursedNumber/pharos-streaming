// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The Pharos/Trireme authors

package no.aasle.trireme.ui

import androidx.compose.animation.AnimatedVisibility
import androidx.compose.animation.fadeIn
import androidx.compose.animation.fadeOut
import androidx.compose.animation.core.animateDpAsState
import androidx.compose.animation.slideInHorizontally
import androidx.compose.animation.slideOutHorizontally
import androidx.compose.foundation.Image
import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.BoxWithConstraints
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxHeight
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.filled.ExitToApp
import androidx.compose.material.icons.automirrored.filled.List
import androidx.compose.material.icons.filled.PlayArrow
import androidx.compose.material.icons.filled.Settings
import androidx.compose.material.icons.filled.SportsEsports
import androidx.compose.material.icons.filled.Apps
import androidx.compose.material3.Icon
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.runtime.withFrameNanos
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.focus.FocusRequester
import androidx.compose.ui.focus.focusRequester
import androidx.compose.ui.focus.onFocusChanged
import androidx.compose.ui.graphics.vector.ImageVector
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.semantics.Role
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.role
import androidx.compose.ui.semantics.selected
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.Dp
import androidx.compose.ui.unit.dp
import no.aasle.trireme.R

/** The navbar destinations, top → bottom in the left rail. Adding a new destination
 *  here is enough for the shell, the icon rail, and the back-handling to pick it up;
 *  add the matching pane in [content] (see `MainActivity.ShellContent`). */
enum class NavDest { Stream, Hosts, Apps, Controllers, Settings }

/**
 * Navbar visibility, a three-step ladder so the menu reveals progressively:
 *  - [Hidden]  — full-screen stream, no chrome.
 *  - [Peek]    — the icon RAIL only (quick access); auto-hides after a timeout if the
 *                user doesn't commit. A D-pad LEFT / OK / the touch edge-handle opens it.
 *  - [Full]    — rail + the active pane; no timer. Reached from [Peek] via LEFT or OK.
 * Closing (→ [Hidden]) is via Back, the Stream rail item, or a touch scrim-tap — NOT a
 * D-pad RIGHT, so a pane's own left/right navigation (e.g. the Apps grid columns) is
 * never stolen.
 */
enum class NavMode { Hidden, Peek, Full }

/** Width of the icon-only rail (the [NavMode.Peek] band width). Matches [NavRail]. */
val RAIL_WIDTH = 96.dp

/**
 * Responsive width of the navbar band (rail + pane) for the available screen width —
 * the single source of truth, used by both [NavShell] and the shell's dismiss-scrim so
 * they always agree. Adapts across resolutions/aspect ratios/orientations rather than a
 * fixed dp:
 *  - narrow (phone portrait, < 600dp): the menu takes the FULL width (no room to show
 *    the stream alongside a usable pane);
 *  - medium (tablet / landscape phone, 600–960dp): ~70%;
 *  - large (TV / desktop, ≥ 960dp): ~60%, capped, so the live stream stays visible to
 *    the right.
 */
fun navBandWidth(available: Dp): Dp = when {
    available < 600.dp -> available
    available < 960.dp -> available * 0.70f
    else -> (available * 0.60f).coerceAtMost(900.dp)
}

/** Reserved top strip for the brand logo so pane content never collides with it. */
private val LOGO_STRIP = 48.dp

/**
 * The left-edge navbar overlay + the active pane, floating over the (still-playing) live stream.
 *
 * Layout: a responsive band ([RAIL_WIDTH] in Peek, [navBandWidth] in Full) pinned to the left edge, backed by [navScrim]
 * (a near-opaque, mode-tuned wash so panel text stays legible over an arbitrary video frame —
 * we never assume the video is dark). Inside, a narrow icon+label RAIL of the four [NavDest]s,
 * and the active pane to its right; the Trireme logo sits in the TOP-RIGHT of the surface
 * (branding, matching the app icon). The whole thing slides in/out on [visible] over the stream,
 * which keeps rendering behind it.
 *
 * Selecting **Stream** (or otherwise leaving the navbar) calls [onCloseToStream]; selecting any
 * other dest calls [onSelect]. The shell (integration) owns the reveal state machine
 * (UP-at-top / LEFT-at-edge / Back) and drives [visible]/[current].
 *
 * TV-focus rule (lesson-compose-tv-focus): each rail item is its OWN focusable click target with
 * [triremeFocus]; we never wrap the rail in a single clickable that would trap the D-pad.
 */
@Composable
fun NavShell(
    mode: NavMode,
    current: NavDest,
    onSelect: (NavDest) -> Unit,
    onCloseToStream: () -> Unit,
    onExit: () -> Unit,
    content: @Composable (NavDest) -> Unit,
) {
    val full = mode == NavMode.Full
    // Focus target for the currently-selected rail item. When the navbar is revealed we
    // programmatically move focus onto it: the background is a bare SurfaceView with no
    // focusable Compose content, so without this the first D-pad press after opening has no
    // focus owner to traverse from and navigation is dead/nondeterministic on a 10-foot TV.
    val railFocus = remember { FocusRequester() }
    LaunchedEffect(mode != NavMode.Hidden) {
        if (mode != NavMode.Hidden) {
            withFrameNanos { }
            runCatching { railFocus.requestFocus() }
        }
        // Closing returns focus to the (focusless) content area implicitly — the shell
        // consumes the closing key, and the SurfaceView holds no focus to fight over.
    }

    // Full-screen overlay; the navbar itself is the left band, the rest stays transparent so the
    // stream shows through (the band is opaque via the scrim). BoxWithConstraints gives us the
    // real available width so the band sizes responsively (see navBandWidth) instead of a fixed dp.
    BoxWithConstraints(Modifier.fillMaxSize()) {
        // Peek shows only the icon rail (RAIL_WIDTH); Full grows to the responsive band and
        // reveals the active pane. Animate the width so Peek↔Full slides smoothly.
        val targetBand = if (full) navBandWidth(maxWidth) else RAIL_WIDTH
        val band by animateDpAsState(targetValue = targetBand, label = "navBand")
        AnimatedVisibility(
            visible = mode != NavMode.Hidden,
            // Slide the whole left band in from off-screen-left; fade smooths the edge.
            enter = slideInHorizontally(initialOffsetX = { -it }) + fadeIn(),
            exit = slideOutHorizontally(targetOffsetX = { -it }) + fadeOut(),
        ) {
            val railDesc = stringResource(R.string.nav_rail_desc)
            Box(
                Modifier
                    .width(band)   // animated band width; navScrim() adds full height + wash
                    .navScrim()
                    .semantics { contentDescription = railDesc },
            ) {
                Row(Modifier.fillMaxSize()) {
                    NavRail(
                        current = current,
                        focusRequester = railFocus,
                        onSelect = { dest ->
                            // Selecting Stream closes the navbar back to full-screen video.
                            if (dest == NavDest.Stream) onCloseToStream() else onSelect(dest)
                        },
                        onExit = onExit,
                    )
                    // The active pane only exists in Full; it fills the remaining width, padded
                    // down past LOGO_STRIP so it never collides with the top-right brand logo.
                    if (full) {
                        Box(
                            Modifier
                                .fillMaxHeight()
                                .weight(1f)
                                .padding(top = LOGO_STRIP),
                        ) {
                            content(current)
                        }
                    }
                }
                // Branding: logo overlay pinned top-right of the band (Full only — in Peek the
                // band is just the rail, so a logo would overflow it).
                if (full) {
                    Image(
                        painter = painterResource(R.drawable.trireme_logo),
                        contentDescription = stringResource(R.string.logo_desc),
                        modifier = Modifier
                            .align(Alignment.TopEnd)
                            .padding(12.dp)
                            .size(40.dp),
                    )
                }
            }
        }
    }
}

/**
 * The narrow vertical rail of the four destinations. [focusRequester] is attached to the item
 * matching [current] so the shell can land focus there when the navbar is revealed (see the
 * LaunchedEffect in [NavShell]).
 */
@Composable
private fun NavRail(
    current: NavDest,
    focusRequester: FocusRequester,
    onSelect: (NavDest) -> Unit,
    onExit: () -> Unit,
) {
    // The requester attaches to whichever item is currently selected — the natural landing spot.
    fun reqFor(dest: NavDest): Modifier =
        if (dest == current) Modifier.focusRequester(focusRequester) else Modifier
    Column(
        Modifier
            .fillMaxHeight()
            .width(96.dp)
            .padding(vertical = 16.dp, horizontal = 8.dp),
    ) {
        // The four destinations scroll on very short landscape heights; the Exit action
        // stays pinned at the bottom of the rail (reachable in Peek and Full alike).
        Column(
            Modifier.weight(1f).verticalScroll(rememberScrollState()),
            verticalArrangement = Arrangement.spacedBy(8.dp),
        ) {
        NavRailItem(
            dest = NavDest.Stream,
            icon = Icons.Filled.PlayArrow,
            label = stringResource(R.string.nav_stream),
            desc = stringResource(R.string.nav_stream_desc),
            selected = current == NavDest.Stream,
            focusModifier = reqFor(NavDest.Stream),
            onClick = { onSelect(NavDest.Stream) },
        )
        NavRailItem(
            dest = NavDest.Hosts,
            icon = Icons.AutoMirrored.Filled.List,
            label = stringResource(R.string.nav_hosts),
            desc = stringResource(R.string.nav_hosts_desc),
            selected = current == NavDest.Hosts,
            focusModifier = reqFor(NavDest.Hosts),
            onClick = { onSelect(NavDest.Hosts) },
        )
        NavRailItem(
            dest = NavDest.Apps,
            icon = Icons.Filled.Apps,
            label = stringResource(R.string.nav_apps),
            desc = stringResource(R.string.nav_apps_desc),
            selected = current == NavDest.Apps,
            focusModifier = reqFor(NavDest.Apps),
            onClick = { onSelect(NavDest.Apps) },
        )
        NavRailItem(
            dest = NavDest.Controllers,
            icon = Icons.Filled.SportsEsports,
            label = stringResource(R.string.nav_controllers),
            desc = stringResource(R.string.nav_controllers_desc),
            selected = current == NavDest.Controllers,
            focusModifier = reqFor(NavDest.Controllers),
            onClick = { onSelect(NavDest.Controllers) },
        )
        NavRailItem(
            dest = NavDest.Settings,
            icon = Icons.Filled.Settings,
            label = stringResource(R.string.nav_settings),
            desc = stringResource(R.string.nav_settings_desc),
            selected = current == NavDest.Settings,
            focusModifier = reqFor(NavDest.Settings),
            onClick = { onSelect(NavDest.Settings) },
        )
        }
        Spacer(Modifier.height(8.dp))
        ExitRailItem(onExit = onExit)
    }
}

/**
 * The Exit action pinned at the bottom of the rail — gracefully disconnects every
 * session and tears the activity down (the same teardown as the double-Back exit, but
 * discoverable). Error tint on focus to read as a leave/destructive action;
 * `role = Button` (not Tab) since it leaves rather than selecting a pane.
 */
@Composable
private fun ExitRailItem(onExit: () -> Unit) {
    var focused by remember { mutableStateOf(false) }
    val desc = stringResource(R.string.nav_exit_desc)
    val tint =
        if (focused) MaterialTheme.colorScheme.error else MaterialTheme.colorScheme.onSurfaceVariant
    Column(
        Modifier
            .fillMaxWidth()
            .clip(RoundedCornerShape(TriremeTokens.tileCorner))
            .background(MaterialTheme.colorScheme.surfaceContainer)
            .onFocusChanged { focused = it.isFocused }
            .clickable(onClick = onExit)
            .triremeFocus(focused)
            .semantics {
                role = Role.Button
                contentDescription = desc
            }
            .padding(vertical = 10.dp),
        horizontalAlignment = Alignment.CenterHorizontally,
        verticalArrangement = Arrangement.spacedBy(2.dp),
    ) {
        Icon(
            Icons.AutoMirrored.Filled.ExitToApp,
            contentDescription = null, // the Column's contentDescription names the action
            tint = tint,
            modifier = Modifier.size(28.dp),
        )
        Text(
            stringResource(R.string.nav_exit),
            style = MaterialTheme.typography.labelMedium,
            color = tint,
        )
    }
}

/**
 * A single rail destination — its own focusable click target (≥ 48.dp), gold focus ring via
 * [triremeFocus], `role = Tab` + `selected` for screen readers. The selected dest gets a faint
 * fill + primary tint so it reads at a glance from the couch.
 */
@Composable
private fun NavRailItem(
    dest: NavDest,
    icon: ImageVector,
    label: String,
    desc: String,
    selected: Boolean,
    focusModifier: Modifier,
    onClick: () -> Unit,
) {
    var focused by remember { mutableStateOf(false) }
    val tint =
        if (selected || focused) MaterialTheme.colorScheme.primary
        else MaterialTheme.colorScheme.onSurfaceVariant
    Column(
        Modifier
            .fillMaxWidth()
            .clip(RoundedCornerShape(TriremeTokens.tileCorner))
            .background(
                if (selected) MaterialTheme.colorScheme.surfaceContainerHighest
                else MaterialTheme.colorScheme.surfaceContainer,
            )
            // The focusRequester (for the selected item) must sit ahead of the focusable
            // node (.clickable) so it targets it; no-op Modifier for the others.
            .then(focusModifier)
            .onFocusChanged { focused = it.isFocused }
            .clickable(onClick = onClick)
            .triremeFocus(focused || selected)
            .semantics {
                role = Role.Tab
                this.selected = selected
                contentDescription = desc
            }
            .padding(vertical = 10.dp),
        horizontalAlignment = Alignment.CenterHorizontally,
        verticalArrangement = Arrangement.spacedBy(2.dp),
    ) {
        Icon(
            icon,
            contentDescription = null, // the Column's contentDescription names the dest
            tint = tint,
            modifier = Modifier.size(28.dp),
        )
        Text(
            label,
            style = MaterialTheme.typography.labelMedium,
            color = tint,
            fontWeight = if (selected) FontWeight.SemiBold else FontWeight.Normal,
        )
    }
}
