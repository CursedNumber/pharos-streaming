# Trireme Xbox build VM provisioning — runs ONCE as SYSTEM via oem-install.bat
# (Phase-0 script, evolved: now also installs the persistent command agents so
# a fresh VM is reachable without the sendkeys.py keystroke bootstrap).
# Logs to C:\build.log, mirrored to the host share.
$ErrorActionPreference = "Continue"
$local = "C:\build.log"
function Log($m) { $t = (Get-Date).ToString("HH:mm:ss"); $line = "$t  $m"; Add-Content $local $line; Write-Host $line }

Log "=== VM provisioning (SYSTEM) starting ==="

# 1. Wait for the dockur host share; early marker = log channel works.
$share = $null
foreach ($cand in @("\\host.lan\Data", "\\172.30.0.1\Data", "\\10.0.2.4\Data")) {
    for ($i = 0; $i -lt 30; $i++) { if (Test-Path $cand) { $share = $cand; break }; Start-Sleep 10 }
    if ($share) { break }
}
$slog = $null
if ($share) {
    "started $(Get-Date -Format o)" | Out-File "$share\setup-started.txt" -Encoding ascii
    $slog = "$share\build.log"
    Log "host share: $share"
} else { Log "WARNING: host share not found; logs local only (C:\build.log)" }
function Mirror() { if ($slog) { Copy-Item $local $slog -Force -ErrorAction SilentlyContinue } }
Mirror

# 2. The command agents (the permanent channel; see agent.ps1 in the repo).
New-Item -ItemType Directory -Force -Path C:\xbox | Out-Null
if ($share -and (Test-Path "$share\cmd\agent.ps1")) {
    Copy-Item "$share\cmd\agent.ps1" C:\xbox\agent.ps1 -Force
    # SYSTEM agent: builds + elevated ops.
    schtasks /create /tn TriremeAgentSys /f /sc onstart /ru SYSTEM /rl HIGHEST `
        /tr "powershell -ExecutionPolicy Bypass -WindowStyle Hidden -File C:\xbox\agent.ps1 -Role sys"
    schtasks /run /tn TriremeAgentSys
    # User agent: UWP deploy/launch in the auto-logged-in dev session. The
    # all-users Startup folder always exists at oem time (the dev profile may
    # not yet), and SYSTEM can write it.
    $startup = "C:\ProgramData\Microsoft\Windows\Start Menu\Programs\StartUp"
    @"
@echo off
start "" /min powershell -ExecutionPolicy Bypass -WindowStyle Hidden -File C:\xbox\agent.ps1 -Role user
"@ | Out-File (Join-Path $startup "trireme-agent.cmd") -Encoding ascii
    Log "agents installed (sys task + user startup hook)"
} else { Log "WARNING: agent.ps1 not on share; channel bootstrap will need sendkeys.py" }

# 3. Developer Mode: loose-file Add-AppxPackage -Register for the UWP harness.
reg add "HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\AppModelUnlock" `
    /t REG_DWORD /v AllowDevelopmentWithoutDevLicense /d 1 /f | Out-Null
Log "developer mode enabled"; Mirror

# 4. VS Build Tools (MSVC + Windows SDK), silent, multi-GB.
$bt = "C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools"
if (-not (Test-Path $bt)) {
    Log "installing VS Build Tools (VCTools + recommended SDK) ..."; Mirror
    $vs = "$env:TEMP\vs_buildtools.exe"
    Invoke-WebRequest "https://aka.ms/vs/17/release/vs_BuildTools.exe" -OutFile $vs
    $p = Start-Process $vs -Wait -PassThru -ArgumentList `
        "--quiet", "--wait", "--norestart", "--nocache", `
        "--add", "Microsoft.VisualStudio.Workload.VCTools", "--includeRecommended"
    Log "VS Build Tools exit: $($p.ExitCode)  (0/3010 = ok)"; Mirror
} else { Log "VS Build Tools already present" }

# 5. Rust nightly + rust-src (build-std for the tier-3 UWP target). Note this
# lands in the SYSTEM profile — builds must run through the sys-role agent.
$cb = "$env:USERPROFILE\.cargo\bin"
if (-not (Test-Path "$cb\rustup.exe")) {
    Log "installing rustup (nightly, minimal) ..."; Mirror
    Invoke-WebRequest "https://win.rustup.rs/x86_64" -OutFile "$env:TEMP\rustup-init.exe"
    & "$env:TEMP\rustup-init.exe" -y --default-toolchain nightly --profile minimal | Out-Null
}
& "$cb\rustup.exe" component add rust-src --toolchain nightly *>&1 | Add-Content $local
& "$cb\rustup.exe" target add x86_64-pc-windows-msvc *>&1 | Add-Content $local
Log "rustc: $(& "$cb\rustc.exe" --version 2>&1)"; Mirror

Log "=== provisioning DONE — drop commands in cmd\sys / cmd\user ==="; Mirror
schtasks /delete /tn "TriremeSetup" /f *>&1 | Out-Null
