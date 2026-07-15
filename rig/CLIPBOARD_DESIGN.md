# Clipboard access/transfer — design (Pharos host ⇄ Trireme client)

Goal: bidirectional **text** clipboard sync between the Pharos Wayland host and the
Trireme client, over the existing QUIC connection. Text/UTF-8 only in v1 (images/
files deferred). "Copy on the host, paste on the phone" and vice versa.

## Ground truth (verified on `feature/clipboard-transfer`, ex `origin/main`)

- Wire protocol: `crates/core-proto/src/lib.rs`. `apps.rs` is the template for a
  small enum + postcard `encode_*/decode_*`. Framing everywhere: `u32 BE len || postcard`.
- **App relay** (the template, `transport/src/lib.rs`): host `conn.open_bi()`
  (server-initiated), client `conn.accept_bi()`. `tokio::join!(read, write)`.
  Backed by `TransportHandle.app_catalog_tx: watch::Sender<Vec<AppInfo>>` (host→client)
  and `app_launch_rx: Mutex<Option<mpsc::UnboundedReceiver<AppMsg>>>` (client→host).
  Compositor calls `send_app_catalog()` / `take_app_launch_rx()`.
- **Input relay**: client `conn.open_bi()` (client-initiated), host `conn.accept_bi()`.
  Client→host only in practice.
- QUIC fact: client-initiated and server-initiated bidi streams are **separate ID
  spaces**. That is *why* input (client-opened) and app relay (host-opened) never
  collide — each space has exactly one stream today. Adding a second stream to
  either space needs disambiguation (order or tag), or streams will be misrouted.
- Compositor selection: `crates/compositor/src/lib.rs` has `DataDeviceState` (l.130,
  init l.164), an **empty** `SelectionHandler for State` (l.403), `DataDeviceHandler`
  (l.406). No primary-selection, no data-control.
- smithay 0.7 selection API (verified in the vendored source):
  - `SelectionHandler::new_selection(&mut self, ty: SelectionTarget, source: Option<SelectionSource>, seat)`
    — fires when a **client** sets the selection. `ty == Clipboard` is our case.
  - `SelectionHandler::send_selection(&mut self, ty, mime_type: String, fd: OwnedFd, seat, user_data: &SelectionUserData)`
    — fires when a client wants to **read** a *compositor-set* selection; we write bytes to `fd`.
  - `request_data_device_client_selection(seat, mime_type, fd) -> Result<(), SelectionRequestError>`
    — asks the current **client-owned** selection to write its bytes into `fd` (host reads the read end).
  - `set_data_device_selection(dh, seat, mime_types: Vec<String>, user_data: SelectionUserData)`
    — makes the **compositor** the selection owner (host apps can now paste).
  - `type SelectionUserData: Clone + Send + Sync + 'static` — we set it to the payload bytes.
- Client FFI: `crates/core-ffi/src/lib.rs`. `AppCatalogSink` (`callback_interface`,
  host→client), `GamepadInput.app_tx` + `launch_app()` (client→host), `start_video_stream(...)`
  (l.464) wires sinks + channels into `transport::run_video_client_with_input`.
- No clipboard crate is a client dep yet (`arboard`/`copypasta` absent).

## Wire protocol (core-proto, new `clipboard.rs`)

```rust
pub enum ClipboardMsg { Text(String) }          // extensible: add Mime{ mime, data } later
pub fn encode_clipboard(&ClipboardMsg) -> Result<Vec<u8>, WireError>   // postcard
pub fn decode_clipboard(&[u8]) -> Result<ClipboardMsg, WireError>
```
No `PROTOCOL_VERSION` bump: clipboard lives on its own stream that old peers never
open/accept — additive, like the app relay was.

## Transport multiplexing decision

**Chosen: a dedicated CLIENT-initiated bidi "clipboard relay", accepted by the host
*serially after* the input stream in the same task, leaving the freshly-validated
app relay 100% untouched.**

