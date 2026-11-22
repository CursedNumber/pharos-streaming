// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The Pharos authors

package io.github.cursednumber.trireme.ui

import androidx.compose.animation.core.animateFloatAsState
import androidx.compose.foundation.border
import androidx.compose.foundation.layout.fillMaxHeight
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.darkColorScheme
import androidx.compose.material3.lightColorScheme
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.composed
import androidx.compose.ui.draw.drawBehind
import androidx.compose.ui.draw.drawWithContent
import androidx.compose.ui.geometry.CornerRadius
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.geometry.Size
import androidx.compose.ui.graphics.Brush
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.drawscope.Stroke
import androidx.compose.ui.graphics.graphicsLayer
import androidx.compose.ui.unit.Dp
import androidx.compose.ui.unit.dp
import androidx.compose.foundation.isSystemInDarkTheme

/**
 * Trireme nautical identity — chosen direction: **"Aegean deep-sea dark + antique gold"**.
 *
 * Three directions were weighed against the constraint that the navbar floats over a LIVE
 * VIDEO frame (arbitrary content, can be bright):
 *   1. *Translucent glass over live video* — beautiful but fails the legibility bar: text on
 *      a 20% scrim is unreadable over a bright frame, exactly the 10-foot failure mode we must
 *      avoid. Rejected as the base.
 *   2. *Antique nautical chart / parchment* — warm and on-brand, but a light, low-contrast
 *      surface over video is the worst case for legibility and clashes with a dark TV room.
 *      Kept as the *light-mode* expression only.
 *   3. *Aegean deep-sea dark* — near-black-sea surfaces, parchment text, antique-gold accents.
 *      Opaque-enough panels read over any frame; the gold reads as "naval brass" branding.
 *      **Committed** as the primary (dark) identity; parchment-chart is the light variant.
 *
 * Both modes are explicit branded schemes (NOT Material You / dynamic color) so the nautical
 * identity is constant across devices. The whole UI follows the system setting via
 * [isSystemInDarkTheme]; the video itself is mode-independent, but every panel/overlay recolors.
 *
 * WCAG AA body-text contrast (computed, ≥ 4.5:1) for the load-bearing pairs:
 *   DARK  parchment onSurface  F0EAD2 / surface 001019 .......... 15.99:1
 *   DARK  seafoam onSurfaceVar 9DB8B8 / surface 001830 ...........  8.51:1
 *   DARK  gold primary         E8C77A / surface 001019 .......... 11.84:1
 *   DARK  navy onPrimary       001019 / gold D8B86A ............. 10.09:1
 *   DARK  crimson secondary    D06A4E / surface 001019 ...........  5.38:1
 *   LIGHT navy onSurface       102030 / surface FFFFFF .......... 16.52:1
 *   LIGHT navy onSurface       102030 / surface F4F1E4 .......... 14.59:1
 *   LIGHT bronze primary       7E6020 / surface F4F1E4 ...........  5.18:1  (gold deepened for AA)
 *   LIGHT parchment onPrimary  F8F4E2 / bronze 7E6020 ...........  5.31:1
 *   LIGHT crimson secondary    7A2A18 / surface FFFFFF ...........  9.67:1
 * Light-mode gold is intentionally deepened to bronze (#7E6020): the dark-mode #D8B86A is
 * illegible on parchment (~1.5:1), so the light scheme swaps in the deeper tone for AA.
 */

/**
 * Single source of truth for raw nautical colors + key dimensions. UI modules reference these
 * (never re-declare hex) so the brand stays consistent. Colors are mode-agnostic constants;
 * pick the mode-aware accessors ([scrimColor]) where the value differs by light/dark.
 */
object TriremeTokens {
    // --- Sea (dark surfaces, sampled from the logo background) ---
    val SeaBlack = Color(0xFF001019)   // deepest hull shadow — darkest surface
    val SeaNavy = Color(0xFF001830)    // primary sea-navy
    val SeaNavyHi = Color(0xFF002A40)  // raised sea-navy (elevated containers)
    val SeaNavyHi2 = Color(0xFF003048) // highest sea-navy
    val SeaBackdrop = Color(0xFF000C14) // window backdrop, below SeaBlack

