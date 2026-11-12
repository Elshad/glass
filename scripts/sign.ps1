# Signs Windows programs and installers (Authenticode, SHA-256, timestamped) with a
# code-signing certificate in a .pfx file, using the Windows SDK's signtool:
#   $env:GLASS_SIGN_PFX = 'C:\keys\glass.pfx'; $env:GLASS_SIGN_PFX_PASSWORD = '…'
#   pwsh scripts/sign.ps1 glass.exe glassw.exe glass.msi
# GLASS_SIGN_TIMESTAMP picks the RFC 3161 timestamp server (default DigiCert's): the
# timestamp keeps signatures valid after the certificate expires.
param([Parameter(Mandatory, ValueFromRemainingArguments)][string[]]$Files)
$ErrorActionPreference = 'Stop'

if (-not $env:GLASS_SIGN_PFX) { throw 'set GLASS_SIGN_PFX (and GLASS_SIGN_PFX_PASSWORD)' }
$timestamp = if ($env:GLASS_SIGN_TIMESTAMP) { $env:GLASS_SIGN_TIMESTAMP } else { 'http://timestamp.digicert.com' }

# The newest Windows SDK's signtool for this computer (GitHub's Windows runners have one).
$cpu = if ($env:PROCESSOR_ARCHITECTURE -eq 'ARM64') { 'arm64' } else { 'x64' }
$signtool = Get-ChildItem "${env:ProgramFiles(x86)}\Windows Kits\10\bin\*\$cpu\signtool.exe" -ErrorAction SilentlyContinue |
    Sort-Object { try { [version]$_.Directory.Parent.Name } catch { [version]'0.0' } } |
    Select-Object -Last 1 -ExpandProperty FullName
if (-not $signtool) { $signtool = (Get-Command signtool.exe -ErrorAction SilentlyContinue).Source }
if (-not $signtool) { throw 'signtool not found: install the Windows SDK' }

& $signtool sign /fd SHA256 /f $env:GLASS_SIGN_PFX /p $env:GLASS_SIGN_PFX_PASSWORD /tr $timestamp /td SHA256 `
    /d Glass /du https://glassdesk.dev $Files
if ($LASTEXITCODE) { throw "signtool failed ($LASTEXITCODE)" }

# Signed and timestamped, whatever the computer thinks of the certificate's issuer.
foreach ($f in $Files) {
    $sig = Get-AuthenticodeSignature $f
    if (-not $sig.SignerCertificate -or -not $sig.TimeStamperCertificate) { throw "$f isn't signed and timestamped" }
    Write-Host "signed $f ($($sig.SignerCertificate.Subject))"
}
