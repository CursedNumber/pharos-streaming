// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The Pharos/Trireme authors

package no.aasle.trireme.ui

import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.focus.FocusRequester
import androidx.compose.ui.focus.focusProperties
import androidx.compose.ui.focus.focusRequester
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.unit.dp
import no.aasle.trireme.R
import no.aasle.trireme.ServerEntry
import uniffi.core_ffi.PairException
import uniffi.core_ffi.PairResult

/**
 * Host add/edit/pair dialogs, lifted out of the old `ServerSelectorActivity` so the new
 * [no.aasle.trireme.ui.panes.HostsPane] can reuse them (the integration agent deletes the
 * old activity). Behaviour is unchanged from the original; only the hardcoded English literals
 * moved into string resources and the success/badge colours now flow from the theme. The shared
 * verbs (Cancel/Save/Connect/Back) come from the asset agent's `strings.xml`.
 */

@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun AddOrEditDialog(
    title: String,
    initialHost: String,
    initialPort: String,
    initialAlias: String,
    confirmLabel: String,
    onDismiss: () -> Unit,
    initialExternalHost: String = "",
    initialExternalPort: String = "",
    onConfirm: (
        host: String,
        port: Int,
        alias: String,
        externalHost: String,
        externalPort: Int?,
    ) -> Unit,
) {
    var host by rememberSaveable { mutableStateOf(initialHost) }
    var port by rememberSaveable { mutableStateOf(initialPort) }
    var alias by rememberSaveable { mutableStateOf(initialAlias) }
    var externalHost by rememberSaveable { mutableStateOf(initialExternalHost) }
    var externalPort by rememberSaveable { mutableStateOf(initialExternalPort) }
    val portValid = port.toIntOrNull()?.let { it in 1..65535 } == true
    // External port is optional; if present it must be a valid port.
    val extPortValid = externalPort.isEmpty() ||
        externalPort.toIntOrNull()?.let { it in 1..65535 } == true
    val ready = host.trim().isNotEmpty() && portValid && extPortValid
    // RIGHT off any field jumps to Save; the two buttons cross-link L/R (see TvEditField).
    val cancelFocus = remember { FocusRequester() }
    val saveFocus = remember { FocusRequester() }

    AlertDialog(
        onDismissRequest = onDismiss,
        title = { Text(title) },
        text = {
            Column {
                TvEditField(
                    value = host,
                    onValueChange = { host = it },
                    label = stringResource(R.string.hosts_field_host),
                    modifier = Modifier.fillMaxWidth(),
                    rightFocus = saveFocus,
                )
                Spacer(Modifier.height(8.dp))
                TvEditField(
                    value = port,
                    onValueChange = { port = it.filter { c -> c.isDigit() } },
                    label = stringResource(R.string.hosts_field_port),
                    keyboardType = KeyboardType.Number,
                    modifier = Modifier.fillMaxWidth(),
                    rightFocus = saveFocus,
                )
                Spacer(Modifier.height(8.dp))
                TvEditField(
                    value = alias,
                    onValueChange = { alias = it },
                    label = stringResource(R.string.hosts_field_alias),
                    modifier = Modifier.fillMaxWidth(),
                    rightFocus = saveFocus,
                )
                Spacer(Modifier.height(8.dp))
                // Optional WAN/external address — a DDNS domain (IDN like `bücher.example`
                // is fine), tried as a fallback after the LAN host. Pairing on
                // LAN/VPN first; this is for streaming over the internet afterward.
                TvEditField(
                    value = externalHost,
                    onValueChange = { externalHost = it },
                    label = stringResource(R.string.hosts_field_external_host),
                    modifier = Modifier.fillMaxWidth(),
                    rightFocus = saveFocus,
                )
                Spacer(Modifier.height(8.dp))
                TvEditField(
                    value = externalPort,
                    onValueChange = { externalPort = it.filter { c -> c.isDigit() } },
                    label = stringResource(R.string.hosts_field_external_port),
                    keyboardType = KeyboardType.Number,
                    modifier = Modifier.fillMaxWidth(),
                    rightFocus = saveFocus,
                )
            }
        },
        confirmButton = {
            TextButton(
                enabled = ready,
                onClick = {
                    onConfirm(
                        host.trim(),
                        port.toInt(),
                        alias.trim(),
                        externalHost.trim(),
                        externalPort.toIntOrNull(),
                    )
                },
                modifier = Modifier
                    .focusRequester(saveFocus)
                    .focusProperties { left = cancelFocus },
            ) { Text(confirmLabel) }
        },
        dismissButton = {
            TextButton(
                onClick = onDismiss,
                modifier = Modifier
                    .focusRequester(cancelFocus)
                    .focusProperties { right = saveFocus },
            ) { Text(stringResource(R.string.action_cancel)) }
        },
    )
}

