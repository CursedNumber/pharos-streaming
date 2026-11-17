# Pharos host workarounds — out-of-tree, environment-specific

This file lists everything outside the Pharos source tree that the Pharos host
relies on to be usable on **this** machine (a KDE Plasma 6 / Wayland session on
CachyOS). Each entry is something that **touches the user's computer** in a way
that is desktop-specific, distro-specific, or per-account — so it doesn't
belong inside the crates but does need to be documented so the workaround
isn't lost when a build is rebuilt, the repo is moved, or another machine is
brought up.

Every entry includes how to **revert** it — none of these workarounds change
system files or require root persistence; they all live under the user's
`~/.local/` or `~/.config/`, or as transient systemd units that vanish on stop.

---

## Spectacle keyboard-shortcut wrapper — ❌ RETIRED 2026-06-23 (was counterproductive)

**Status:** the wrapper indirection was **removed**; the per-user `.desktop`
now just mirrors the system file (`Exec=/usr/bin/spectacle`). The old shim
`Exec=` *broke* screenshots worse than the problem it tried to solve. This is
documented so the failure mode isn't re-introduced. **Never point the user-dir
`org.kde.spectacle.desktop` `Exec=` at a wrapper/shim** — it must be the real
`/usr/bin/spectacle` binary.

**What it tried to do (history):** force Spectacle's own Wayland connection onto
KWin (`wayland-0`) via `~/.local/bin/spectacle-safe`
(`exec env WAYLAND_DISPLAY=wayland-0 /usr/bin/spectacle "$@"`), wired in through
a `sed`-rewritten user override `~/.local/share/applications/org.kde.spectacle.desktop`
whose every `Exec=` called the wrapper. Goal: dodge an
`xdg-desktop-portal-kde` output-enumeration crash when pharos's synthetic
outputs (on `wayland-1`) are present.

**Why it backfired:** KWin's screenshot authorization
(`org.kde.KWin.ScreenShot2`) resolves the caller's systemd app-unit
→ desktop-id `org.kde.spectacle` → that `.desktop` file, then **validates the
running binary against the file's `Exec=` line** and reads
`X-KDE-DBUS-Restricted-Interfaces` from it. The user override pointed `Exec=`
at `…/spectacle-safe`, which does **not** match the real running
`/usr/bin/spectacle` → KWin returns *"The process is not authorized to take a
screenshot"* (`CaptureScreen`, AccessDenied). Same root cause as KDE
[bug 446628](https://bugs.kde.org/show_bug.cgi?id=446628) (the firejail/firecfg
case: any user-dir `org.kde.spectacle.desktop` whose `Exec` is a shim breaks
auth, **even though both restricted-interface keys are present**).

**Why the wrapper was also pointless:** the session `WAYLAND_DISPLAY` is
**already `wayland-0`** (pharos is a *standalone headless* compositor — it
renders offscreen via GBM/EGL on `/dev/dri/renderD128`, never connects to the
host's `wayland-0`, and only *serves* its own `wayland-1`; nothing exports
`wayland-1` into the session). And Spectacle is `DBusActivatable=true`, so the
global-shortcut / menu path D-Bus-activates `/usr/bin/spectacle --dbus` (unit
`app-org.kde.spectacle.service`) — bypassing the wrapper anyway. So forcing
`wayland-0` changed nothing and only diverged the `Exec=` line.

**Verified fix (2026-06-23):** point the user-dir `.desktop` `Exec=` back at the
real `/usr/bin/spectacle` (simplest: `cp` the system file over it). A headless
capture (`spectacle -b -n -f -o …`) launched inside an `app-org.kde.spectacle`
scope then authorizes and writes a full-screen PNG **with pharos running**.
Current state on this machine: `~/.local/share/applications/org.kde.spectacle.desktop`
is byte-identical to the system file (`diff` clean), the orphaned
`~/.local/bin/spectacle-safe` is left inert (harmless), and KSycoca was rebuilt
so kwin_wayland (which *is* the kglobalaccel/shortcut owner on Wayland — the
separate `plasma-kglobalaccel.service` is normally inactive) re-resolves the
service.

⚠️ **Gotcha that bit us:** while iterating, renaming the override to `.bak`
left the shortcut daemon pointing at a now-missing path → *"Unable to make the
service Spectacle executable … No such file or directory"* on the next Print.
Lesson: keep a valid file at that path (mirror the system one) rather than
deleting it out from under the cached service. A user copy is harmless **as long
as its `Exec` is correct** — the earlier denial was the shim, not the file's
existence.

**Full clean-up to pure stock (optional, needs a re-login):** delete the user
copy so it auto-tracks future spectacle updates instead of shadowing them.
```sh
rm -f ~/.local/share/applications/org.kde.spectacle.desktop ~/.local/bin/spectacle-safe
update-desktop-database ~/.local/share/applications/
kbuildsycoca6 --noincremental
# then log out / in so kwin_wayland drops the cached user-path KService
```

