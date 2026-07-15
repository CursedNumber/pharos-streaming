# xbox-winvm — headless Windows build VM for the Xbox UWP client

A `dockurr/windows` QEMU/KVM guest on devbox that builds and runs the
Windows-only parts of the Xbox client work: the tier-3
`x86_64-uwp-windows-msvc` Rust build, the C++/WinRT harness/shell, and UWP
(AppContainer) runtime tests. Validated for Phase 0 (2026-07-15: build + C ABI
round-trip) and built out for Phase 1 (pair + QUIC stream inside the sandbox).

## Layout on the host

```
~/xbox-winvm/
  storage/   VM disk (btrfs: chattr +C BEFORE first boot)
  shared/    → \\host.lan\Data in the guest — the file channel
  oem/       install.bat + setup.ps1, run ONCE at Windows-install time
```

`run-vm.sh` (re)creates the container; a fresh install provisions itself via
`oem-setup.ps1`: VS Build Tools (VCTools + recommended SDK), Rust nightly +
rust-src (in the **SYSTEM** profile), Developer Mode, and the command agents.

## The command channel (agent.ps1)

noVNC screenshots time out and the guest has no exec/SSH — all driving happens
over the share:

- Drop `NN-name.ps1` into `shared/cmd/sys/` (SYSTEM: builds, registry) or
  `shared/cmd/user/` (interactive dev session: `Add-AppxPackage -Register`,
  UWP launches, LocalState harvesting).
- The matching agent executes new files in name order, exactly once, and
  writes the full transcript to `NN-name.ps1.out` beside it.
- Liveness: `shared/agent-{sys,user}.alive` heartbeats (~5 s).

On a VM that predates the agents (or if they die), bootstrap by typing into
the guest with `sendkeys.py` (QEMU monitor `sendkey`; root):

```sh
sudo ./sendkeys.py --combo meta_l-r          # Win+R
sudo ./sendkeys.py --delay 1 --type 'powershell -ep bypass -w hidden -f //host.lan/Data/cmd/bootstrap.ps1'
sudo ./sendkeys.py --combo ret
# UAC consent for the elevated half, if prompted:
sudo ./sendkeys.py --delay 3 --combo alt-y
```

`bootstrap.ps1` installs the user agent (Startup folder) + spawns
`install-sys.ps1` (SYSTEM agent + Developer Mode), each writing
`bootstrap-{user,sys}.ok` to the share as blind-typing verification. Only
layout-stable characters (letters digits space dash dot slash) go through
sendkeys — the guest layout is assumed en-US.

## Phase-1 gate run (what the pieces are for)

1. rsync the repo to `shared/trireme-src/`.
2. `cmd/sys`: sync source to `C:\src`, run
   `client/xboxApp/phase1/build-phase1.ps1` → `C:\xbox\AppX` (Rust UWP lib +
   harness exe + manifest).
3. Read the live pairing PIN from the Pharos control socket
   (`HostStatus.pairing.pin` — it rotates after every handshake).
4. `cmd/user`: `Add-AppxPackage -Register C:\xbox\AppX\AppxManifest.xml`,
   write `phase1.txt` (host + PIN) into the package's `LocalState`, launch
   `shell:AppsFolder\<PFN>!App`, wait, copy `LocalState\phase1.log` back to
   the share.
5. `phase1.log` ends in `PHASE1 VERDICT: PASS/FAIL — …`.

Guest credentials: `dev` / `trireme-dev` (RDP on 127.0.0.1:3389, noVNC on
127.0.0.1:8006 — view only).