@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun PairDialog(
    target: ServerEntry,
    onDismiss: () -> Unit,
    onPair: (pin: String, cb: (Result<PairResult>) -> Unit) -> Unit,
) {
    var pin by rememberSaveable { mutableStateOf("") }
    var inFlight by remember { mutableStateOf(false) }
    var result by remember { mutableStateOf<Result<PairResult>?>(null) }
    val pinValid = pin.length == 6 && pin.all { it.isDigit() }
    val startFocus = remember { FocusRequester() } // RIGHT off the PIN field -> Start

    AlertDialog(
        onDismissRequest = { if (!inFlight) onDismiss() },
        title = { Text(stringResource(R.string.hosts_pair_title, target.friendlyName())) },
        text = {
            Column {
                Text(
                    stringResource(R.string.hosts_pair_hint),
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
                Spacer(Modifier.height(8.dp))
                TvEditField(
                    value = pin,
                    onValueChange = { pin = it.filter { c -> c.isDigit() }.take(6) },
                    label = stringResource(R.string.hosts_pair_pin_label),
                    enabled = !inFlight,
                    keyboardType = KeyboardType.NumberPassword,
                    modifier = Modifier.fillMaxWidth(),
                    rightFocus = startFocus,
                )
                Spacer(Modifier.height(12.dp))
                when {
                    inFlight -> Row(verticalAlignment = Alignment.CenterVertically) {
                        CircularProgressIndicator(modifier = Modifier.size(18.dp))
                        Spacer(Modifier.width(8.dp))
                        Text(stringResource(R.string.hosts_pair_running))
                    }
                    result?.isSuccess == true -> Text(
                        stringResource(
                            R.string.hosts_pair_ok,
                            result!!.getOrNull()!!.hostPubkeyHex.take(8),
                        ),
                        // Use the theme tertiary (sea-foam) for the success affordance rather
                        // than a hardcoded green, so it recolors with the mode.
                        color = MaterialTheme.colorScheme.tertiary,
                    )
                    result?.isFailure == true -> Text(
                        stringResource(
                            R.string.hosts_pair_failed,
                            pairErrorMessage(result!!.exceptionOrNull()),
                        ),
                        color = MaterialTheme.colorScheme.error,
                    )
                }
            }
        },
        confirmButton = {
            if (result?.isSuccess == true) {
                TextButton(onClick = onDismiss) { Text(stringResource(R.string.hosts_pair_close)) }
            } else {
                TextButton(
                    enabled = pinValid && !inFlight,
                    onClick = {
                        inFlight = true
                        result = null
                        onPair(pin) { r ->
                            inFlight = false
                            result = r
                        }
                    },
                    modifier = Modifier.focusRequester(startFocus),
                ) { Text(stringResource(R.string.hosts_pair_start)) }
            }
        },
        dismissButton = {
            if (result?.isSuccess != true) {
                TextButton(enabled = !inFlight, onClick = onDismiss) {
                    Text(stringResource(R.string.action_cancel))
                }
            }
        },
    )
}

/**
 * Map a [PairException] to a human message. A @Composable so it reads string resources;
 * mirrors the original `pairErrorMessage` in ServerSelectorActivity one-for-one.
 */
@Composable
fun pairErrorMessage(t: Throwable?): String = when (t) {
    is PairException.PinMismatch -> stringResource(R.string.pair_err_pin_mismatch)
    is PairException.HostAborted -> stringResource(R.string.pair_err_host_aborted, t.reason)
    is PairException.Network -> stringResource(R.string.pair_err_network, t.reason)
    is PairException.Protocol -> stringResource(R.string.pair_err_protocol, t.reason)
    is PairException.BadAddress -> stringResource(R.string.pair_err_bad_address, t.reason)
    is PairException.Identity -> stringResource(R.string.pair_err_identity, t.reason)
    null -> stringResource(R.string.pair_err_unknown)
    else -> t.message ?: t.toString()
}
