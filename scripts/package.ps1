# Builds the Windows release into dist/: a zip, and an MSI for managed installs, with
# their checksums.
#   pwsh scripts/package.ps1 x86_64      (or arm64)
# Zip layout: glass/bin/glass.exe, glass/bin/glassw.exe (the background launcher),
# glass/share/glass/web/*. The MSI installs the same into Program Files\Glass
# (packaging/windows/glass.wxs; needs WiX 5: dotnet tool install --global wix --version 5.0.2,
# then wix extension add -g WixToolset.Firewall.wixext/5.0.2 WixToolset.Util.wixext/5.0.2).
# With GLASS_PREBUILT set, uses the binaries already built.
#
# Signing: with GLASS_SIGN_PFX (a .pfx file) and GLASS_SIGN_PFX_PASSWORD set, the programs
# and the MSI are signed (scripts/sign.ps1). The release workflow signs between steps
# instead, so it can use Azure Trusted Signing too:
#   -Step stage    build, and copy the files into target/package/<arch>/glass
#   -Step bundle   the zip and MSI from them (sign the programs before this)
#   -Step sums     checksums and the glass-latest copies (sign the MSI before this)
# Without -Step, all three.
param(
    [Parameter(Mandatory)][ValidateSet('x86_64', 'arm64')][string]$Arch,
    [ValidateSet('all', 'stage', 'bundle', 'sums')][string]$Step = 'all'
)
$ErrorActionPreference = 'Stop'
Set-Location (Join-Path $PSScriptRoot '..')

$target = if ($Arch -eq 'arm64') { 'aarch64-pc-windows-msvc' } else { 'x86_64-pc-windows-msvc' }
$version = (Select-String -Path Cargo.toml -Pattern '^version = "(.*)"' | Select-Object -First 1).Matches.Groups[1].Value
$stage = "target/package/$Arch"
$zip = "glass-$version-windows-$Arch.zip"
$msi = "glass-$version-windows-$Arch.msi"
$sign = $Step -eq 'all' -and $env:GLASS_SIGN_PFX

if ($Step -in 'all', 'stage') {
    if (-not $env:GLASS_PREBUILT) {
        cargo build --release -p glass-host --target $target
        if ($LASTEXITCODE) { throw 'cargo build failed' }
    }
    $out = "target/$target/release"
    if (Test-Path $stage) { Remove-Item $stage -Recurse -Force }
    New-Item -ItemType Directory -Force "$stage/glass/bin", "$stage/glass/share/glass" | Out-Null
    Copy-Item "$out/glass-host.exe" "$stage/glass/bin/glass.exe"
    Copy-Item "$out/glassw.exe" "$stage/glass/bin/glassw.exe"
    Copy-Item web "$stage/glass/share/glass/web" -Recurse
    if ($sign) { & "$PSScriptRoot/sign.ps1" "$stage/glass/bin/glass.exe" "$stage/glass/bin/glassw.exe" }
}

if ($Step -in 'all', 'bundle') {
    New-Item -ItemType Directory -Force dist | Out-Null
    Remove-Item "dist/$zip", "dist/$msi" -ErrorAction SilentlyContinue
    Compress-Archive -Path "$stage/glass" -DestinationPath "dist/$zip"

    # The MSI's icon: the PNG in an .ico, as the per-user shortcut does (src/app/shortcut.rs).
    $png = [IO.File]::ReadAllBytes((Resolve-Path crates/glass-host/assets/icon.png))
    $ico = [byte[]]((0, 0, 1, 0, 1, 0, 0, 0, 0, 0, 1, 0, 32, 0) + [BitConverter]::GetBytes([uint32]$png.Length) + [BitConverter]::GetBytes([uint32]22) + $png)
    $icon = Join-Path (Resolve-Path $stage) 'Glass.ico'
    [IO.File]::WriteAllBytes($icon, $ico)
    $wixArch = if ($Arch -eq 'arm64') { 'arm64' } else { 'x64' }
    # MSI versions are numbers only.
    $msiVersion = ($version -split '-')[0]
    wix build packaging/windows/glass.wxs -arch $wixArch -ext WixToolset.Firewall.wixext -ext WixToolset.Util.wixext `
        -d "Version=$msiVersion" -d "Stage=$((Resolve-Path "$stage/glass").Path)" -d "Icon=$icon" -o "dist/$msi"
    if ($LASTEXITCODE) { throw 'wix build failed' }
    Remove-Item dist/*.wixpdb -ErrorAction SilentlyContinue
    if ($sign) { & "$PSScriptRoot/sign.ps1" "dist/$msi" }
}

if ($Step -in 'all', 'sums') {
    foreach ($name in $zip, $msi) {
        $sum = (Get-FileHash -Algorithm SHA256 "dist/$name").Hash.ToLower()
        # Same format as sha256sum, with Unix line endings.
        [IO.File]::WriteAllText((Join-Path (Get-Location) "dist/$name.sha256"), "$sum  $name`n")
        Copy-Item "dist/$name" ("dist/" + ($name -replace "^glass-$([regex]::Escape($version))-", 'glass-latest-'))
        Write-Host "dist/$name ($([math]::Round((Get-Item "dist/$name").Length / 1MB, 1)) MB)"
    }
}
