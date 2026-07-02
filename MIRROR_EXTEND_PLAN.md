# Multi-client streaming: mirror (share one encode) vs extend (separate streams)

> Design note, 2026-06-25. NOT built — captures the decided shape + a hard panel
> requirement before implementation. Prerequisite for everything below: lift the
> current single-client slot (the streamer accepts one client and **pre-empts**
> the rest — `crates/transport/src/lib.rs:1379`, `:1476`).

## Decided behavior

| Scenario | Encodes | Per-client adaptation | Notes |
|---|---|---|---|
| **1 host → 1 client** | 1 | **Full.** Live bitrate (NVENC RC reconfigure) + fps pacing per session. | Today's behavior. Unchanged. |
| **Extend → N clients** | N (one per `Output`) | **Full, independent** per stream. | Native grain (see below). Bounded by NVENC ~3-session cap + the pixel budget. |
| **Mirror → N clients** | **1**, fanned out | **None per-client** — one shared stream clamped to the **lowest-common-denominator** client (resolution **and** frame rate **and** bitrate). | Accepted tradeoff. **Must be surfaced in the panel (see requirement).** |

**Why mirror loses per-client adaptation:** one NVENC session = one resolution, one
fps, one RC bitrate. The shared stream must fit the *weakest* connected client —
min() across clients on decode capability (res/codec/fps) and network (QoS/RTT/loss).
The W8 pacer + `reconfigure_bitrate` are per-session, so they'd drive the *single*
shared session toward the most-constrained client.

## How it maps onto the existing architecture

The compositor is already built as **N independent pipelines — one Smithay `Output`
+ render-target + NVENC `Session` + `QuicStreamer` each** (`crates/compositor/src/multi.rs:4`,
`:221`, `:243`), capped at ~3 because consumer NVIDIA limits concurrent NVENC sessions
(`multi.rs:201`). So:

- **Extend = the native model.** Each extended display is its own `Output` → its own
  encode → its own stream, each drawing from `max_pixels_per_sec` (Σ w·h·fps). Bounded
  by the ~3-session NVENC cap.
- **Mirror = a fan-out to add.** Today, overlapping two outputs at (0,0) (`multi.rs:9`)
  gives *two* encodes of identical content — no saving. The optimization keeps **one**
  `Output`/encoder and fans its encoded bitstream to N client connections. Two changes:
  1. **Channel:** the encoded-frame path is an `mpsc` (single-consumer, `transport/src/lib.rs:365`,
     `:449`). Swap to a `broadcast` / per-client queues so N client loops each receive
     the same `VideoChunk`s.
  2. **Accept loop:** today single-client serial + pre-empt. For mirror, *keep* N
     connections and feed all, each with its own bounded drop-oldest queue (reuse the
     existing per-client QoS pacing) so one slow TV can't stall the others.

**Mirror sidesteps the ~3-session NVENC cap** for same-content viewers (1 session, many
clients) — extend is the path bounded by it.

## What "saves bandwidth" actually means here

- **GPU encode + pixel budget → saved by mirror** (1 session/encode instead of N). This is
  the scarce + fault-prone resource (the ~3-session cap; the 3070 Ti faults).
- **Host uplink bytes → NOT saved by unicast.** Identical frames to N clients over N QUIC
  connections is still N× the wire bytes. True wire sharing needs **multicast**, a poor fit
  (QUIC is unicast; multicast is LAN-segment-only, no WAN; loses QUIC encryption/reliability/
  pacing + the Ed25519 channel binding). **Do not** pursue multicast for this stack.

## HARD REQUIREMENT — panel must surface the mirror throttle

When the user **manually initiates mirror mode**, the compositor panel
(`crates/pharos-panel/src/tabs/compositor.rs`, with a mirror of the badge on Overview)
**must clearly indicate that resolution and frame rate (and bitrate) are throttled** —
i.e. the stream is clamped to the lowest-common-denominator client, *not* running at each
client's native capability. Concretely:

- A visible **"Mirror — quality limited"** state on the compositor view while mirror is active.
- Show the **clamped values** actually being encoded: `1080p @ 60` (and bitrate), and ideally
  **why** — name/flag the limiting client (e.g. "capped by *living-room-tv*: 1080p60").
- Make it unmistakable that this is the *expected* consequence of mirror, not a fault — phrase
  as a throttle/limit, not an error.
- This needs new signal in `HostStatus`: stream **mode** (single/mirror/extend), the **clamped
  res/fps/bitrate**, and per-client caps so the panel can name the limiter. The single-client
  and extend paths must NOT show the throttle badge (they adapt per-client).

## Status / order of work

1. Lift the single-client slot (prerequisite — keep-N connections instead of pre-empt). **TODO**
2. Mirror fan-out: `broadcast` channel + per-client drop-oldest queues. **TODO**
3. ✅ **DONE (stage 1, commit `1d184f7`)** — `HostStatus.stream_mode` (`StreamMode::Single | Mirror{clamp}`)
   + `MirrorClamp` + `Request::SetStreamMode{mirror}` in `control-ipc`; compositor `MIRROR_ENABLED`
   flag published into the status push; panel Compositor-tab toggle + "quality limited" throttle
   badge with the real clamp numbers (the requirement above). Single-client path byte-unchanged;
   host still reports/serves `Single` until steps 1–2 land, and the panel says so honestly.
4. Extend client→stream binding (which client attaches to which `Output`). **TODO**
5. Pixel-budget accounting across modes (mirror = 1×; extend = Σ). **TODO**

## Stage 2 UI requirements (folded in 2026-06-25 — depend on the engine being live)

These were requested but **must not be built before the fan-out engine** — without it the host is
single-client, so `client_count ≤ 1`, the encode isn't shared, and a "mirror budget / N clients"
display would be fiction. Build them *with* the engine, driven by real `HostStatus.clients`.

1. **Mirror-aware encode pixel budget** (`compositor.rs` canvas overlay `:1236-1267`, save-time
   warning `:2489-2500`). Today `pps = Σ(o.w·o.h)·fps` over every `OutputTile` — correct for extend,
   but in mirror N peer tiles collapse to ONE shared encode, so Σ over-counts N×. When
   `stream_mode == Mirror` AND the engine is genuinely sharing: `pps = (one shared output w·h)·fps`,
   labelled "1× shared (saves N−1×)". `N` = clients actually sharing (from the engine), NOT tile count.
   Gate the "saves" claim on real fan-out so it never overstates.
2. **Per-client "client cards" on the canvas** (replace the fixed-anchor name/res/port texts at
   `compositor.rs:1062-1086`). Consolidate each client's name + res·fps + port into one compact card.
   Per-client data path (from the workflow map): name ← `state.peers` by `ClientStatus.pubkey`; res ←
   `state.device_geoms[pubkey]`; fps ← `ClientStatus.fps`; port ← `ClientStatus.addr`. Real only once
   the engine reports >1 connected client.
3. **No client-card overlap, even in mirror** (the layout-overlap part is engine-independent and could
   land early). Cluster `OutputTile`s by AABB overlap (reuse `:1026-1032` / `has_overlap` `:2507`);
   for each cluster grid-pack the member cards within the cluster's union bounding box
   (`cols = ceil(sqrt(n))`, cells never overlap), clip each card to its cell (`painter.with_clip_rect`)
   so text can't bleed. Singleton cluster → one card filling its tile. This is the "dynamically moving
   in their bounding box" the user asked for.

## Stage 2 engine — locked implementation plan (design panel 2026-06-25)

Chosen model: **per-connection actor + gated central dispatcher** (NOT broadcast-everywhere). Single-client
stays byte-identical because the mirror branch is **dead code when `MIRROR_ENABLED=false`** (the default —
the only state validatable without a 2-client rig). Key decisions: keep the existing `mpsc<VideoChunk>` for
BOTH modes (producer untouched); per-client `mpsc` actor queues (not broadcast) for attributable drop
accounting + per-client fps; dispatcher must `try_send` drop-oldest, **never await** a slow client; **cap N
= 4** with a distinct "host full" close code; **primary-only** input/app-launch/bench fan-out (one smithay
seat); **require-reconnect** on a mirror toggle (a benign `MODE_SWITCH` close code, no NVENC re-init);
`connected` becomes a fetch_add/sub COUNT not a bool; RAII deregister per actor.

