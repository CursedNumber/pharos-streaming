#!/usr/bin/env bash
# Create (or recreate) the Windows 11 build VM for the Xbox UWP client work —
# the exact recipe validated for Phase 0/1 (captured 2026-07-16 from the live
# container). dockurr/windows runs a real QEMU/KVM Windows guest inside a
# container; we use it because the UWP-target Rust build, MSVC, and UWP app
# deployment are Windows-only, and no physical Windows box is on the LAN.
#
# Channels into the headless guest (noVNC screenshots time out; don't GUI-drive):
#   - /oem      → runs install.bat ONCE at Windows-install time (SYSTEM)
#   - shared/   → \\host.lan\Data inside the guest; agent.ps1 polls
#                 shared/cmd/{sys,user}/*.ps1 and mirrors output (see README)
#   - sendkeys.py → QEMU monitor keystrokes, for the one-time agent bootstrap
#
# ⚠️ btrfs: `chattr +C` the storage dir BEFORE first boot or Windows Setup
#    corrupts itself on CoW.
# ⚠️ Ports are bound to 127.0.0.1 only (8006 noVNC, 3389 RDP).
set -euo pipefail

VMDIR="${XBOX_VMDIR:-$HOME/xbox-winvm}"
ISO="${XBOX_ISO:-$HOME/Downloads/Win11_25H2_EnglishInternational_x64_v2.iso}"
NAME="xbox-win"

mkdir -p "$VMDIR/storage" "$VMDIR/shared" "$VMDIR/oem"
chattr +C "$VMDIR/storage" 2>/dev/null || true

# The oem hook + agent come from the repo (rig/xbox-winvm/).
HERE="$(cd "$(dirname "$0")" && pwd)"

# Guest password: $XBOX_VM_PASSWORD, else a random one generated on first run
# and kept in $VMDIR/password (mode 600). Only a fresh install applies it.
PWFILE="$VMDIR/password"
if [[ -z "${XBOX_VM_PASSWORD:-}" ]]; then
    if [[ ! -s "$PWFILE" ]]; then
        mkdir -p "$VMDIR"
        (umask 077; head -c 18 /dev/urandom | base64 | tr -d '/+=' > "$PWFILE")
        echo "Generated a guest password in $PWFILE"
    fi
    XBOX_VM_PASSWORD="$(cat "$PWFILE")"
fi
cp "$HERE/oem-install.bat" "$VMDIR/oem/install.bat"
cp "$HERE/oem-setup.ps1" "$VMDIR/oem/setup.ps1"
mkdir -p "$VMDIR/shared/cmd/sys" "$VMDIR/shared/cmd/user"
cp "$HERE/agent.ps1" "$HERE/bootstrap.ps1" "$HERE/install-sys.ps1" "$VMDIR/shared/cmd/"

sudo systemctl start docker.socket docker.service
sudo docker rm -f "$NAME" 2>/dev/null || true
sudo docker run -d --name "$NAME" \
    --device=/dev/kvm --device=/dev/net/tun --cap-add NET_ADMIN \
    -p 127.0.0.1:8006:8006 \
    -p 127.0.0.1:3389:3389/tcp -p 127.0.0.1:3389:3389/udp \
    -v "$VMDIR/storage:/storage" \
    -v "$VMDIR/shared:/data" \
    -v "$VMDIR/oem:/oem" \
    -v "$ISO:/custom.iso" \
    -e VERSION=11 -e USERNAME=dev -e PASSWORD="$XBOX_VM_PASSWORD" \
    -e CPU_CORES=8 -e RAM_SIZE=10G -e DISK_SIZE=128G \
    dockurr/windows:latest

echo "VM starting. Install progress: http://127.0.0.1:8006 (noVNC, view only)."
echo "A fresh install takes ~20-40 min + VS Build Tools provisioning (oem-setup)."
echo "Channel liveness: watch $VMDIR/shared/agent-*.alive"
