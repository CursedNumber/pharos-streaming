# Trireme VM command agent. Polls the dockur host share for command scripts
# and executes new ones, mirroring their output back to the share — the
# persistent, GUI-free channel into the Windows build VM (the Phase-0 one-shot
# task deleted itself and left the VM unreachable; this replaces it).
#
# Two instances run side by side (see bootstrap.ps1 / install-sys.ps1):
#   -Role sys   SYSTEM, onstart:  builds (rustup lives in the SYSTEM profile
#               from Phase-0 provisioning), registry, anything elevated.
#   -Role user  interactive dev session, onlogon: Add-AppxPackage -Register,
#               UWP app launches, LocalState harvesting.
#
# Protocol, from the Linux side (share root = ~/xbox-winvm/shared):
#   cmd/<role>/NN-name.ps1   — dropped by the host; executed in name order,
#                              exactly once (done-list at C:\xbox\done-<role>.txt)
#   cmd/<role>/NN-name.ps1.out — full transcript of that run (host reads this)
#   agent-<role>.alive       — heartbeat, rewritten every poll (~5s)

param([ValidateSet("sys", "user")][string]$Role = "user")
$ErrorActionPreference = "Continue"

# Single instance per role (a manual /run plus the boot trigger must not race).
$mutex = New-Object System.Threading.Mutex($false, "Global\TriremeAgent-$Role")
if (-not $mutex.WaitOne(0)) { exit 0 }

New-Item -ItemType Directory -Force -Path C:\xbox, C:\xbox\run | Out-Null
$doneFile = "C:\xbox\done-$Role.txt"
if (-not (Test-Path $doneFile)) { New-Item $doneFile -ItemType File | Out-Null }

function Find-Share {
    foreach ($cand in @("\\host.lan\Data", "\\172.30.0.1\Data", "\\10.0.2.4\Data")) {
        if (Test-Path $cand) { return $cand }
    }
    return $null
}

while ($true) {
    $share = Find-Share
    if (-not $share) { Start-Sleep 15; continue }
    $cmdDir = Join-Path $share "cmd\$Role"
    New-Item -ItemType Directory -Force -Path $cmdDir | Out-Null
    "$(Get-Date -Format o) pid=$PID" | Out-File (Join-Path $share "agent-$Role.alive") -Encoding ascii

    $done = @(Get-Content $doneFile -ErrorAction SilentlyContinue)
    $jobs = Get-ChildItem (Join-Path $cmdDir "*.ps1") -ErrorAction SilentlyContinue |
        Where-Object { $done -notcontains $_.Name } | Sort-Object Name
    foreach ($job in $jobs) {
        # Mark done FIRST: a command that hard-crashes the agent (or reboots
        # the VM) must not re-run forever on every restart.
        Add-Content $doneFile $job.Name
        $local = "C:\xbox\run\$($job.Name)"
        Copy-Item $job.FullName $local -Force
        $out = "$($job.FullName).out"
        "=== $($job.Name) start $(Get-Date -Format o) (role=$Role) ===" | Out-File $out -Encoding utf8
        try {
            & powershell -ExecutionPolicy Bypass -NoProfile -File $local *>&1 |
                Out-File $out -Append -Encoding utf8
            "=== exit code: $LASTEXITCODE ===" | Out-File $out -Append -Encoding utf8
        } catch {
            "=== agent-level exception: $_ ===" | Out-File $out -Append -Encoding utf8
        }
        "=== $($job.Name) end $(Get-Date -Format o) ===" | Out-File $out -Append -Encoding utf8
    }
    Start-Sleep 5
}
