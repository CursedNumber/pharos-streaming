// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The Pharos/Trireme authors

package no.aasle.trireme.ui

import android.content.pm.PackageManager
import androidx.compose.foundation.gestures.detectTapGestures
import androidx.compose.foundation.text.KeyboardActions
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.OutlinedTextFieldDefaults
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.ExperimentalComposeUiApi
import androidx.compose.ui.Modifier
import androidx.compose.ui.focus.FocusRequester
import androidx.compose.ui.focus.onFocusChanged
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.input.key.Key
import androidx.compose.ui.input.key.KeyEventType
import androidx.compose.ui.input.key.key
import androidx.compose.ui.input.key.onPreviewKeyEvent
import androidx.compose.ui.input.key.type
import androidx.compose.ui.input.pointer.pointerInput
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.LocalSoftwareKeyboardController
import androidx.compose.ui.text.input.ImeAction
import androidx.compose.ui.text.input.KeyboardType

/**
 * Text field for Trireme — picks the right input model for the device.
 *
 * The hard split is **touch vs. D-pad-only**, keyed on [PackageManager.FEATURE_TOUCHSCREEN]
 * (the same discriminator the navbar edge-handle uses). They need genuinely different
 * behaviour, and conflating them was the bug: a phone (touch) was getting the TV's
 * D-pad-gated field, where tapping never focused and any key press collapsed edit mode
 * and dismissed the keyboard.
 *
 *  - **Touch device (phone/tablet):** [TouchEditField] — a normal editable field. Tap (or
 *    controller-focus) raises the IME; the IME owns D-pad navigation of its own keys; nothing
 *    is intercepted. This is what fixes "tapping the field does nothing / the keyboard closes
 *    on any press / the controller can't drive the soft keyboard" on the 14T Pro.
 *  - **D-pad-only device (TV):** [DpadEditField] — the validated TV-first model where focus only
 *    *selects* a field (gold ring) and editing begins on an explicit OK/center press, so stepping
 *    through a dialog with a remote never pops/dismisses the keyboard on every field.
 *
 * In both, the focused field shows the **single** gold [triremeFocus] ring: the native
 * OutlinedTextField border is made transparent on focus so it isn't doubled by the ring.
 */
@Composable
fun TvEditField(
    value: String,
    onValueChange: (String) -> Unit,
    label: String,
    modifier: Modifier = Modifier,
    enabled: Boolean = true,
    keyboardType: KeyboardType = KeyboardType.Text,
    imeAction: ImeAction = ImeAction.Done,
    rightFocus: FocusRequester? = null,
) {
    val context = LocalContext.current
    val hasTouch = remember {
        context.packageManager.hasSystemFeature(PackageManager.FEATURE_TOUCHSCREEN)
    }
    if (hasTouch) {
        TouchEditField(value, onValueChange, label, modifier, enabled, keyboardType, imeAction)
    } else {
        DpadEditField(value, onValueChange, label, modifier, enabled, keyboardType, imeAction, rightFocus)
    }
}

/**
 * Touch model: a plain editable [OutlinedTextField]. Focus (tap or controller) raises the IME
 * and the IME owns D-pad navigation of its keys — exactly the platform default. We deliberately
 * do NOT gate the IME behind `readOnly`, intercept keys, or consume taps (all of which broke the
 * TV-first field on the phone). The only styling is the single gold focus ring.
 */
@OptIn(ExperimentalMaterial3Api::class)
@Composable
private fun TouchEditField(
    value: String,
    onValueChange: (String) -> Unit,
    label: String,
    modifier: Modifier,
    enabled: Boolean,
    keyboardType: KeyboardType,
    imeAction: ImeAction,
) {
    var focused by remember { mutableStateOf(false) }
    val kb = LocalSoftwareKeyboardController.current
    OutlinedTextField(
        value = value,
        onValueChange = onValueChange,
        label = { Text(label) },
        enabled = enabled,
        singleLine = true,
        keyboardOptions = KeyboardOptions(keyboardType = keyboardType, imeAction = imeAction),
        // Any IME action button just dismisses the keyboard; focus stays on the field.
        keyboardActions = KeyboardActions(
            onDone = { kb?.hide() },
            onGo = { kb?.hide() },
            onSearch = { kb?.hide() },
            onSend = { kb?.hide() },
            onNext = { kb?.hide() },
        ),
        // Transparent border WHEN FOCUSED so the gold triremeFocus ring is the sole focus
        // indicator (no doubled border); a visible outline when unfocused so the field reads
        // as an input box.
        colors = OutlinedTextFieldDefaults.colors(
            focusedBorderColor = Color.Transparent,
            unfocusedBorderColor = MaterialTheme.colorScheme.outline,
        ),
        modifier = modifier
            .onFocusChanged { focused = it.isFocused }
            .triremeFocus(focused),
    )
}