    // --- Sail / parchment (text + light surfaces) ---
    val Parchment = Color(0xFFF0EAD2)  // sail/parchment — dark-mode body text
    val ParchmentHi = Color(0xFFF0F0D8) // brighter sail highlight
    val ParchmentLight = Color(0xFFF4F1E4) // light-mode primary surface
    val ParchmentSurf = Color(0xFFF8F4E2) // light-mode onPrimary text on bronze

    // --- Antique gold / bronze (primary accent + focus) ---
    val Gold = Color(0xFFD8B86A)        // antique gold — dark-mode primary
    val GoldHi = Color(0xFFE8C77A)      // brighter gold — gold-on-dark text + focus border
    val GoldMuted = Color(0xFFD8C090)   // softened gold
    val Bronze = Color(0xFF7E6020)      // deepened gold — light-mode primary (AA on parchment)

    // --- Crimson / oxblood (secondary / alert) ---
    val Crimson = Color(0xFF9A3A22)     // crimson hull stripe
    val CrimsonLite = Color(0xFFD06A4E) // lightened crimson — dark-mode secondary text (AA)
    val Oxblood = Color(0xFF601800)     // deepest oxblood
    val CrimsonDeep = Color(0xFF7A2A18) // light-mode secondary

    // --- Muted sea-foam (tertiary / muted labels) ---
    val SeaFoam = Color(0xFF7E9A9A)     // muted sea-foam
    val SeaFoamHi = Color(0xFF9DB8B8)   // brighter foam — dark-mode onSurfaceVariant (AA)
    val SeaFoamDeep = Color(0xFF35504F) // light-mode onSurfaceVariant (AA on parchment)

    // --- Dimensions / layout (one source so panes agree) ---
    val navWidth: Dp = 280.dp           // left rail + pane width when the navbar is open
    val tileCorner: Dp = 12.dp          // box-art / tile / row corner radius
    val minTarget: Dp = 48.dp           // a11y minimum touch/focus target

    // --- Focus treatment (gold "naval brass" ring) ---
    val focusBorderWidth: Dp = 2.5.dp
    val focusColor: Color = GoldHi      // reads in both modes; theme-tied via this token

    /**
     * Mode-aware scrim painted behind the navbar so the stream stays *faintly* visible while
     * keeping panel text legible over ANY frame. Dark mode uses a near-opaque deep-sea wash;
     * light mode uses a slightly heavier wash still (a thin scrim over a bright frame would let
     * the video bleed through light text). Don't assume the video is dark.
     */
    fun scrimColor(dark: Boolean): Color =
        if (dark) Color(0xF2000C14)   // ~95% deep-sea
        else Color(0xF0F4F1E4)        // ~94% parchment
}

private val TriremeDarkColors = darkColorScheme(
    primary = TriremeTokens.Gold,
    onPrimary = TriremeTokens.SeaBlack,
    primaryContainer = TriremeTokens.SeaNavyHi2,
    onPrimaryContainer = TriremeTokens.GoldHi,
    secondary = TriremeTokens.CrimsonLite,
    onSecondary = TriremeTokens.SeaBlack,
    secondaryContainer = TriremeTokens.Oxblood,
    onSecondaryContainer = TriremeTokens.Parchment,
    tertiary = TriremeTokens.SeaFoamHi,
    onTertiary = TriremeTokens.SeaBlack,
    background = TriremeTokens.SeaBackdrop,
    onBackground = TriremeTokens.Parchment,
    surface = TriremeTokens.SeaBlack,
    onSurface = TriremeTokens.Parchment,
    surfaceVariant = TriremeTokens.SeaNavy,
    onSurfaceVariant = TriremeTokens.SeaFoamHi,
    surfaceContainer = TriremeTokens.SeaNavy,
    surfaceContainerHigh = TriremeTokens.SeaNavyHi,
    surfaceContainerHighest = TriremeTokens.SeaNavyHi2,
    outline = TriremeTokens.SeaFoam,
    outlineVariant = TriremeTokens.SeaNavyHi,
    error = TriremeTokens.CrimsonLite,
    onError = TriremeTokens.SeaBlack,
)

