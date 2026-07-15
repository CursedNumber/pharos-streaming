# One-line bootstrap for the VM command channel. Typed into the guest's Run
# dialog (Win+R) via QEMU monitor sendkey (see sendkeys.py):
#
#   powershell -ep bypass -w hidden -f //host.lan/Data/cmd/bootstrap.ps1
#
# Runs non-elevated as the auto-logged-in `dev` user. Installs + starts the
# user-role agent (Startup-folder persistence — always allowed non-elevated),
# then spawns the elevated half (install-sys.ps1) for the SYSTEM agent and the
# Developer-Mode registry key. Each stage writes a marker file to the share so
# the Linux side can verify blind typing worked.
$ErrorActionPreference = "Continue"

$share = $null
foreach ($cand in @("\\host.lan\Data", "\\172.30.0.1\Data", "\\10.0.2.4\Data")) {
    if (Test-Path $cand) { $share = $cand; break }
}
if (-not $share) { exit 1 }

New-Item -ItemType Directory -Force -Path C:\xbox | Out-Null
Copy-Item "$share\cmd\agent.ps1" C:\xbox\agent.ps1 -Force
Copy-Item "$share\cmd\install-sys.ps1" C:\xbox\install-sys.ps1 -Force

# User agent: persist via the Startup folder, start it now for this session.
$startup = [Environment]::GetFolderPath("Startup")
@"
@echo off
start "" /min powershell -ExecutionPolicy Bypass -WindowStyle Hidden -File C:\xbox\agent.ps1 -Role user
"@ | Out-File (Join-Path $startup "trireme-agent.cmd") -Encoding ascii
Start-Process powershell -WindowStyle Hidden -ArgumentList `
    "-ExecutionPolicy", "Bypass", "-File", "C:\xbox\agent.ps1", "-Role", "user"
"user agent installed $(Get-Date -Format o)" | Out-File "$share\bootstrap-user.ok" -Encoding ascii

# Elevated half: SYSTEM agent + Developer Mode. May raise a UAC consent
# prompt — the Linux driver answers it blind (Alt+Y) and then checks for
# bootstrap-sys.ok.
Start-Process powershell -Verb RunAs -ArgumentList `
    "-ExecutionPolicy", "Bypass", "-File", "C:\xbox\install-sys.ps1"
