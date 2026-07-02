#!/usr/bin/env bash
# Pharos host launcher — REAP + BUNDLE.
#
# One click does the whole thing:
#   1. Kills every running pharos-{compositor,usbimport,tray,panel} process,
#      no matter how it was started — systemd unit, manual `cargo run`, an
#      old debug build still lingering, anything. (The previous version of
#      this script tried to be "idempotent: skip if active" — that's how we
#      ended up with five generations of pharos-compositor coexisting.)
#   2. Starts a fresh stack from the FRESHEST binary on disk
#      (`target/{release,debug}/<name>` picked by mtime) under transient
#      systemd-run units with Restart=on-failure.
#
# Compositor entry point is fixed: `capture-zc-multi` reads
# ~/.config/pharos/multi.toml. The single-output backend was folded into
# the multi backend, so gamepad/gyro + the panel control plane now run on
# the same code path regardless of how many outputs the layout describes
# (a one-output multi.toml is exactly what the old "single" mode produced).
#
# Knobs:
#   PHAROS_LAUNCH_TRAY=0     — skip the system-tray daemon (headless boxes).
#   PHAROS_LAUNCH_REAP_ONLY=1 — kill + exit, don't restart anything.
set -euo pipefail

REPO="$(cd "$(dirname "$0")/.." && pwd)"
TRAY="${PHAROS_LAUNCH_TRAY:-1}"
REAP_ONLY="${PHAROS_LAUNCH_REAP_ONLY:-0}"
UID_N="$(id -u)"

# Pick the freshest of target/release/<name> vs target/debug/<name>; empty if
# neither exists. `-nt` makes a fresh debug rebuild naturally beat last week's
# release without forcing the user to delete stale binaries.
pick_binary() {
    local name="$1"
    local R="$REPO/target/release/$name"
    local D="$REPO/target/debug/$name"
    if [ -x "$R" ] && [ -x "$D" ]; then
        if [ "$R" -nt "$D" ]; then echo "$R"; else echo "$D"; fi
    elif [ -x "$R" ]; then echo "$R"
    elif [ -x "$D" ]; then echo "$D"
    else echo ""
    fi
}

say() {
    echo "pharos-launch: $*" >&2
    command -v notify-send >/dev/null && notify-send -a "Pharos" "Pharos host" "$*" || true
}

# --- reap ---------------------------------------------------------------
# Kill ANY pharos binary anywhere — across builds (debug/release), across
# launch methods (systemd / direct exec / cargo run), across our four
# component names. We do this BEFORE picking binaries so the freshest pick
# is the only thing left on disk that matters.
#
# Note for the historian: the "orphan-killing daemon" framing was strictly a
# field expedient for the iterate-fast-on-a-host-while-three-old-builds-still-
# hold-the-QUIC-socket phase. Nothing about this is intended as a long-term
# architectural choice — when builds stop turning over every ten minutes, the
# launcher can shrink back to "idempotent start, nudge what's already up".
#
# Pattern is `target/(debug|release)/pharos-X` matched against the full
# cmdline (`pgrep -f`). Two reasons for this anchor:
#   - `pgrep -x pharos-compositor` doesn't work — Linux truncates `comm` to
#     15 chars, so the kernel reports `pharos-composit` and `-x` would miss.
#   - Anchoring the match to `target/<profile>/` keeps an in-progress
#     `cargo build --bin pharos-compositor` from being killed mid-build (its
#     cmdline reads `cargo build --bin pharos-compositor`, no `target/...`).
# NOTE: pharos-tray AND pharos-panel are deliberately NOT reaped — they're
# persistent/user-facing UI processes that must survive a host (re)launch. The
# reap only cycles the host stack itself (compositor + usbimport). The tray is
# started idempotently below; the panel is opened on demand (single-instance).
REAP_PATTERNS=(
    "target/(debug|release)/pharos-compositor"
    "target/(debug|release)/pharos-usbimport"
)

reap_systemd_units() {
    # Stop the host but LEAVE pharos-tray.service alone — the tray is a persistent
    # daemon, not part of the host stack we're cycling.
    systemctl --user stop pharos-host.service 2>/dev/null || true
    # Clear a tripped GPU-fault circuit-breaker (StartLimitBurst) so a re-launch
    # of a `failed` host unit actually starts instead of bouncing.
    systemctl --user reset-failed pharos-host.service pharos-tray.service 2>/dev/null || true
    if sudo -n true 2>/dev/null; then
        sudo -n systemctl stop pharos-usbimport.service 2>/dev/null || true
        # Also clear a failed system unit, else the systemd-run below collides.
        sudo -n systemctl reset-failed pharos-usbimport.service 2>/dev/null || true
    fi
}

reap_processes() {
    # `pgrep | while read` would inherit pgrep's exit-1 (no-match) under
    # `set -o pipefail` and abort the whole launcher. Capture into a
    # variable with `|| true` so the no-match exit is absorbed once,
    # and then iterate as a normal word-split list.
    local sig="$1" pat pids pid
    for pat in "${REAP_PATTERNS[@]}"; do
        pids=$(pgrep -f "$pat" 2>/dev/null || true)
        for pid in $pids; do
            kill -"$sig" "$pid" 2>/dev/null || true
        done
    done
}