private val TriremeLightColors = lightColorScheme(
    primary = TriremeTokens.Bronze,
    onPrimary = TriremeTokens.ParchmentSurf,
    primaryContainer = TriremeTokens.GoldMuted,
    onPrimaryContainer = Color(0xFF2A2008),
    secondary = TriremeTokens.CrimsonDeep,
    onSecondary = TriremeTokens.ParchmentSurf,
    secondaryContainer = Color(0xFFF2D6CC),
    onSecondaryContainer = TriremeTokens.Oxblood,
    tertiary = TriremeTokens.SeaFoamDeep,
    onTertiary = TriremeTokens.ParchmentSurf,
    background = TriremeTokens.ParchmentLight,
    onBackground = Color(0xFF102030),
    surface = Color(0xFFFFFFFF),
    onSurface = Color(0xFF102030),
    surfaceVariant = TriremeTokens.ParchmentLight,
    onSurfaceVariant = TriremeTokens.SeaFoamDeep,
    surfaceContainer = TriremeTokens.ParchmentLight,
    surfaceContainerHigh = Color(0xFFEDE8D6),
    surfaceContainerHighest = Color(0xFFE5DFC9),
    outline = TriremeTokens.SeaFoamDeep,
    outlineVariant = Color(0xFFCFC9B2),
    error = TriremeTokens.CrimsonDeep,
    onError = TriremeTokens.ParchmentSurf,
)

/**
 * Branded nautical Material3 theme. Follows the system dark/light setting (CRUCIAL) and applies
 * the deep-sea (dark) or parchment-chart (light) scheme — never dynamic color, so every device
 * shows the same Trireme identity. Typography/shapes are left to Material defaults (sp-sized
 * text honours user scaling); only the color identity is overridden.
 */
@Composable
fun TriremeTheme(content: @Composable () -> Unit) {
    val dark = isSystemInDarkTheme()
    MaterialTheme(
        colorScheme = if (dark) TriremeDarkColors else TriremeLightColors,
        content = content,
    )
}

/**
 * Standard TV focus treatment — the single source of focus styling so every focusable row/tile
 * looks identical. A gold "naval brass" border plus a subtle scale lift when [focused]. Driven
 * off [TriremeTokens.focusColor] (a bright gold that reads in both modes). Apply per ROW/tile
 * (never on a container wrapping inner focusables — that traps the D-pad; see TV-focus lesson).
 */
// Implemented via `composed` so the scale can animate with the focus transition.
fun Modifier.triremeFocus(focused: Boolean): Modifier = composed {
    if (!focused) return@composed this
    val color = TriremeTokens.focusColor
    val strokeDp = TriremeTokens.focusBorderWidth
    val cornerDp = TriremeTokens.tileCorner
    // Draw the gold ring INSET — fully inside the layout bounds — instead of an
    // edge-centred border + outward scale. The old approach put half the stroke (and
    // the 1.04x grow) OUTSIDE the bounds, where the card's own `.clip(shape)` and the
    // scrolling container both cut it off → the reported left/right clipping. An inset
    // stroke can't be clipped by either, in any pane, at any size.
    drawWithContent {
        drawContent()
        val sw = strokeDp.toPx()
        val inset = sw / 2f
        drawRoundRect(
            color = color,
            topLeft = Offset(inset, inset),
            size = Size(size.width - sw, size.height - sw),
            cornerRadius = CornerRadius((cornerDp.toPx() - inset).coerceAtLeast(0f)),
            style = Stroke(width = sw),
        )
    }
}

/**
 * Mode-tuned translucent scrim for the navbar surface over live video. A flat near-opaque wash
 * plus a faint vertical gradient (heavier at the rail's leading edge, lighter toward the pane)
 * so the stream stays *hinted* without ever compromising text legibility over a bright frame.
 * Full-height wash only — the CALLER sets the band width (it is responsive to the
 * screen: see `navBandWidth` in NavShell), so this no longer hardcodes a width.
 */
fun Modifier.navScrim(): Modifier = composed {
    val dark = isSystemInDarkTheme()
    val base = TriremeTokens.scrimColor(dark)
    // A touch of extra opacity at the leading edge, easing toward the pane edge.
    val edge = if (dark) Color(0xFF000C14) else Color(0xFFFFFFFF)
    this
        .fillMaxHeight()
        .drawBehind {
            drawRect(base)
            drawRect(
                brush = Brush.horizontalGradient(
                    0f to edge.copy(alpha = if (dark) 0.25f else 0.18f),
                    1f to Color.Transparent,
                ),
            )
        }
}
