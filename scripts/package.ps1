# Builds the Windows release zip into dist/.
#   pwsh scripts/package.ps1 x86_64      (or arm64)
# Layout: glass/bin/glass.exe, glass/bin/glassw.exe (the background launcher),
# glass/share/glass/web/*. With GLASS_PREBUILT set, uses the binaries already built.
param([Parameter(Mandatory)][ValidateSet('x86_64', 'arm64')][string]$Arch)
$ErrorActionPreference = 'Stop'
Set-Location (Join-Path $PSScriptRoot '..')

$target = if ($Arch -eq 'arm64') { 'aarch64-pc-windows-msvc' } else { 'x86_64-pc-windows-msvc' }
$version = (Select-String -Path Cargo.toml -Pattern '^version = "(.*)"' | Select-Object -First 1).Matches.Groups[1].Value
if (-not $env:GLASS_PREBUILT) {
    cargo build --release -p glass-host --target $target
    if ($LASTEXITCODE) { throw 'cargo build failed' }
}
$out = "target/$target/release"

$stage = Join-Path ([IO.Path]::GetTempPath()) ("glass-stage-" + [Guid]::NewGuid())
New-Item -ItemType Directory -Force "$stage/glass/bin", "$stage/glass/share/glass" | Out-Null
Copy-Item "$out/glass-host.exe" "$stage/glass/bin/glass.exe"
Copy-Item "$out/glassw.exe" "$stage/glass/bin/glassw.exe"
Copy-Item web "$stage/glass/share/glass/web" -Recurse

New-Item -ItemType Directory -Force dist | Out-Null
$name = "glass-$version-windows-$Arch.zip"
Remove-Item "dist/$name" -ErrorAction SilentlyContinue
Compress-Archive -Path "$stage/glass" -DestinationPath "dist/$name"
Remove-Item $stage -Recurse -Force
$sum = (Get-FileHash -Algorithm SHA256 "dist/$name").Hash.ToLower()
# Same format as sha256sum, with Unix line endings.
[IO.File]::WriteAllText((Join-Path (Get-Location) "dist/$name.sha256"), "$sum  $name`n")
Copy-Item "dist/$name" "dist/glass-latest-windows-$Arch.zip"
Write-Host "dist/$name ($([math]::Round((Get-Item "dist/$name").Length / 1MB, 1)) MB)"