count_alive() {
    # Same pipefail trap as reap_processes: `pgrep -f ... | wc -l` would
    # bubble pgrep's exit-1. Use `pgrep -c` (always prints one integer) and
    # `|| echo 0` for the no-match case, captured into a single substitution
    # so arithmetic sees one number.
    local n=0 c pat
    for pat in "${REAP_PATTERNS[@]}"; do
        c=$({ pgrep -cf "$pat" 2>/dev/null || true; } | head -n1)
        n=$((n + ${c:-0}))
    done
    echo "$n"
}

say "reaping any existing pharos-* processes…"
reap_systemd_units
reap_processes TERM
# Grace period for clean shutdown (Wineserver, NVENC release, QUIC close).
for _ in 1 2 3; do
    [ "$(count_alive)" = "0" ] && break
    sleep 1
done
if [ "$(count_alive)" != "0" ]; then
    say "stragglers still up after SIGTERM — SIGKILL"
    reap_processes KILL
    sleep 1
fi

if [ "$REAP_ONLY" = "1" ]; then
    say "PHAROS_LAUNCH_REAP_ONLY=1 — done (stack is down)"
    exit 0
fi

# --- pick binaries (after reap, after any in-progress rebuild has landed) ---
COMPOSITOR=$(pick_binary pharos-compositor)
USBIMPORT=$(pick_binary pharos-usbimport)
TRAY_BIN=$(pick_binary pharos-tray)

# --- start: system-tray daemon — FIRST and IDEMPOTENT --------------------
# Brought up before the compositor/usbimport checks below so the tray appears
# (and stays) regardless of whether the host stack starts — a broken host build
# or a mid-launch failure must never leave the user without their tray icon. An
# already-running tray is left untouched; restart it manually to pick up a
# rebuild (`systemctl --user restart pharos-tray`).
if [ "$TRAY" = "1" ]; then
    if systemctl --user is-active --quiet pharos-tray.service; then
        say "tray already running — leaving it up"
    elif [ -z "$TRAY_BIN" ]; then
        say "pharos-tray not built — skipping (cargo build --release --bin pharos-tray)"
    else
        systemd-run --user --unit=pharos-tray \
            -p Restart=on-failure -p RestartSec=2 \
            -p WorkingDirectory="$REPO" \
            --setenv=DBUS_SESSION_BUS_ADDRESS="unix:path=/run/user/$UID_N/bus" \
            --setenv=XDG_RUNTIME_DIR="/run/user/$UID_N" \
            --setenv=RUST_LOG=info \
            "$TRAY_BIN" >/dev/null
        say "tray started — $(basename "$(dirname "$TRAY_BIN")")/$(basename "$TRAY_BIN")"
    fi
fi

if [ -z "$COMPOSITOR" ]; then
    say "pharos-compositor isn't built. Run: cargo build --release --bin pharos-compositor"
    exit 1
fi
if [ -z "$USBIMPORT" ]; then
    say "pharos-usbimport isn't built. Run: cargo build --bin pharos-usbimport"
    exit 1
fi

# --- start: compositor (user service) ---
# ⚠️ SCREENSHOT-SAFETY INVARIANT: do NOT add `--setenv=WAYLAND_DISPLAY=...` here,
# and never `systemctl --user import-environment WAYLAND_DISPLAY` /
# `dbus-update-activation-environment` with Pharos's socket. The compositor mints
# its own private `wayland-N` socket and hands it to launched clients per-process;
# it must stay out of the session + D-Bus activation environment. If Pharos's
# socket leaks there, xdg-desktop-portal-kde / Spectacle / KWin screenshots bind
# to Pharos (no real outputs) and screen capture goes dark. The compositor logs a
# loud error at startup if it ever sees WAYLAND_DISPLAY pointing at its own socket
# (see crates/compositor/src/lib.rs::init_wayland_listener and rig/WORKAROUNDS.md).
systemd-run --user --unit=pharos-host \
    -p Restart=on-failure -p RestartSec=2 \
    -p Type=notify -p WatchdogSec=10 \
    -p StartLimitIntervalSec=60 -p StartLimitBurst=5 \
    -p WorkingDirectory="$REPO" \
    --setenv=XDG_RUNTIME_DIR="/run/user/$UID_N" \
    --setenv=DBUS_SESSION_BUS_ADDRESS="unix:path=/run/user/$UID_N/bus" \
    --setenv=RUST_LOG=info \
    "$COMPOSITOR" capture-zc-multi >/dev/null
say "compositor started — $(basename "$(dirname "$COMPOSITOR")")/$(basename "$COMPOSITOR")"

# --- start: USB/IP bridge (system service, root via passwordless sudo) ---
if ! sudo -n true 2>/dev/null; then
    say "sudo -n unavailable: skipping pharos-usbimport (run 'sudo $0' to include it)"
else
    sudo -n systemd-run --system --unit=pharos-usbimport \
        -p Restart=on-failure -p RestartSec=2 \
        -p WorkingDirectory="$REPO" \
        --setenv=RUST_LOG=info \
        "$USBIMPORT" >/dev/null
    say "usbimport started — $(basename "$(dirname "$USBIMPORT")")/$(basename "$USBIMPORT")"
fi

# (the system-tray daemon is started earlier now — before the host checks — so a
#  failed host launch never leaves the user without their tray icon)
