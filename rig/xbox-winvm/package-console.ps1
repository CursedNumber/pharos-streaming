# Pack the Phase-1 loose AppX layout (produced by
# client/xboxApp/phase1/build-phase1.ps1 into C:\xbox\AppX) into a signed .appx
# for Xbox Device Portal deployment, and stage it plus the signing cert.
# Run over SSH in the build VM after build-phase1.ps1.
param([string]$AppX = "C:\xbox\AppX", [string]$Out = "C:\xbox", [string]$Stage = "\\host.lan\Data")
$ErrorActionPreference = "Stop"

# Newest SDK tool bin (sort by real version; skip arm64/x86 siblings and stale
# stub versions that lack makeappx).
$ver = Get-ChildItem "C:\Program Files (x86)\Windows Kits\10\bin" -Directory |
    Where-Object { $_.Name -match '^\d+\.\d+\.\d+' -and (Test-Path (Join-Path $_.FullName "x64\makeappx.exe")) } |
    Sort-Object { [version]$_.Name } | Select-Object -Last 1
$sdkbin = Join-Path $ver.FullName "x64"
$makeappx = Join-Path $sdkbin "makeappx.exe"
$signtool = Join-Path $sdkbin "signtool.exe"

$appxPkg = Join-Path $Out "TriremePhase1.appx"
& $makeappx pack /d $AppX /p $appxPkg /o
if ($LASTEXITCODE -ne 0) { throw "makeappx failed ($LASTEXITCODE)" }

# Self-signed cert; subject MUST match the manifest Publisher (CN=TriremeDev).
$cert = New-SelfSignedCertificate -Type CodeSigningCert -Subject "CN=TriremeDev" `
    -CertStoreLocation Cert:\CurrentUser\My -KeyUsage DigitalSignature `
    -TextExtension @("2.5.29.37={text}1.3.6.1.5.5.7.3.3")
$pfx = Join-Path $Out "TriremeDev.pfx"
$cer = Join-Path $Out "TriremeDev.cer"
$pw = ConvertTo-SecureString "trireme" -AsPlainText -Force   # throwaway dev-cert pw
Export-PfxCertificate -Cert $cert -FilePath $pfx -Password $pw | Out-Null
Export-Certificate -Cert $cert -FilePath $cer | Out-Null

& $signtool sign /fd SHA256 /f $pfx /p "trireme" $appxPkg
if ($LASTEXITCODE -ne 0) { throw "signtool failed ($LASTEXITCODE)" }

Copy-Item $appxPkg $Stage -Force
Copy-Item $cer $Stage -Force
Write-Host "staged: TriremePhase1.appx + TriremeDev.cer"
Get-FileHash $appxPkg -Algorithm SHA256 | Format-List
