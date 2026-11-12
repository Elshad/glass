# Glass quick support for Windows: https://glassdesk.dev/help
#
#   irm glassdesk.dev/help | iex
#
# Runs Glass for one session from a temporary folder: shows a code to read to the person
# helping, who connects once you allow it. Nothing is installed; it's all deleted when
# you stop.

$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'  # Invoke-WebRequest is many times slower with it

$Base = if ($env:GLASS_BASE) { $env:GLASS_BASE } else { 'https://glassdesk.dev' }

function Info($text) { Write-Host "  > $text" -ForegroundColor Cyan }
function Fail($text) { Write-Host "  x $text" -ForegroundColor Red; throw $text }

Write-Host ''
Write-Host 'Glass quick support' -ForegroundColor White
Write-Host ''

if ([Environment]::OSVersion.Version.Build -lt 19041) { Fail 'Glass needs Windows 10 version 2004 or newer, or Windows 11.' }
$arch = switch ($env:PROCESSOR_ARCHITECTURE) {
    'AMD64' { 'x86_64' }
    'ARM64' { 'arm64' }
    default { Fail "Quick support runs on x64 and Arm64 Windows; this computer is $env:PROCESSOR_ARCHITECTURE." }
}
$Archive = "glass-latest-windows-$arch.zip"

$tmp = Join-Path ([IO.Path]::GetTempPath()) ("glass-help-" + [Guid]::NewGuid())
New-Item -ItemType Directory -Path $tmp | Out-Null
try {
    Info 'Downloading Glass for this session (nothing is installed)'
    $zip = Join-Path $tmp 'glass.zip'
    try { Invoke-WebRequest -UseBasicParsing -Uri "$Base/releases/$Archive" -OutFile $zip }
    catch { Fail 'Download failed. Check your connection and try again.' }
    try { $sum = (Invoke-WebRequest -UseBasicParsing -Uri "$Base/releases/$Archive.sha256").Content }
    catch { Fail "Couldn't fetch the checksum." }
    if ($sum -is [byte[]]) { $sum = [Text.Encoding]::ASCII.GetString($sum) }
    $expected = ($sum.Trim() -split '\s+')[0].ToLower()
    $actual = (Get-FileHash -Algorithm SHA256 $zip).Hash.ToLower()
    if ($expected -ne $actual) { Fail 'Checksum mismatch: the download is corrupt or was tampered with.' }
    Expand-Archive -Path $zip -DestinationPath $tmp -Force

    $env:GLASS_BASE = $Base
    & (Join-Path $tmp 'glass\bin\glass.exe') support
} finally {
    # Glass may take a moment to let go of its files after the session ends.
    for ($i = 0; $i -lt 20 -and (Test-Path $tmp); $i++) {
        Remove-Item $tmp -Recurse -Force -ErrorAction SilentlyContinue
        if (Test-Path $tmp) { Start-Sleep -Milliseconds 500 }
    }
}
Write-Host ''
Info 'Done. Glass is gone from this computer.'
