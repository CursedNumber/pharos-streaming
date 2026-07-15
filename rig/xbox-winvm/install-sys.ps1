# Elevated half of the VM channel bootstrap (spawned by bootstrap.ps1 via
# RunAs): register the SYSTEM-role agent as an onstart scheduled task and
# enable Developer Mode so loose-file `Add-AppxPackage -Register` works.
$ErrorActionPreference = "Continue"

reg add "HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\AppModelUnlock" `
    /t REG_DWORD /v AllowDevelopmentWithoutDevLicense /d 1 /f

schtasks /create /tn TriremeAgentSys /f /sc onstart /ru SYSTEM /rl HIGHEST `
    /tr "powershell -ExecutionPolicy Bypass -WindowStyle Hidden -File C:\xbox\agent.ps1 -Role sys"
schtasks /run /tn TriremeAgentSys

$share = $null
foreach ($cand in @("\\host.lan\Data", "\\172.30.0.1\Data", "\\10.0.2.4\Data")) {
    if (Test-Path $cand) { $share = $cand; break }
}
if ($share) {
    "sys agent installed $(Get-Date -Format o)" | Out-File "$share\bootstrap-sys.ok" -Encoding ascii
}
