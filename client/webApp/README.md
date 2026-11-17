# webApp — Trireme browser client (the zero-Dev-Mode fallback)

A browser-based Trireme client that streams a Pharos host into a plain web page:
WebTransport for the wire, WebCodecs for decode, the Gamepad API for input. It
needs **no Developer Mode, no Partner Center enrollment, no sideloading, no
signing** — it runs in the console's Microsoft Edge (or any modern Chromium
browser) the same way a website does.

It exists as the deliberate second route to the same console. The native
[xboxApp](../xboxApp/README.md) is the front door (better latency, full control)
but it is gated behind Microsoft's developer enrollment (paid-then-free, now an
ID+selfie identity check). This is the vent: if that gate is shut, closed, or
simply not worth the hassle, the browser client reaches the same Xbox with
nothing but a URL. Keep **both** viable; never bet the whole console on one gate
opening. (See the memory note `feedback-multiple-paths`.)

> Status: **SCOPE ONLY.** Nothing here is built yet. Start at the Phase-0 gate.

## The existential gate (Phase 0) — verify before building anything

The native client's gate was "does quinn's QUIC survive the UWP sandbox?" (it
did, 2026-07-16). The browser client's gate is:

> **Does the Xbox's specific Edge build expose WebTransport + WebCodecs (HW
> HEVC/AV1) + the Gamepad API, in a secure context?**

This cannot be answered from a desk. Desktop Edge/Chromium has shipped all of it
for years (WebTransport + `serverCertificateHashes` since Chromium 100;
hardware HEVC in WebCodecs since 107/108; AV1 since 94-ish), **but the Xbox Edge
browser lags desktop and runs in a constrained TV profile**, so its build may
omit or gate these. Answer it first with `capability-probe.html` (in this dir):
serve it over HTTPS, open it in the console's Edge, read the report. Branches:

- **All green** → build the full path below (WebTransport + WebCodecs).
- **No WebTransport, WebCodecs OK** → drop to the transport ladder (WebSocket, or
  WebRTC) and keep WebCodecs. Higher latency, still hardware decode.
- **No WebCodecs HEVC** → fall back to **AVC** (H.264 is the most universally
  decodable; the host already emits AVC as of 2026-07-14) via WebCodecs `avc1`,
  or ultimately **MSE** (Media Source Extensions, fragmented-MP4) which works on
  nearly everything but adds buffering latency.
- **Neither** → the browser fallback is not viable on this console; stay native.

Because the probe only reads API presence, it needs no host changes — it is the
cheapest possible way to kill or green-light the whole effort.

## Architecture — two options, ship the bridge first

The browser cannot run quinn, tokio, or the Rust core's transport: it only has
WebTransport / WebSocket / WebRTC. So the client JS/TS is genuinely new code.
The **host-facing** side, though, can reuse everything:

### Option A — `pharos-web-bridge` (MVP, recommended first)

```
 browser (Edge on Xbox)                pharos-web-bridge                 Pharos host
 ├ WebTransport session   ── HTTP/3 ──▶ ├ wtransport server              │
 ├ WebCodecs decode                     ├ static file server (the SPA)   │
 ├ Gamepad API                          └ native Trireme client  ── QUIC ▶ (unchanged)
                                           via core-ffi FrameSink/AudioSink
```

A new binary that is a **native Trireme client on one side and a WebTransport
server on the other**. It calls the same `core_ffi::start_video_stream` every
client uses, implements `FrameSink`/`AudioSink` (confirmed traits: `on_frame(
Vec<u8>)` access units, `on_packet(Vec<u8>)` Opus) to shove encoded units into
WebTransport streams, and forwards Gamepad input back through the returned
`GamepadInput`. **Zero host changes**; reuses the entire native stack; the only
new Rust is the ~WebTransport plumbing. Cost: one extra LAN hop (sub-millisecond)
and a re-frame. Perfect for a fallback and fast to a first demo. It also serves
the SPA and the current cert hash, so the browser is fully self-provisioned from
one origin.

### Option B — native WebTransport endpoint on the host (graduate to later)

Teach the host itself to serve WebTransport, sharing the encoder's frame source
between the quinn-native and WebTransport sessions. No extra hop, but real host
refactoring (abstract the session transport) and it needs SPAKE2-in-WASM so the
browser can pair directly. Do this only if the bridge's extra hop ever proves to
matter.

## Component map (client side)