- Client (`run_video_client_with_input`): after `conn.open_bi()` for input, do a
  second `conn.open_bi()` for clipboard (only if the client wants clipboard).
- Host input-accept task: `accept_bi()` → **spawn the input reader** → then
  `accept_bi()` again for clipboard. Spawning the input reader *before* the 2nd
  accept means input is never gated on the clipboard stream. quinn yields accepted
  streams in ascending stream-ID = open order, and both awaits are sequential in one
  task, so #1=input, #2=clipboard deterministically (no cross-task race).
- Old client that never opens stream #2: the host's 2nd `accept_bi().await` simply
  parks forever (harmless; input already handled by its spawned reader).
- Rationale for not tagging/ordering the *app* relay: app-launch was just validated
  in PR #23; keep it byte-for-byte unchanged. The input relay's accept becomes a
  serial-of-2 but its per-event framing is unchanged and accept happens once at
  connect (no latency impact).

Clipboard relay carries `ClipboardMsg` both ways (`u32 BE len || postcard`), capped
(e.g. 1 MiB) defensively like the app relay's 64 KiB cap.

`TransportHandle` gains, mirroring the app fields:
- `clipboard_out_tx: watch::Sender<ClipboardMsg>` (host→client; latest-wins is
  correct for a clipboard). Writer skips the initial empty value.
- `clipboard_in_rx: Mutex<Option<mpsc::UnboundedReceiver<ClipboardMsg>>>` (client→host).
- `send_clipboard(&self, text: String)` and `take_clipboard_in_rx(&self)`.

## Host side (compositor)

`type SelectionUserData = std::sync::Arc<Vec<u8>>` (the clipboard bytes for
compositor-owned selections).

**Host → client (a host app copied):**
1. `new_selection(Clipboard, Some(source), seat)`: a host client set the clipboard.
   Guard: if `source` mime types don't include a text flavor, ignore. Skip when the
   change is our own compositor-set selection (see echo guard).
2. Create a pipe (`rustix`/`nix` `pipe()`), call
   `request_data_device_client_selection(seat, "text/plain;charset=utf-8", write_fd)`,
   **move the read fd to a detached std::thread** that reads to EOF (bounded, e.g.
   ≤1 MiB), decodes UTF-8, and pushes `TransportHandle::send_clipboard(text)`.
   NEVER read on the calloop thread (render-loop-must-not-block invariant).
3. Echo guard: keep `last_seen_hash`; skip if the text equals what we last set/sent.

**Client → host (client copied, host app pastes):**
1. Compositor drains `clipboard_in_rx` in the calloop loop (mirror how `app_launch_rx`
   is drained in `multi.rs` ~l.2622). On `ClipboardMsg::Text(t)`:
   `set_data_device_selection(dh, seat, vec!["text/plain;charset=utf-8".into(),
   "text/plain".into(), "UTF8_STRING".into(), "STRING".into()], Arc::new(t.into_bytes()))`.
   Record `last_set_hash` so the resulting `new_selection` (compositor source) is not
   echoed back.
2. `send_selection(Clipboard, mime, fd, seat, user_data)`: a host app is pasting.
   Write `user_data` bytes to `fd` on a detached thread (pipe write can block if the
   paster is slow); match the requested mime loosely (any text flavor → the bytes).

Mime flavors offered/accepted: `text/plain;charset=utf-8`, `text/plain`,
`UTF8_STRING`, `STRING`.

## Client side (core-ffi + platforms)

- `ClipboardSink` (`callback_interface`): `fn on_clipboard(&self, text: String)` —
  host→client; the platform shell sets its OS clipboard.
- `GamepadInput.clipboard_tx: mpsc::UnboundedSender<ClipboardMsg>` + method
  `set_clipboard(&self, text: String)` (client→host). Client echo guard: don't
  re-send text we just received from the host.
