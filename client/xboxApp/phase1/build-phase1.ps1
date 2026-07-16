# Build the Phase-1 UWP gate harness on Windows: the Rust static lib for the
# UWP triple, then main.cpp against it, into a loose AppX layout that
# `Add-AppxPackage -Register` can deploy unsigned (Developer Mode).
#
# Requires VS 2022 Build Tools (VCTools + recommended SDK) and Rust nightly
# with rust-src — exactly the Phase-0 VM provisioning (rig/xbox-winvm/).
# cppwinrt.exe comes from the SDK when present; otherwise it is pulled once
# from the Microsoft.Windows.CppWinRT NuGet package.
#
#   powershell -ep bypass -File build-phase1.ps1 -RepoRoot C:\src -OutDir C:\xbox
#
# Output: <OutDir>\AppX\ (TriremePhase1.exe + AppxManifest.xml + Assets).

param(
    [string]$RepoRoot = "C:\src",
    [string]$OutDir = "C:\xbox"
)
$ErrorActionPreference = "Stop"

# --- VS environment (vcvars64 imported into this process) -------------------
$vcvars = "C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\VC\Auxiliary\Build\vcvars64.bat"
if (-not (Test-Path $vcvars)) { throw "vcvars64.bat not found: $vcvars" }
cmd /c "`"$vcvars`" >nul 2>&1 && set" | ForEach-Object {
    if ($_ -match '^([^=]+)=(.*)$') { Set-Item "env:$($Matches[1])" $Matches[2] }
}
Write-Host "==> MSVC: $(& cl.exe 2>&1 | Select-Object -First 1)"

# --- 1. Rust static lib for x86_64-uwp-windows-msvc (tier 3, build-std) ------
$cargo = Join-Path $env:USERPROFILE ".cargo\bin\cargo.exe"
if (-not (Test-Path $cargo)) { throw "cargo not found at $cargo (rustup nightly + rust-src required)" }
Set-Location $RepoRoot
# Static CRT so the exe is self-contained. The loose /MD build ran in the VM
# (the AppContainer borrowed the desktop CRT from System32), but failed to
# activate on the locked-down console (0x8027025B, no loadable VC runtime).
# +crt-static here, paired with /MT on the cl link below, fixes it.
$env:RUSTFLAGS = "-C target-feature=+crt-static"
& $cargo +nightly build --release -p trireme-xbox-ffi `
    --target x86_64-uwp-windows-msvc -Z build-std=std,panic_abort -Z build-std-features=
if ($LASTEXITCODE -ne 0) { throw "rust UWP build failed ($LASTEXITCODE)" }
$lib = Join-Path $RepoRoot "target\x86_64-uwp-windows-msvc\release\trireme_xbox.lib"
if (-not (Test-Path $lib)) { throw "static lib missing: $lib" }
Write-Host "==> $lib"

# --- 2. C++/WinRT projection headers ----------------------------------------
$bld = Join-Path $OutDir "build"
$gen = Join-Path $bld "gen"
New-Item -ItemType Directory -Force -Path $bld | Out-Null
$cppwinrt = Get-ChildItem "C:\Program Files (x86)\Windows Kits\10\bin\10.*\x64\cppwinrt.exe" `
    -ErrorAction SilentlyContinue | Sort-Object FullName | Select-Object -Last 1 -ExpandProperty FullName
if (-not $cppwinrt) {
    # SDK box without the cppwinrt tool: fetch the NuGet package once.
    $nupkg = Join-Path $bld "cppwinrt.zip"
    if (-not (Test-Path (Join-Path $bld "cppwinrt\bin\cppwinrt.exe"))) {
        Write-Host "==> cppwinrt.exe not in SDK; downloading Microsoft.Windows.CppWinRT from NuGet"
        Invoke-WebRequest "https://www.nuget.org/api/v2/package/Microsoft.Windows.CppWinRT" -OutFile $nupkg
        Expand-Archive $nupkg -DestinationPath (Join-Path $bld "cppwinrt") -Force
    }
    $cppwinrt = Join-Path $bld "cppwinrt\bin\cppwinrt.exe"
}
Write-Host "==> cppwinrt: $cppwinrt"
if (-not (Test-Path (Join-Path $gen "winrt\base.h"))) {
    & $cppwinrt -input sdk -output $gen
    if ($LASTEXITCODE -ne 0) { throw "cppwinrt projection failed ($LASTEXITCODE)" }
}

# --- 3. Compile + link into the loose AppX layout ----------------------------
$appx = Join-Path $OutDir "AppX"
New-Item -ItemType Directory -Force -Path (Join-Path $appx "Assets") | Out-Null
$srcDir = Join-Path $RepoRoot "client\xboxApp\phase1"
$mainCpp = Join-Path $srcDir "main.cpp"
$ffiInc = Join-Path $RepoRoot "crates\trireme-xbox-ffi"
$exe = Join-Path $appx "TriremePhase1.exe"

# /APPCONTAINER marks the exe UWP-activatable; WINAPI_FAMILY_APP keeps us
# honest about the allowed API surface at compile time. /MT (static CRT, paired
# with +crt-static on the Rust lib above) makes the exe self-contained; a /MD
# build runs in the VM but fails activation on the real console (0x8027025B),
# which has no loadable VC runtime for the AppContainer. The trailing libs
# cover the Rust core's imports (sockets, RNG, IOCP, dirs).
& cl.exe /nologo /std:c++17 /permissive- /EHsc /O2 /MT /bigobj `
    /DWINAPI_FAMILY=WINAPI_FAMILY_APP /DWIN32_LEAN_AND_MEAN /DNOMINMAX `
    /I $gen /I $ffiInc `
    $mainCpp `
    /Fo"$bld\" /Fe"$exe" `
    /link /SUBSYSTEM:WINDOWS /APPCONTAINER `
    "$lib" WindowsApp.lib ws2_32.lib bcrypt.lib ntdll.lib userenv.lib `
    advapi32.lib iphlpapi.lib synchronization.lib
if ($LASTEXITCODE -ne 0) { throw "cl failed ($LASTEXITCODE)" }

Copy-Item (Join-Path $srcDir "AppxManifest.xml") $appx -Force
Copy-Item (Join-Path $srcDir "Assets\*") (Join-Path $appx "Assets") -Force
Write-Host "==> AppX layout ready: $appx"
