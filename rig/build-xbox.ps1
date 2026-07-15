# Build the Trireme client core as a static lib for the Xbox UWP target.
#
# RUN ON WINDOWS. Requires:
#   - Visual Studio 2022 with "Universal Windows Platform development" and
#     "Desktop development with C++" workloads (gives MSVC + the Windows SDK).
#   - Rust nightly with the rust-src component (for -Z build-std).
#     rustup toolchain install nightly
#     rustup component add rust-src --toolchain nightly
#
# The `x86_64-uwp-windows-msvc` target is tier 3: no prebuilt std ships for it,
# so we compile std ourselves via build-std. This is the fragile step; pin the
# nightly (see rust-toolchain below) so a std internal change doesn't break you.
#
# Output: target/x86_64-uwp-windows-msvc/release/trireme_xbox.lib
#         (+ regenerated trireme_xbox.h)

$ErrorActionPreference = "Stop"
$Target = "x86_64-uwp-windows-msvc"

# Run from the repo root regardless of where the script was invoked.
$RepoRoot = Split-Path -Parent $PSScriptRoot
Set-Location $RepoRoot

Write-Host "==> Building trireme-xbox-ffi for $Target (build-std)" -ForegroundColor Cyan
cargo +nightly build `
    --release `
    -p trireme-xbox-ffi `
    --target $Target `
    -Z build-std=std,panic_abort `
    -Z build-std-features=

$Lib = "target/$Target/release/trireme_xbox.lib"
if (-not (Test-Path $Lib)) {
    throw "Expected static lib not found at $Lib"
}
Write-Host "==> Static lib: $Lib" -ForegroundColor Green

# Regenerate the C header if cbindgen is installed (cargo install cbindgen).
if (Get-Command cbindgen -ErrorAction SilentlyContinue) {
    Write-Host "==> Regenerating trireme_xbox.h" -ForegroundColor Cyan
    cbindgen --config crates/trireme-xbox-ffi/cbindgen.toml `
        --crate trireme-xbox-ffi `
        --output crates/trireme-xbox-ffi/trireme_xbox.h
    Write-Host "==> Header: crates/trireme-xbox-ffi/trireme_xbox.h" -ForegroundColor Green
} else {
    Write-Host "cbindgen not found; skipping header regen (cargo install cbindgen)" -ForegroundColor Yellow
}