**Corrected root-cause (verified 2026-06-23) — the portal CAN'T see Pharos's
outputs.** The old "portal output-enumeration crash from pharos's synthetic
outputs" theory above is mechanically wrong for the current architecture.
Pharos is headless and isolated: its `wl_output`/`xdg_output` globals live
**only** on its private `wayland-1` socket (no EVDI, no DRM connector, no
KScreen registration, no udev hotplug — there is nothing for KWin/KScreen/the
portal to enumerate). Code audit found **no** `WAYLAND_DISPLAY` export and **no**
`dbus-update-activation-environment` / `systemctl --user import-environment`
anywhere; launched clients get the socket via a per-process `Command::env` only.
So the host portal/Spectacle/KWin stay bound to `wayland-0` and screenshots work
with pharos running (re-verified). The likely real history: the dark-screen
predates the headless design (an earlier EVDI/nested experiment that *did*
expose host-visible outputs — see the no-EVDI lesson), and the current design
already fixed it.

**The only way it could regress** is if Pharos's `wayland-1` ever leaks into the
session or D-Bus/systemd activation environment (then the portal binds to Pharos
→ dark screen). Guards now in place: `crates/compositor/src/lib.rs::init_wayland_listener`
logs a loud error if it sees `WAYLAND_DISPLAY` == its own socket, and
`rig/launch-host.sh` carries a "never `--setenv=WAYLAND_DISPLAY`" warning.
**Invariant for all future work: never export Pharos's socket session-wide.**
The planned extend-physical/duplicate-physical feature is the *inbound* mirror
(Pharos connecting to the host `wayland-0` as a client to capture the real
desktop) and is unaffected by this rule — it never advertises Pharos outputs to
the host. If a dark-screen ever does recur, check `WAYLAND_DISPLAY` leakage
first; as a stop-gap, stop pharos-compositor before screenshotting.

---

## Application-menu shortcuts

**Why:** clickable launch from the KDE menu instead of remembering the long
`systemd-run` invocations.

**What was installed:**
- `~/.local/share/applications/pharos-panel.desktop` — runs `rig/launch-panel.sh`,
  which picks the freshest of `target/{release,debug}/pharos-panel` by mtime so
  a fresh `cargo build` is what the next click runs.
- `~/.local/share/applications/pharos-host.desktop` — runs `rig/launch-host.sh`,
  which starts `pharos-host` (user systemd unit, compositor), `pharos-usbimport`
  (system systemd unit, USB/IP import bridge — needs root via passwordless
  sudo), AND `pharos-tray` (user systemd unit, system-tray daemon) with
  `Restart=on-failure`. Idempotent: clicking when already up reports it and
  exits; `PHAROS_LAUNCH_RESTART=1` forces restart so the latest binaries get
  picked up; `PHAROS_LAUNCH_TRAY=0` skips the tray (headless boxes). The
  compositor entry point is fixed at `capture-zc-multi` (the single-output
  backend was folded into multi; gyro/gamepad now work for both single- and
  multi-output layouts via `~/.config/pharos/multi.toml`).
- Both entries use `Icon=<repo>/rig/icons/pharos-app.png` (the
  watercolor lighthouse). Absolute path, not a theme name — so the icon
  survives a `gtk-update-icon-cache` purge and isn't hidden behind hicolor
  installation steps. Tradeoff: not theme-scaled; on hi-DPI panels KDE just
  resizes the 1024² source.

**Revert:**
```sh
rm ~/.local/share/applications/pharos-{panel,host}.desktop
update-desktop-database ~/.local/share/applications/
```

The `rig/launch-{panel,host}.sh` scripts stay in the repo — they're harmless
on their own and useful from a terminal.

---

## System-tray lighthouse daemon (`pharos-tray`)

**Why:** a one-glance status indicator (idle/connected/streaming) plus a
right-click menu (Open Panel / Exit) so the user can stop the host stack
without dropping to a terminal. Lives in the workspace as
`crates/pharos-tray`, builds to `target/{release,debug}/pharos-tray`.

**What touches the desktop:**
- Registers via freedesktop StatusNotifierItem on the **session D-Bus**. No
  files written: KDE Plasma's tray (`plasma-workspace` StatusNotifierWatcher)
  picks up the registration the moment `tray.spawn()` returns. The well-known
  bus name is `org.kde.StatusNotifierItem-<pid>-<n>`; the stable id is
  `pharos-tray` so Plasma remembers panel placement across restarts.
- Icons come from the shared `pharos-assets` crate (`include_bytes!` over the
  five SVGs in `rig/icons/pharos-tray-{offline,idle,streaming,degraded,faulted}.svg`,
  exposed as `pharos_assets::PHAROS_*` and selected by `State::svg()`), rasterized
  at startup via `usvg`+`resvg`+`tiny-skia` to ARGB32 pixmaps at 22 and 44 px.
  Nothing lands in `~/.local/share/icons/hicolor/...`.