- `start_video_stream(...)` gains `clipboard_sink: Option<Box<dyn ClipboardSink>>`;
  create the `(clipboard_tx, clipboard_rx)` pair; pass both into
  `run_video_client_with_input`.
- Client transport: open the 2nd bidi (clipboard) after input; spawn a reader
  (host→client → `on_clipboard`) and writer (drain `clipboard_rx` → frames).
- Android (Kotlin): `ClipboardManager`. Implement `ClipboardSink.onClipboard` to set
  the primary clip; observe `OnPrimaryClipChangedListener` (or a "send clipboard"
  action) to call `GamepadInput.setClipboard`. Regenerate uniffi bindings.
- Desktop trireme: optional `arboard` behind a feature; out of scope for v1 unless trivial.

## Echo-loop safety (both ends)

Each side holds `last_hash` of the text it most recently *applied from the peer*.
Before sending local clipboard, skip if it equals `last_hash`. Host additionally
distinguishes compositor-owned vs client-owned selections so its own
`set_data_device_selection` doesn't bounce out via `new_selection`.

## REVISION after adversarial review (supersedes the transport section above)

The original "second client-initiated bidi" plan was WRONG: the host runs an
unbounded `loop { accept_bi() -> decode_input }` in BOTH `serve_stream`
(transport/src/lib.rs:2372) and `run_mirror_loop` (:2022) that treats every
client-initiated bidi as input (4 KiB frame cap). A client-opened clipboard stream
would be swallowed as input. Corrected design:

- **Clipboard rides the HOST-initiated (server-initiated) bidi space, beside the app
  relay** — a separate QUIC stream-ID space from input, so ZERO collision with the
  input loop and NO capability handshake needed. The host opens the app relay
  (idx0) then the clipboard relay (idx1) **sequentially in one task** (deterministic
  open order; quinn yields accepts in ascending stream-id = open order, verified).
  The client accepts idx0=app then idx1=clipboard. App relay bytes are UNCHANGED
  (tagging it would break old clients). Compat: old host never opens idx1 → new
  client's 2nd accept parks harmlessly; old client never accepts idx1 → host's
  clipboard writer parks harmlessly. Both directions ride this one bidi stream
  (host writes on its send half, client writes on its send half), exactly like the
  app relay carries catalog + launch.
- The real host type is `QuicStreamer` (not "TransportHandle"). It is NOT Clone and
  is owned inline on the render stack. Add `clipboard_sender(&self) ->
  watch::Sender<ClipboardMsg>` (watch::Sender IS Clone+Send), mirroring `audio_handle()`.
- `State` is built (multi.rs:1416) BEFORE the streamer (multi.rs:1712). So
  `State.clipboard_out: Option<watch::Sender<ClipboardMsg>>` starts `None` and is
  injected after the streamer exists.

**Host read timing (major fix):** `new_selection` runs BEFORE smithay commits the
new selection (device.rs:141 commits after :134 calls back). Do NOT call
`request_data_device_client_selection` synchronously inside `new_selection`. Instead
`new_selection` records `State.pending_clipboard_mimes = Some(source.mime_types())`;
the render loop (next turn, near the app_launch drain at multi.rs:2622) does the
pipe + `request_data_device_client_selection(seat, chosen_mime, write_fd)` after the
commit, then spawns a detached reader thread (UTF-8 validate, drop on error, ≤1 MiB)
that pushes via a cloned `clipboard_out` sender. Pick `chosen_mime` from a preference
list [`text/plain;charset=utf-8`, `UTF8_STRING`, `text/plain`, `STRING`] intersected
with the offered mimes.

**Echo guard (corrected):** compositor-set selections do NOT re-fire `new_selection`
(verified: set_data_device_selection -> set_clipboard_selection directly, no handler
callback). So the host-side new_selection echo guard is dead code — drop it. The
load-bearing guard is CLIENT-side: track `last_applied_hash`; don't re-send text the
client just received from the host. Add a cheap host-side content-hash de-dup too
(skip applying client->host text identical to what we last sent host->client).

