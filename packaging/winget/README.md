# winget

Manifests for `winget install Glassdesk.Glass`: the all-users MSI, x64 and Arm64. The files
in `manifests/` are templates; for a release, fill them in and submit them by hand.

1. The release workflow runs `node scripts/winget.mjs <version>` and uploads the result as
   the `winget-manifests` artifact (or run it yourself after a release: it reads the
   checksums from `dist/`, else from the GitHub release).
2. On Windows, check it: `winget validate --manifest <dir>`, then
   `winget install --manifest <dir>` in a test VM (`winget settings --enable LocalManifestFiles` first).
3. Open a pull request on microsoft/winget-pkgs adding the folder under
   `manifests/g/Glassdesk/Glass/<version>/` (or use `wingetcreate submit <dir>`).
