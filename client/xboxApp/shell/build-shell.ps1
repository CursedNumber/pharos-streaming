# Build the Trireme Xbox client shell: the Rust core static lib (UWP triple,
# static CRT) + the C++/WinRT shell into a loose AppX layout. Same toolchain as
# the Phase-1 harness (rig/xbox-winvm/). Links the graphics/media/audio stacks
# the layers need (d3d11/dxgi, d2d1/dwrite, Media Foundation, XAudio2).
#
#   powershell -ep bypass -File build-shell.ps1 -RepoRoot C:\src -OutDir C:\shell
#
# Output: <OutDir>\AppX\ (Trireme.exe + AppxManifest.xml + Assets).
param([string]$RepoRoot = "C:\src", [string]$OutDir = "C:\shell")
$ErrorActionPreference = "Stop"

$vcvars = "C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\VC\Auxiliary\Build\vcvars64.bat"
cmd /c "`"$vcvars`" >nul 2>&1 && set" | ForEach-Object {
    if ($_ -match '^([^=]+)=(.*)$') { Set-Item "env:$($Matches[1])" $Matches[2] }
}

# 1. Rust static lib, static CRT (self-contained for the locked-down console).
$cargo = Join-Path $env:USERPROFILE ".cargo\bin\cargo.exe"
Set-Location $RepoRoot
$env:RUSTFLAGS = "-C target-feature=+crt-static"
& $cargo +nightly build --release -p trireme-xbox-ffi `
    --target x86_64-uwp-windows-msvc -Z build-std=std,panic_abort -Z build-std-features=
if ($LASTEXITCODE -ne 0) { throw "rust build failed ($LASTEXITCODE)" }
$lib = Join-Path $RepoRoot "target\x86_64-uwp-windows-msvc\release\trireme_xbox.lib"

# 2. C++/WinRT projection (reuse if present).
$bld = Join-Path $OutDir "build"; $gen = Join-Path $bld "gen"
New-Item -ItemType Directory -Force -Path $bld | Out-Null
if (-not (Test-Path (Join-Path $gen "winrt\base.h"))) {
    $cppwinrt = Get-ChildItem "C:\Program Files (x86)\Windows Kits\10\bin\10.*\x64\cppwinrt.exe" `
        -ErrorAction SilentlyContinue | Sort-Object FullName | Select-Object -Last 1 -ExpandProperty FullName
    & $cppwinrt -input sdk -output $gen
    if ($LASTEXITCODE -ne 0) { throw "cppwinrt failed ($LASTEXITCODE)" }
}

# 3. Compile every .cpp in shell/ and link into the AppX layout.
$appx = Join-Path $OutDir "AppX"
New-Item -ItemType Directory -Force -Path (Join-Path $appx "Assets") | Out-Null
$srcDir = Join-Path $RepoRoot "client\xboxApp\shell"
$ffiInc = Join-Path $RepoRoot "crates\trireme-xbox-ffi"
$exe = Join-Path $appx "Trireme.exe"
$srcs = Get-ChildItem (Join-Path $srcDir "*.cpp") | ForEach-Object { $_.FullName }

# FFmpeg (vcpkg x64-uwp) for D3D11VA hardware decode, when built. Present ->
# compile video_ffmpeg.cpp (TRIREME_FFMPEG) and link the av* libs; absent ->
# fall back to the Media Foundation software path in video.cpp.
# Prefer the static FFmpeg (self-contained CRT, no VCRUNTIME140_APP.dll needed);
# fall back to the dynamic build; else the Media Foundation path in video.cpp.
$ffRoot = $null; $ffStatic = $false
if (Test-Path "C:\vcpkg\installed\x64-uwp-static\include\libavcodec\avcodec.h") { $ffRoot = "C:\vcpkg\installed\x64-uwp-static"; $ffStatic = $true }
elseif (Test-Path "C:\vcpkg\installed\x64-uwp\include\libavcodec\avcodec.h") { $ffRoot = "C:\vcpkg\installed\x64-uwp" }
$useFF = [bool]$ffRoot

$clArgs = @("/nologo", "/std:c++17", "/permissive-", "/EHsc", "/O2", "/MT", "/bigobj",
            "/DWINAPI_FAMILY=WINAPI_FAMILY_APP", "/DWIN32_LEAN_AND_MEAN", "/DNOMINMAX",
            "/I", $gen, "/I", $ffiInc)
if ($useFF) { Write-Host "==> linking FFmpeg (D3D11VA)"; $clArgs += @("/DTRIREME_FFMPEG", "/I", (Join-Path $ffRoot "include")) }
$clArgs += $srcs
$clArgs += @("/Fo$bld\", "/Fe$exe", "/link", "/SUBSYSTEM:WINDOWS", "/APPCONTAINER", $lib,
             "WindowsApp.lib", "d3d11.lib", "dxgi.lib", "d2d1.lib", "dwrite.lib",
             "mfplat.lib", "mfuuid.lib", "mfreadwrite.lib", "xaudio2.lib",
             "ws2_32.lib", "bcrypt.lib", "ntdll.lib", "userenv.lib", "advapi32.lib",
             "iphlpapi.lib", "synchronization.lib")
if ($useFF) {
    $clArgs += @("/LIBPATH:$ffRoot\lib", "avcodec.lib", "avutil.lib", "avformat.lib", "swscale.lib", "swresample.lib")
    if ($ffStatic) { $clArgs += @("strmiids.lib", "secur32.lib", "ole32.lib", "oleaut32.lib", "user32.lib", "gdi32.lib") }
}

& cl.exe @clArgs
if ($LASTEXITCODE -ne 0) { throw "cl failed ($LASTEXITCODE)" }

Copy-Item (Join-Path $srcDir "AppxManifest.xml") $appx -Force
# Stamp a build-unique package version (0.2.MMdd.HHmm): the console blocks
# reinstalling a same-version package whose contents differ, which stalls the
# deploy loop with the repo's static version.
$now = Get-Date
$ver = "0.2.{0}.{1}" -f [int]$now.ToString("MMdd"), [int]$now.ToString("HHmm")
$mf = Join-Path $appx "AppxManifest.xml"
(Get-Content $mf -Raw) -replace '(<Identity[^>]*Version=")[0-9.]+(")', "`${1}$ver`${2}" |
    Set-Content $mf -Encoding utf8
Write-Host "==> stamped package version $ver"
Copy-Item (Join-Path $srcDir "Assets\*") (Join-Path $appx "Assets") -Force
if ($useFF -and -not $ffStatic) {
    # Dynamic FFmpeg: bundle the av* DLLs. Their VCRUNTIME140_APP.dll comes from
    # the Microsoft.VCLibs.140.00 framework declared in AppxManifest.xml.
    Copy-Item (Join-Path $ffRoot "bin\*.dll") $appx -Force
    Write-Host "==> bundled FFmpeg DLLs"
}
Write-Host "==> shell AppX layout ready: $appx"