- Started by `rig/launch-host.sh` as the transient user unit
  `pharos-tray.service` with `Restart=on-failure`. Clicking the "Exit"
  menu item runs `systemctl --user stop pharos-host.service` and
  `sudo -n systemctl stop pharos-usbimport.service`, then the tray
  process exits cleanly (so its own service goes inactive without
  triggering the restart-on-failure policy).

**State detection (reads the shared `control_ipc::client` cached link):**
The connection to the control-ipc socket
(`control_ipc::socket_path()` → `$XDG_RUNTIME_DIR/pharos/control.sock`) and the
~1 Hz `GetStatus` poll live in the shared `control_ipc::client` thread, not the
tray. The tray just reads that client's cached link + latest `HostStatus` and
maps it (`State::from_status`) to one of five icons:
1. control plane unreachable → *Offline* (or *Faulted* if a durable fault file
   is present).
2. `HostStatus.gpu_health` is `Faulted`/`Degraded` → *Faulted*/*Degraded*.
3. healthy and `streaming` → *Streaming*.
4. healthy otherwise (host up, no client pulling frames) → *Idle*.

There is no *amber* state and no `systemctl is-active` gate any more — a host
that isn't running simply reads as *Offline*. (`systemctl` now appears only in
the "Exit" menu action above.)

**Caveats:**
- Linux-only — SNI is freedesktop session-bus. On non-KDE/Wayland desktops
  without an SNI-capable tray (e.g. plain GNOME) `tray.spawn()` will fail and
  the daemon logs+exits cleanly.
- The "Exit" menu item assumes the same passwordless-sudo posture as the
  launcher script. Without it the USB bridge stays running until manually
  stopped.

**Revert:**
```sh
systemctl --user stop pharos-tray.service
rm -rf target/{release,debug}/pharos-tray
# (the crate stays in the workspace; remove from members in Cargo.toml if you
# want to drop the build entirely.)
```

---

## Passwordless sudo

**Why:** `pharos-usbimport` writes to `/sys/devices/platform/vhci_hcd.0/attach`,
which is root-only. The launcher uses `sudo -n` so a single click can start
the bridge without a GUI password prompt. The host script falls back gracefully
if `sudo -n` is rejected (prints a notification, skips the bridge), so this is
a "nice-to-have," not a hard requirement.

**Where:** a `/etc/sudoers.d/*` entry granting your user NOPASSWD for at least:
- `/usr/bin/systemctl`
- `/usr/bin/systemd-run`

**Revert:** remove the sudoers drop-in. The launcher will then skip the USB
bridge silently and the user can start it manually with
`sudo systemd-run --system --unit=pharos-usbimport ...`.

---

## UFW (firewall)

**Why:** Trireme clients dial the **stream port `:15777`** (the single deliberate
connect port, `transport::PHAROS_STREAM_PORT`) over QUIC (UDP); USB import is
`:4434`; the pairing channel is `:4432`. Without these open, the TV's exporter
never reaches the host. ⚠️ **Moving the stream port without opening it in UFW is a
silent failure** — the client dials it, the packets are dropped, and you get a
client-side `timed out` with *nothing* in the host log (bit us on the 4433→15777
flip, 2026-06-23).

**Where:** current `ufw status verbose` shows on this machine:
- `15777/udp ALLOW` (video stream — the deliberate connect port)
- `4434/udp ALLOW IN 192.0.2.0/24` (USB/IP)
- `4432:4455/udp ALLOW IN 192.0.2.0/24` (pairing + legacy stream range)

Add the stream port: `sudo ufw allow 15777/udp comment "pharos stream"`.

**Revert** (per-rule): `sudo ufw delete allow 15777/udp`

---

## Transient systemd units

**Why:** `Restart=on-failure` survives crashes without writing persistent unit
files. The launcher uses `systemd-run` which creates `--transient` units that
vanish when stopped — no `/etc/systemd/system/` files, no `daemon-reload`, no
state to clean up later.

**Currently in use:**
- `pharos-host.service` (user) — the compositor.
- `pharos-usbimport.service` (system) — the USB/IP import bridge.

**Reboot persistence:** transient units do NOT survive a host reboot — that's
the documented tradeoff. If you want one of these to start on boot, write a
real unit file under `/etc/systemd/system/` and `enable --now` it.

**Revert:** `systemctl --user stop pharos-host` and
`sudo systemctl stop pharos-usbimport`. Nothing else to clean up.

---

## Render group membership (informational)

For SBC clients (Pi5, RK3588) we needed `usermod -aG render <user>` so the
`trireme-kms` zero-copy DRM path could open `/dev/dri/renderD128` over SSH
(the `render` group is what `logind`'s `uaccess` ACL grants on a seated
session — SSH has no seat). On this desktop host we don't need it — the user
already has the render group via desktop session ACLs. Documented here for
completeness; see also the project-android-bravia-client memory note.