**Clear + UTF-8 (minor fixes):** `new_selection(source=None)` (a host app cleared
the clipboard) -> send `ClipboardMsg::Text("")`; client applies `Text("")` by
clearing its OS clipboard. Host fd reader uses `String::from_utf8`, DROPs on error
(never panics). Offer text/plain;charset=utf-8 + UTF8_STRING + text/plain + STRING
when SETTING; request the first offered text flavor when READING.

**Gating + scope (v1 policy):** clipboard binds to the SAME streamer input/app use
(first streamer via find_map) — consistent with input's existing single-primary
binding; mirror is dead-by-default. The `PHAROS_CLIPBOARD` env var gates both
directions (`PHAROS_CLIPBOARD=0` disables clipboard sync; default on); per-device/
per-direction gates are a follow-up. `new_selection` only fires for the keyboard-FOCUSED client, so host->client
sync works while an app is focused (not on the idle screen) — acceptable.

**Compat note:** no `PROTOCOL_VERSION` bump (own stream, additive). Adding
`clipboard_sink` to `start_video_stream` touches all callers (Kotlin StreamHub.kt +
GyroCalibrationDialog.kt, Rust trireme/src/play.rs, transport tests, audio_e2e) —
pass `None`/no-op. Regen uniffi bindings via rig/build-android.sh (checksum guard
hard-fails a stale-binding APK).

## Implementation-review fixes (2026-07-07, all 8 confirmed findings addressed)

An adversarial review of the IMPLEMENTED diff (3 lenses → per-finding verification)
confirmed 8 real issues (0 refuted); all fixed:
- **[major] client→host dead until host copies first** — a host-opened QUIC bidi is
  invisible to the peer until the opener writes, so the client's `accept_bi` parked
  and its client→host writer never started. Fix: the host writer NUDGES the stream
  open with a zero-length frame; both readers skip `len == 0`. Regression test
  `clipboard_client_to_host_without_prior_host_copy`.
- **[major] render-loop DoS** — the client→host mpsc was drained uncapped on the
  render thread; a flooding client could trip the 1.5s watchdog. Fix: coalesce to
  the newest text (last-writer-wins) with a per-frame drain cap (256).
- **[major] extend-mode leak** — only the first streamer's receiver was drained.
  Fix: take + drain EVERY streamer's `clipboard_in_rx`.
- **[minor] writer parked after disconnect** — `join!` → `select!` in clipboard_relay.
- **[minor] reader thread could park forever** — `read_selection_text` now uses
  non-blocking + `poll(2)` with a 5s deadline.
- **[minor] stale secret replay** — dropped the resync-on-connect; only live,
  in-session host copies flow to a client (a freshly-joined device never auto-gets a
  pre-existing host clipboard).
- **[nit] UTF-8 truncation dropped the whole paste** — deliver the `valid_up_to`
  prefix instead.
- **[nit] stale doc** in core-proto clipboard.rs corrected (host opens, not client).
- (client) Android: clear `lastSentClip` on host-applied clipboard so re-copying an
  earlier value still propagates after the host changed ours.

## Risks / must-verify

1. quinn accept order == open order (relied on for input-then-clipboard). Verify.
2. `new_selection` firing for compositor-set selections (would need the echo guard).
3. Reading/writing selection fds must be fully off the calloop thread.
4. Unbounded clipboard size — cap on both the wire (relay) and the fd reader.
5. `request_data_device_client_selection` when the current selection is
   compositor-owned returns `ServerSideSelection` — handle gracefully (that's our own).
6. Security: clipboard can carry secrets. Make client→host and host→client each
   gate-able; default on, but respect existing per-device input gates? (clipboard ≠ input;
   decide whether the input gate also gates clipboard.)