/**
 * TV-first text field — the D-pad-only a11y standard for remote-driven text entry.
 *
 * **The rule:** on a D-pad remote, focus must only *select* a field (show the focus ring),
 * never pop the soft keyboard. Editing begins only on an explicit **OK/center** press. A plain
 * [OutlinedTextField] raises the IME the instant it gains focus, so stepping through a dialog with
 * the remote pops and dismisses the keyboard on every field — slow and error-prone.
 *
 * **How:** a single [OutlinedTextField] gated by `readOnly = !editing`. Verified against this
 * project's `foundation` 1.6.8: `CoreTextField` only starts the input session when
 * `hasFocus && enabled && !readOnly`, so a focused **readOnly** field shows no IME; flipping
 * `editing=true` (OK) makes Compose start the session itself. Re-verify the gate if the Compose
 * BOM is bumped past 1.7 (BasicTextField2 routes the IME differently).
 *
 * **Leaving edit (the TV-correct signal):** on a TV the soft keyboard owns the D-pad while it's
 * up — the field receives D-pad keys ONLY once the keyboard is down. So we don't try to catch
 * Back (the IME swallows it) or read IME insets (the docked TV keyboard doesn't report them):
 * any key arriving at the field means "we're navigating now", so we normalize `editing=false`.
 * (This assumption holds on a D-pad-only TV; it does NOT on a touch phone, which is why touch
 * devices take the [TouchEditField] path instead.)
 *
 * **Navigation:** while editing, the IME owns the arrows — untouched. Once the keyboard is down,
 * [rightFocus] (when set) makes **RIGHT** jump straight to the Cancel/Save button.
 */
@OptIn(ExperimentalMaterial3Api::class, ExperimentalComposeUiApi::class)
@Composable
private fun DpadEditField(
    value: String,
    onValueChange: (String) -> Unit,
    label: String,
    modifier: Modifier,
    enabled: Boolean,
    keyboardType: KeyboardType,
    imeAction: ImeAction,
    rightFocus: FocusRequester?,
) {
    var focused by remember { mutableStateOf(false) }
    // Saveable so a mid-edit field doesn't snap shut on rotation; defaults false so focus
    // alone never starts in edit mode.
    var editing by rememberSaveable { mutableStateOf(false) }
    val kb = LocalSoftwareKeyboardController.current

    OutlinedTextField(
        value = value,
        onValueChange = onValueChange,
        label = { Text(label) },
        enabled = enabled,
        readOnly = !editing, // THE GATE: focused + readOnly = no IME (verified, foundation 1.6.8)
        singleLine = true,
        keyboardOptions = KeyboardOptions(keyboardType = keyboardType, imeAction = imeAction),
        // Any IME action button exits edit + hides the keyboard (focus stays on the field).
        keyboardActions = KeyboardActions(
            onDone = { editing = false; kb?.hide() },
            onGo = { editing = false; kb?.hide() },
            onSearch = { editing = false; kb?.hide() },
            onSend = { editing = false; kb?.hide() },
            onNext = { editing = false; kb?.hide() },
        ),
        // Transparent border when focused → the gold triremeFocus ring is the ONLY focus
        // indicator (no doubled border); visible outline when unfocused.
        colors = OutlinedTextFieldDefaults.colors(
            focusedBorderColor = Color.Transparent,
            unfocusedBorderColor = MaterialTheme.colorScheme.outline,
        ),
        modifier = modifier
            .onFocusChanged {
                focused = it.isFocused
                if (!it.isFocused) editing = false // leaving the field always exits edit
            }
            // Preview so we intercept OK / RIGHT *before* the field can swallow them.
            .onPreviewKeyEvent { ev ->
                if (ev.type != KeyEventType.KeyDown) return@onPreviewKeyEvent false
                // Backspace/Delete are EDITING keys the soft keyboard forwards as key events
                // even while it's up — they must delete a character, NOT be read as "navigating
                // away". Without this exception, pressing Backspace on the on-screen keyboard
                // flips editing=false → readOnly=true → the keyboard slams shut mid-edit (so you
                // can never correct a typo on a TV). Let them through to the field, which deletes
                // a char while editing and harmlessly ignores them while readOnly.
                if (ev.key == Key.Backspace || ev.key == Key.Delete) {
                    return@onPreviewKeyEvent false
                }
                // Any OTHER key reaching the field means the keyboard is DOWN (the IME owns the
                // D-pad while up), i.e. we're navigating — so normalize out of any stale edit
                // state. This is how a keyboard dismissed by Back (which the IME swallows) gets
                // observed.
                if (editing) editing = false
                when {
                    ev.key == Key.DirectionCenter || ev.key == Key.Enter ||
                        ev.key == Key.NumPadEnter -> {
                        editing = true; kb?.show(); true // enter edit; Compose auto-starts the session
                    }
                    // Keyboard is down (see above), so RIGHT jumps to the action button.
                    // The requester's target may not be composed right now (the pair
                    // dialog swaps Start — which carries the requester — for Close after
                    // success); requestFocus() on a detached requester throws
                    // IllegalStateException and killed the app on the X90J. Fall back to
                    // default D-pad focus search instead of consuming the key.
                    ev.key == Key.DirectionRight && rightFocus != null -> {
                        runCatching { rightFocus.requestFocus() }.isSuccess
                    }
                    else -> false
                }
            }
            // Touch path is independent of the D-pad path: a finger tap edits immediately.
            .pointerInput(Unit) {
                detectTapGestures(onTap = { if (!editing) { editing = true; kb?.show() } })
            }
            .triremeFocus(focused), // the sole focus ring — gold, D-pad focus = selection
    )
}
