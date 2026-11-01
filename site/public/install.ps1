# Glass installer for Windows: https://glassdesk.dev
#
#   irm glassdesk.dev/install.ps1 | iex
#
# Installs to %LOCALAPPDATA%\Programs\Glass, puts `glass` on your PATH, then starts
# `glass setup`. No administrator needed. Re-run any time to update.

$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'  # Invoke-WebRequest is many times slower with it

$Base = if ($env:GLASS_BASE) { $env:GLASS_BASE } else { 'https://glassdesk.dev' }
$Prefix = Join-Path $env:LOCALAPPDATA 'Programs\Glass'
$Bin = Join-Path $Prefix 'bin'
$Glass = Join-Path $Bin 'glass.exe'

function Info($text) { Write-Host "  > $text" -ForegroundColor Cyan }
function Fail($text) { Write-Host "  x $text" -ForegroundColor Red; throw $text }

Write-Host ''
Write-Host 'Glass: your computer, in any browser' -ForegroundColor White
Write-Host ''

if ([Environment]::OSVersion.Version.Build -lt 19041) { Fail 'Glass needs Windows 10 version 2004 or newer, or Windows 11.' }
$arch = switch ($env:PROCESSOR_ARCHITECTURE) {
    'AMD64' { 'x86_64' }
    'ARM64' { 'arm64' }
    default { Fail "Glass on Windows supports x64 and Arm64; this computer is $env:PROCESSOR_ARCHITECTURE." }
}
$Archive = "glass-latest-windows-$arch.zip"

# What's there already, so an update can say so (and skip the setup guide). The download
# says it too, so glassdesk.dev counts updates apart from new installs.
$previous = ''
if (Test-Path $Glass) { try { $previous = ((& $Glass --version) -split ' ')[1] } catch {} }

$tmp = Join-Path ([IO.Path]::GetTempPath()) ("glass-" + [Guid]::NewGuid())
New-Item -ItemType Directory -Path $tmp | Out-Null
try {
    Info 'Downloading'
    $zip = Join-Path $tmp 'glass.zip'
    $headers = @{ 'X-Glass-Previous' = $(if ($previous) { $previous } else { 'none' }) }
    try { Invoke-WebRequest -UseBasicParsing -Headers $headers -Uri "$Base/releases/$Archive" -OutFile $zip }
    catch { Fail 'Download failed. Check your connection and try again.' }
    try { $sum = (Invoke-WebRequest -UseBasicParsing -Uri "$Base/releases/$Archive.sha256").Content }
    catch { Fail "Couldn't fetch the checksum." }
    if ($sum -is [byte[]]) { $sum = [Text.Encoding]::ASCII.GetString($sum) }
    $expected = ($sum.Trim() -split '\s+')[0].ToLower()
    $actual = (Get-FileHash -Algorithm SHA256 $zip).Hash.ToLower()
    if ($expected -ne $actual) { Fail 'Checksum mismatch: the download is corrupt or was tampered with.' }

    Info "Installing to $Prefix"
    Expand-Archive -Path $zip -DestinationPath $tmp -Force
    $new = Join-Path $tmp 'glass'

    # A running program can't be overwritten, but it can be renamed: move the old ones
    # aside (Glass may be running, or have started this script) and clean up what earlier
    # updates left. Nothing needs stopping first.
    New-Item -ItemType Directory -Force -Path $Bin | Out-Null
    Get-ChildItem $Bin -Filter '*.old' -ErrorAction SilentlyContinue | Remove-Item -Force -ErrorAction SilentlyContinue
    foreach ($exe in Get-ChildItem (Join-Path $new 'bin')) {
        $dest = Join-Path $Bin $exe.Name
        if (Test-Path $dest) { Move-Item $dest "$dest.$([Guid]::NewGuid().ToString('N')).old" -Force }
        Copy-Item $exe.FullName $dest
    }
    $share = Join-Path $Prefix 'share'
    if (Test-Path $share) { Remove-Item $share -Recurse -Force }
    Copy-Item (Join-Path $new 'share') $share -Recurse
} finally {
    Remove-Item $tmp -Recurse -Force -ErrorAction SilentlyContinue
}

# `glass` on the PATH for new terminals, and for this one.
$userPath = ([Environment]::GetEnvironmentVariable('Path', 'User') + '').TrimEnd(';')
if (-not (($userPath -split ';') -contains $Bin)) {
    $joined = if ($userPath) { "$userPath;$Bin" } else { $Bin }
    [Environment]::SetEnvironmentVariable('Path', $joined, 'User')
    Info 'Added Glass to your PATH (new terminals see it)'
}
if (-not (($env:Path -split ';') -contains $Bin)) { $env:Path = "$env:Path;$Bin" }

$version = ((& $Glass --version) -split ' ')[1]
if (-not $previous) { Info "Installed Glass $version" }
elseif ($previous -eq $version) { Info "Glass $version is the latest; reinstalled it" }
else { Info "Updated Glass $previous -> $version" }

# Restart the background host onto the new version (it reaches the desktop session from
# an SSH shell too).
$installed = Test-Path (Join-Path $env:LOCALAPPDATA 'Glass\agent')
if ($installed) {
    & $Glass restart | Out-Null
    if ($LASTEXITCODE -eq 0) { Info 'Restarted Glass in the background' }
}

# The app, in the Start menu (Glass.app on a Mac).
$app = $false
try { & $Glass app --install | Out-Null; $app = ($LASTEXITCODE -eq 0) } catch {}
if ($app -and -not $previous) { Info 'Added Glass to the Start menu' }

Write-Host ''
if ($previous -and $installed) {
    Write-Host 'Glass is up to date and running. Open Glass from the Start menu for settings.' -ForegroundColor White
    return
}
if ($app -and [Environment]::UserInteractive -and -not $env:SSH_CONNECTION) {
    Start-Process (Join-Path $Bin 'glassw.exe') 'app'
    Write-Host 'Glass is open: follow the steps in its window. (Or in a terminal: glass setup)' -ForegroundColor White
} elseif ([Environment]::UserInteractive -and -not $env:SSH_CONNECTION) {
    & $Glass setup
} else {
    Write-Host 'Next: run  glass setup' -ForegroundColor White
}
