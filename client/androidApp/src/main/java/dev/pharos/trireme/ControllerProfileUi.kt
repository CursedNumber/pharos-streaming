package dev.pharos.trireme

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.Button
import androidx.compose.material3.DropdownMenu
import androidx.compose.material3.DropdownMenuItem
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import dev.pharos.trireme.ui.TvEditField
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import androidx.compose.ui.window.Dialog
import androidx.compose.ui.window.DialogProperties
import uniffi.core_ffi.defaultControllerProfile

private val TILE = Color(110, 120, 135)

/**
 * Per-controller profile picker: a dropdown of `Default` + every saved preset,
 * plus a `Manage` button. Picking a profile copies it onto this controller (face
 * swaps + gyro feel) and records it as the controller's assigned profile so the
 * dropdown reflects what's applied; the host picks the new feel up on connect.
 */
@Composable
internal fun ProfileRow(
    prefs: GamepadPrefs,
    controllerKey: String,
    onApplied: () -> Unit,
    onManage: () -> Unit,
) {
    var expanded by remember { mutableStateOf(false) }
    // Read fresh each recomposition; the parent bumps its revision (onApplied /
    // onChanged) whenever a dialog mutates these, which re-runs this composable.
    val current = prefs.assignedProfile(controllerKey)
    val presets = prefs.savedProfileNames()

    Row(Modifier.fillMaxWidth(), verticalAlignment = Alignment.CenterVertically) {
        Text("Profile", fontSize = 13.sp, modifier = Modifier.weight(1f))
        Box {
            TileButton("$current  ▾", color = TILE, active = false) { expanded = true }
            DropdownMenu(expanded = expanded, onDismissRequest = { expanded = false }) {
                DropdownMenuItem(
                    text = { Text(GamepadPrefs.PROFILE_DEFAULT) },
                    onClick = {
                        prefs.setProfile(controllerKey, defaultControllerProfile())
                        prefs.setAssignedProfile(controllerKey, GamepadPrefs.PROFILE_DEFAULT)
                        expanded = false
                        onApplied()
                    },
                )
                for (name in presets) {
                    DropdownMenuItem(
                        text = { Text(name) },
                        onClick = {
                            prefs.loadNamedProfile(name)?.let { prefs.setProfile(controllerKey, it) }
                            prefs.setAssignedProfile(controllerKey, name)
                            expanded = false
                            onApplied()
                        },
                    )
                }
            }
        }
        Spacer(Modifier.width(8.dp))
        TileButton("Manage", color = TILE, active = false) { onManage() }
    }
}

/**
 * Save / rename / delete named presets. "Save" captures this controller's current
 * tuning under a name; presets are global, so any controller's dropdown can then
 * apply them.
 */
@Composable
internal fun ManageProfilesDialog(
    prefs: GamepadPrefs,
    controllerKey: String,
    onChanged: () -> Unit,
    onDismiss: () -> Unit,
) {
    var presets by remember { mutableStateOf(prefs.savedProfileNames()) }
    var newName by remember { mutableStateOf("") }
    var renaming by remember { mutableStateOf<String?>(null) }
    var renameText by remember { mutableStateOf("") }

    fun refresh() {
        presets = prefs.savedProfileNames()
        onChanged()
    }

    Dialog(onDismissRequest = onDismiss, properties = DialogProperties(usePlatformDefaultWidth = false)) {
        Surface(
            shape = RoundedCornerShape(14.dp),
            color = MaterialTheme.colorScheme.surface,
            modifier = Modifier.fillMaxWidth(0.92f).padding(8.dp),
        ) {
            LazyColumn(Modifier.padding(16.dp), verticalArrangement = Arrangement.spacedBy(8.dp)) {
                item {
                    Row(verticalAlignment = Alignment.CenterVertically) {
                        Text("Manage profiles", fontWeight = FontWeight.SemiBold, fontSize = 16.sp, modifier = Modifier.weight(1f))
                        OutlinedButton(onClick = onDismiss) { Text("Close", fontSize = 12.sp) }
                    }
                }
                item {
                    Text(
                        "Save this controller's current tuning as a reusable preset.",
                        fontSize = 11.sp,
                        color = MaterialTheme.colorScheme.onSurfaceVariant,
                    )
                }
                item {
                    Row(verticalAlignment = Alignment.CenterVertically) {
                        TvEditField(
                            value = newName,
                            onValueChange = { newName = it },
                            label = "New preset name",
                            modifier = Modifier.weight(1f),
                        )
                        Spacer(Modifier.width(8.dp))
                        Button(
                            enabled = isValidPresetName(newName),
                            onClick = {
                                prefs.saveNamedProfile(newName.trim(), prefs.profile(controllerKey))
                                prefs.setAssignedProfile(controllerKey, newName.trim())
                                newName = ""
                                refresh()
                            },
                        ) { Text("Save", fontSize = 12.sp) }
                    }
                }
                item {
                    Spacer(Modifier.height(4.dp))
                    Text("Saved presets", fontSize = 12.sp, fontWeight = FontWeight.SemiBold)
                }
                if (presets.isEmpty()) {
                    item {
                        Text("None yet.", fontSize = 11.sp, color = MaterialTheme.colorScheme.onSurfaceVariant)
                    }
                }
                items(presets, key = { it }) { name ->
                    if (renaming == name) {
                        Row(verticalAlignment = Alignment.CenterVertically) {
                            TvEditField(
                                value = renameText,
                                onValueChange = { renameText = it },
                                label = "Rename",
                                modifier = Modifier.weight(1f),
                            )
                            Spacer(Modifier.width(8.dp))
                            Button(
                                enabled = isValidPresetName(renameText),
                                onClick = {
                                    prefs.renameNamedProfile(name, renameText.trim())
                                    renaming = null
                                    refresh()
                                },
                            ) { Text("Save", fontSize = 12.sp) }
                            TextButton(onClick = { renaming = null }) { Text("Cancel", fontSize = 12.sp) }
                        }
                    } else {
                        Row(verticalAlignment = Alignment.CenterVertically) {
                            Text(name, fontSize = 13.sp, modifier = Modifier.weight(1f))
                            TextButton(onClick = { renaming = name; renameText = name }) {
                                Text("Rename", fontSize = 12.sp)
                            }
                            TextButton(onClick = { prefs.deleteNamedProfile(name); refresh() }) {
                                Text("Delete", fontSize = 12.sp, color = Color(200, 110, 110))
                            }
                        }
                    }
                }
            }
        }
    }
}