- **Step 1 ✅ (`36ab1a0`)** — `VideoChunk.au` → `Arc<[u8]>` + `#[derive(Clone)]` (cheap fan-out, wire-transparent).
- **Step 2 ✅ (`d2a5dc3`)** — extract shared `authenticate()` handshake; single SESSION loop untouched;
  verified by the `enforce.rs` real-handshake test.
- **Steps 3–5 ✅ (`a3e976b`)** — the keep-N engine, gated dead-by-default. `QuicStreamer` gained an
  injected `mirror: Arc<AtomicBool>` (compositor `mirror_flag()`) + `clients: Arc<Mutex<Vec<ClientSlot>>>`
  registry + `connected_clients()`. `serve_stream`: `if mirror { run_mirror_loop(&mut rx, …); continue }`,
  single body unchanged below. `run_mirror_loop` borrows `&mut rx` and `select!`s accept (auth+admit in a
  spawned task) / `rx.recv` (fan Arc-clone to every slot, **try_send drop-newest, never awaits**) / audio
  (datagram→all) / gyro (→primary). `admit_mirror_client`: per-conn actor drains its queue, measures fps,
  RAII-deregisters; input task forwards only while primary (`min(id)`, auto-reassigns). `MIRROR_MAX_CLIENTS=4`
  + `CAPACITY_CLOSE_CODE`/`MODE_SWITCH_CLOSE_CODE`. Status push emits one `ClientStatus` per mirrored client
  (single → ≤1 → byte-identical); `MirrorClamp.client_count` now truthful.
  - v1 deltas from the locked plan: **drop-newest** per slot (not drop-oldest — both are non-blocking; simpler,
    matches the single path); `connected` stayed a **bool** (set/cleared under the registry mutex, no flap);
    **audio fans to all** now (not the planned primary-only-v1). App-relay + per-client QoS in mirror are NOT
    wired yet (deferred with step 7).
- **Step 6 ✅ (in `a3e976b`)** — mirror→single is automatic (the 200ms `!mirror` check in `run_mirror_loop`
  closes all with `MODE_SWITCH_CLOSE_CODE` + returns); single→mirror applies on the next connect/reconnect
  (the active single session isn't interrupted — require-reconnect, as planned).
- **Stage 2b ✅ (`3a38d22`)** — panel: mirror-aware pixel budget (Σ → 1× shared, "−N×" label) on the canvas
  overlay + save-time warning; per-client "client cards" (cluster overlapping tiles by AABB union-find,
  grid-pack `ceil(sqrt(n))` cols within the cluster bounding box, clip per card → never overlap).
- **Step 7 (DEFERRED — needs the 2-client rig)** — min-across-clients clamp + `limited_by` (needs a
  client→host decode-caps datagram) + per-client QoS + app-relay in mirror. Until then `limited_by=None`
  + truthful `client_count` is the honest interim.

⚠️ **NOT live-validated** — the whole engine is dead-by-default; it needs a supervised run streaming to
**2 real devices** (mirror ON) before mirror can be claimed working. Single-client is byte-identical + green.

Open risks to settle ON THE RIG: dispatcher never head-of-line-blocks (unit-test a full slot), lagged client
recovers within ≤1s IDR, primary reassignment on primary-disconnect (oldest live slot), connected-count flap,
`MODE_SWITCH` reads as a clean redial not "Preempted". Uplink is N× (mirror saves GPU encode, not wire bytes).

See [[ADAPTIVE_QUALITY_SCOPE.md]], [[project-encode-pixel-budget]],
[[lesson-host-single-client-serial]], [[project-pairing-seamless]].
