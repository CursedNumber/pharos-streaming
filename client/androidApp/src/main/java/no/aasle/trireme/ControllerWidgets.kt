// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The Pharos/Trireme authors

package no.aasle.trireme

import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.RowScope
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Slider
import androidx.compose.material3.Switch
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.focus.onFocusChanged
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp

/**
 * Tiny focusable widgets shared by the Controllers pane, the gyro calibration
 * overlay, and the controller profile UI. Lifted out of `ControllersActivity`
 * so the activity could go away without dragging these callers with it; the
 * package stays `no.aasle.trireme` so the existing call sites compile
 * unchanged.
 *
 * TV-focus rule (lesson-compose-tv-focus): every widget here is its OWN
 * focusable click target — never a wrapper around child focusables — so a
 * remote's D-pad reaches each control directly.
 */

/**
 * A full-width, focusable row that toggles on D-pad center — the whole row is the
 * focus target (so the TV remote reaches it and shows a clear highlight), and the
 * [Switch] is display-only (`onCheckedChange = null`) so it isn't a second,
 * competing focus stop.
 */
@Composable
internal fun ToggleRow(label: String, checked: Boolean, bold: Boolean = false, onChange: (Boolean) -> Unit) {
    FocusRow(onClick = { onChange(!checked) }) {
        Text(
            label,
            fontSize = if (bold) 15.sp else 13.sp,
            fontWeight = if (bold) FontWeight.SemiBold else FontWeight.Normal,
            modifier = Modifier.weight(1f),
        )
        Switch(checked = checked, onCheckedChange = null)
    }
}

/**
 * Focusable, clickable full-width row with a visible focus highlight (default
 * Material indication is too subtle on a 10-foot TV). `content` is laid out in a
 * [RowScope] so callers can use `Modifier.weight`.
 */
@Composable
internal fun FocusRow(onClick: () -> Unit, content: @Composable RowScope.() -> Unit) {
    var focused by remember { mutableStateOf(false) }
    Row(
        Modifier
            .fillMaxWidth()
            .clip(RoundedCornerShape(8.dp))
            .onFocusChanged { focused = it.isFocused }
            .clickable { onClick() }
            .background(
                if (focused) MaterialTheme.colorScheme.primary.copy(alpha = 0.22f) else Color.Transparent,
            )
            .padding(horizontal = 8.dp, vertical = 10.dp),
        verticalAlignment = Alignment.CenterVertically,
        content = content,
    )
}

/**
 * A focus-key'd slider that commits its value on every change and on finish.
 * Keyed on the incoming [value] so external updates (Reset, Load preset) move
 * the slider — but during a drag the parent doesn't change [value], so the
 * local position is preserved.
 */
@Composable
internal fun GyroSlider(label: String, value: Float, range: ClosedFloatingPointRange<Float>, onCommit: (Float) -> Unit) {
    var v by remember(label, value) { mutableStateOf(value) }
    Spacer(Modifier.height(2.dp))
    Text("$label  ${"%.1f".format(v)}", fontSize = 12.sp)
    Slider(
        value = v,
        // Commit on every change AND on finish: on a TV the D-pad drives the
        // slider in discrete steps and `onValueChangeFinished` doesn't fire
        // reliably, so committing per step is what makes it persist + relay.
        onValueChange = { v = it; onCommit(it) },
        onValueChangeFinished = { onCommit(v) },
        valueRange = range,
    )
}

/**
 * A focusable, clickable capability tile (Gyro / Rumble / Paddles). `active`
 * fills it in; inactive shows a dim outline. Focus adds a ring so the TV remote
 * shows where it is.
 */
@Composable
internal fun TileButton(text: String, color: Color, active: Boolean, onClick: () -> Unit) {
    var focused by remember { mutableStateOf(false) }
    Box(
        Modifier
            .clip(RoundedCornerShape(6.dp))
            .onFocusChanged { focused = it.isFocused }
            .clickable { onClick() }
            .background(color.copy(alpha = if (active) 0.32f else 0.10f))
            .background(if (focused) Color.White.copy(alpha = 0.18f) else Color.Transparent)
            .padding(horizontal = 10.dp, vertical = 4.dp),
    ) {
        Text(
            text,
            color = if (active) color else color.copy(alpha = 0.55f),
            fontSize = 11.sp,
            fontWeight = if (focused) FontWeight.SemiBold else FontWeight.Normal,
        )
    }
}