/** Reserved sentinels can't be a preset name; neither can blank. */
private fun isValidPresetName(name: String): Boolean {
    val n = name.trim()
    return n.isNotEmpty() && n != GamepadPrefs.PROFILE_DEFAULT && n != GamepadPrefs.PROFILE_CUSTOM
}

/**
 * Details for one (possibly composite) controller: rename it, and inspect/adjust
 * the individual interfaces it was folded from. Each non-primary or unrecognised
 * interface can be detached from the auto-grouping if it was wrongly absorbed.
 */
@Composable
internal fun ControllerDetailsDialog(
    group: ControllerGroupView,
    prefs: GamepadPrefs,
    onChanged: () -> Unit,
    onDismiss: () -> Unit,
) {
    var name by remember { mutableStateOf(prefs.customName(group.key) ?: "") }

    Dialog(onDismissRequest = onDismiss, properties = DialogProperties(usePlatformDefaultWidth = false)) {
        Surface(
            shape = RoundedCornerShape(14.dp),
            color = MaterialTheme.colorScheme.surface,
            modifier = Modifier.fillMaxWidth(0.92f).padding(8.dp),
        ) {
            LazyColumn(Modifier.padding(16.dp), verticalArrangement = Arrangement.spacedBy(8.dp)) {
                item {
                    Row(verticalAlignment = Alignment.CenterVertically) {
                        Text("Details — ${group.title}", fontWeight = FontWeight.SemiBold, fontSize = 16.sp, modifier = Modifier.weight(1f))
                        OutlinedButton(onClick = onDismiss) { Text("Close", fontSize = 12.sp) }
                    }
                }
                item {
                    Row(verticalAlignment = Alignment.CenterVertically) {
                        TvEditField(
                            value = name,
                            onValueChange = { name = it },
                            label = "Custom name",
                            modifier = Modifier.weight(1f),
                        )
                        Spacer(Modifier.width(8.dp))
                        Button(onClick = { prefs.setCustomName(group.key, name); onChanged() }) {
                            Text("Save", fontSize = 12.sp)
                        }
                    }
                }
                item {
                    Text(
                        if (group.members.size > 1) {
                            "This controller reports ${group.members.size} interfaces, folded into one card."
                        } else {
                            "Interface"
                        },
                        fontSize = 12.sp,
                        fontWeight = FontWeight.SemiBold,
                    )
                }
                items(group.members, key = { it.id }) { m ->
                    Column {
                        Text(m.name, fontSize = 13.sp)
                        Text(
                            "${m.address} · ${m.role}",
                            fontSize = 11.sp,
                            color = MaterialTheme.colorScheme.onSurfaceVariant,
                        )
                        // Only bridge/extra interfaces are foldable; the primary
                        // (recognised) interface always anchors its own card.
                        if (!m.recognized) {
                            ToggleRow("Auto-group this interface", !prefs.isSplit(m.address)) { grouped ->
                                prefs.setSplit(m.address, !grouped)
                                onChanged()
                            }
                        }
                    }
                }
                item {
                    Text(
                        "Turn off auto-grouping if an interface was wrongly folded in (it then " +
                            "gets its own card).",
                        fontSize = 11.sp,
                        color = MaterialTheme.colorScheme.onSurfaceVariant,
                    )
                }
            }
        }
    }
}

/** A flattened, UI-only view of a controller group so the dialog file stays
 *  decoupled from the activity's private data classes. */
internal data class ControllerGroupView(
    val key: String,
    val title: String,
    val members: List<MemberView>,
)

internal data class MemberView(
    /** Stable list key (the Android input-device id). */
    val id: Int,
    val name: String,
    /** "vvvv:pppp" — also the split-override key. */
    val address: String,
    val role: String,
    val recognized: Boolean,
)