| Concern | Web API | Fed by / notes |
|---|---|---|
| Transport | **WebTransport** (HTTP/3); unidirectional streams for video/audio, a bidi stream or datagrams for input | falls back to WebSocket / WebRTC per the gate |
| Video decode | **WebCodecs** `VideoDecoder`, `hardwareAcceleration:"prefer-hardware"`, `optimizeForLatency:true` | host's raw HEVC/AV1 AUs → `EncodedVideoChunk`. Use **`hev1`/`av01`** (in-band SPS/PPS) so we feed units with inline parameter sets and need no separate `description` — matches the host's opener-NAL sniffing |
| Present | `VideoFrame` → `<canvas>` via `drawImage` (or WebGL for a tonemap shader); `requestVideoFrameCallback` pacing | port the client HDR tonemap as WebGL later |
| Audio | **WebCodecs** `AudioDecoder` (`opus`) → `AudioWorklet` ring buffer | host's Opus packets → `EncodedAudioChunk` |
| Input | **Gamepad API** poll loop → `core_proto::InputEvent` → transport | canonical index mapping shared via the WASM proto module |
| Discovery | manual host URL for MVP; later a small host-list served by the bridge | mDNS isn't reachable from a browser |

## Transport & trust (the self-signed cert model)

WebTransport to a LAN host with no CA cert is exactly the case
`serverCertificateHashes` was built for, and it lines up with the existing
"[TLS cert = opaque tunnel](../../)" lesson (the cert is never the trust anchor;
SPAKE2 pairing is). Browser-enforced constraints, verified:

- Cert **must be ECDSA P-256 (secp256r1)**, **not RSA**.
- Cert validity **must be ≤ 14 days** → the bridge regenerates roughly every
  13 days and republishes the hash.
- The browser pins `serverCertificateHashes: [{algorithm:"sha-256", value:…}]`;
  the bridge **embeds the current hash in the served page**, so the SPA always
  has a fresh pin without a CA. (The host's existing rcgen certs are the wrong
  shape for this — the bridge mints its own short-lived ECDSA cert.)

Auth ladder: MVP trusts the LAN (bridge pairs with the host once, natively; the
browser presents a short PIN or token to the bridge). Later, compile
`core-crypto`'s SPAKE2 to WASM for a real browser↔host pairing (Option B).

## Reuse map (fighting drift)

- **Bridge**: reuses `core-ffi` wholesale (`start_video_stream`, `FrameSink`,
  `AudioSink`, `GamepadInput`). No protocol re-implementation.
- **Browser ↔ bridge wire**: define via a small **`wasm-bindgen` module that
  compiles `core-proto`** (message framing, `InputEvent`, gamepad indices) to
  WASM, so the browser and the Rust side can't drift. Add `core-crypto` SPAKE2
  to the same module when Option B lands.
- **Codec sniffing**: reuse the host's opener-NAL behavior by choosing `hev1`/
  `av01` in-band configs; do NOT re-sniff mid-GOP (that was the black-screen bug
  `efe4d8b`).

## Phased plan (gate first, mirror the native discipline)

0. **`capability-probe.html`** in the console's Edge → API report. GO/NO-GO.
1. **WebTransport echo**: browser opens a WebTransport session to a stub bridge
   with the pinned self-signed cert; round-trip a datagram. Proves the transport
   survives the console browser (the browser analogue of native Phase 1).
2. **First pixels**: bridge runs a real native stream and forwards encoded HEVC
   AUs; browser `VideoDecoder` → canvas.
3. **Playable**: Gamepad API → bridge → host.
4. **Audio**: Opus via `AudioDecoder`.
5. **Harden**: PIN/SPAKE2 auth, AV1, reconnect/IDR request, HDR canvas, a served
   host list. Consider Option B if the hop matters.

## Risks / unknowns

- **Xbox Edge API coverage** (Phase 0) — the whole thing hinges on it; unverifiable remotely.
- **Secure context** — WebTransport/WebCodecs are secure-context-only. The SPA
  must be served over HTTPS; on the LAN that means a self-signed cert and a
  click-through warning in Edge, or serving via your own real HTTPS origin.
- **Cert rotation** — the ≤14-day ECDSA cert needs an automatic regen+republish
  in the bridge, or WebTransport silently stops connecting after two weeks
  (a WAN-port-style trap).
- **Input latency & focus** — browser gamepad polling and background-tab
  throttling are worse than native; keep the tab foregrounded/fullscreen.
- **HDR** — canvas HDR in browsers is limited; SDR-tonemap first (reuse the
  client tonemap as WebGL), treat HDR as a stretch goal.
- **Bridge hop** — sub-ms on the LAN, but real; Option B removes it if needed.
