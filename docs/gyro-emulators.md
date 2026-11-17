# Gyro aiming in emulators

Aim with motion in a Switch emulator running on your PC while you play on a
phone or TV. Pharos runs a **DSU motion server** (the "cemuhook" protocol that
Lichii/Eden, Cemu, Dolphin, and others read), fed by the gyro of whatever you
hold: your phone, or a controller with its own gyro.

**Tested:** Lichii, with a GameSir G8 clamp controller and the phone's own gyro
(Xiaomi 14T Pro): Lichii's motion test passes, and aiming follows the phone in
game. A Flydigi Vader 5 Pro forwarded over USB/IP streams its gyro to the motion
server too (checked with a DSU client); it hasn't been tried in Lichii yet.

## How it fits together

```
phone gyro / controller gyro
        │  (Trireme sends motion with the pad's input)
        ▼
Pharos host ── per-controller gyro settings (invert, axes) ──▶ DSU server 127.0.0.1:26760
                                                                    │
                                                                    ▼
                                                     Lichii: motion source = CemuhookUDP
```

The pad games see on the PC is a virtual Xbox pad, which has no motion sensors.
That's why motion travels separately, over DSU: the emulator reads buttons from
the pad and motion from the DSU server, and pairs them as player 1.

## Setting it up

### 1. Turn on the motion server (host, once)

In **pharos-panel → Settings → Motion**, tick **Share client gyro over DSU**.
Or in `~/.config/pharos/config.toml`:

```toml
[dsu]
enabled = true
```

It takes effect within a couple of seconds; no restart needed.

### 2. Send gyro from Trireme (client)

In Trireme's **Settings**:

- **Send controller gyro to host**: On. This applies the next time you connect.
- **Gyro source**: **Phone** to use the phone's own sensors (best for clamp
  controllers like the G8, Backbone, or Razer Kishi), or **Controller** to use a
  pad's own gyro where Android exposes it.

The phone gyro works with **Controller handling** on or off.

### 3. Point Lichii at it

In Lichii (and other Eden-derived emulators), go to **Emulation → Configure →
Controls**:

- The motion server list should hold **127.0.0.1**, port **26760** (Lichii's
  default). Press **Test**; it should report success while you're streaming.
- For player 1, set **Motion** to the CemuhookUDP / DSU pad.

Lichii's app entry in `apps.toml` must have `gyro_mode = "enabled"` (the
default). Apps set to `"disabled"` get no motion; `"mouse"` turns gyro into
mouse movement instead.

### 4. Tune it from the couch

Open Trireme → **Controllers** → your controller → **Gyro**. The screen shows
live roll, pitch, and yaw while you move the phone or controller. Its settings
apply to the emulator's motion straight away:

| Setting | What it does to emulator motion |
|---|---|
| **Axis ←→** | Which rotation the emulator receives as yaw (turning left and right). |
| **Axis ↑↓** | Which rotation the emulator receives as pitch (tilting up and down). |
| **Invert ←→** | Flips yaw. |
| **Invert ↑↓** | Flips pitch. |

The settings are saved on the host for that controller, so they survive
reconnects and restarts. With the phone as the gyro source, they're stored
under the controller the phone is clamped to.

**Pitch moves the camera sideways?** Swap the two axis choices. On our G8 and
phone setup, that was the one fix needed. **A direction is backwards?** Use
that direction's invert toggle.

The sensitivity and acceleration sliders only shape gyro-as-mouse aiming;
emulators apply their own sensitivity.

## Controllers with their own gyro

- **Phone (any clamp controller):** the most reliable source; the phone's motion
  is corrected for screen rotation before it's sent.
- **Flydigi Vader 5 Pro:** forward it to the host with Trireme's USB export.
  The host reads its gyro directly, with no Flydigi Space Station needed: on
  first contact, Pharos turns on the controller's "allow third-party apps"
  setting itself. It appears as the first free DSU slot, and the gyro screen
  works for it too.
- **Controllers whose gyro Android hides** (common on Android TV, including
  Sony Bravia sets on Android 12) can't send motion from the client. Use the
  phone, or a controller the host can read, as above.

## Troubleshooting

- **Lichii's Test fails:** check that a Trireme client is connected and
  streaming, that **Share client gyro over DSU** is on, and that nothing else is
  using UDP port 26760 (another DSU server such as DS4Windows or BetterJoy).
- **The gyro screen says "Waiting for the host gyro":** with **Phone** as the
  source it should go live as soon as the phone moves. Otherwise, the host isn't
  reading this controller's gyro (see above).
- **Motion drifts or feels mirrored across the board:** the global axis map
  `[dsu] gyro_axes` / `accel_axes` in `config.toml` (default `-y,x,z`) re-maps
  every source at once. Prefer the per-controller settings above.
