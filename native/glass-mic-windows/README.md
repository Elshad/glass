# Glass Microphone for Windows

A virtual audio driver that brings a viewer's microphone to the computer's apps. It has
two endpoints:

- **Microphone (Glass Microphone)**: what apps record from.
- **Feed (Glass Microphone)**: what Glass plays the viewer's mic into. It's a line
  connector, so Windows doesn't pick it as the default output, and Glass never sends it
  back to viewers as system audio.

Whatever is played into the feed comes out of the microphone (`Source/Utilities/loopback.cpp`:
a ring buffer of at most 200 ms, converting between any 8/16/24/32-bit PCM or 32-bit float
format with integer math). It's the macOS Glass Microphone's counterpart (`native/glass-mic`).

Based on [Virtual-Audio-Driver](https://github.com/VirtualDrivers/Virtual-Audio-Driver)
(MIT, `LICENSE`), itself Microsoft's SysVAD sample (MS-PL, `THIRD_PARTY_NOTICES.md`). The
sample's capture endpoint made silence or a test tone; this one carries real audio.

## Building

CI builds it (`.github/workflows/glass-mic-windows.yml`) for x64 and Arm64 on GitHub's
windows-2022 image, which has the WDK, and signs it with a throwaway test certificate
(`glassmic-test.cer` in the artifact). Locally: Visual Studio 2022 with the WDK, then
`msbuild VirtualAudioDriver.sln /p:Configuration=Release /p:Platform=x64 /p:SignMode=Off`.

## Installing

`glass mic install` installs it when Glass has the package in
`share\glass\drivers\glassmic` (or `GLASS_MIC_DRIVER` points at a folder with
`glassmic.inf`): it adds the `ROOT\GlassMicrophone` device and its driver as
administrator (one UAC prompt), as `devcon install` would. By hand, elevated:
`glass --glass-mic-driver install <path>\glassmic.inf` (and `--glass-mic-driver remove`).

A test-signed build only loads with test signing on and the certificate trusted:

```powershell
bcdedit /set testsigning on    # Secure Boot must be off; then restart
Import-Certificate glassmic-test.cer -CertStoreLocation Cert:\LocalMachine\Root
Import-Certificate glassmic-test.cer -CertStoreLocation Cert:\LocalMachine\TrustedPublisher
```

The Windows VM suite (`tests/e2e/windows-vm`) checks a viewer's mic reaches apps through it.

## Shipping

Windows loads kernel drivers only when Microsoft has signed them. That takes a
[Partner Center hardware account](https://learn.microsoft.com/windows-hardware/drivers/dashboard/hardware-program-register)
(a registered organisation) with an EV code-signing certificate, then attestation signing
of each build (or WHCP/HLK for Windows Update). Until then releases don't include the
driver, and Glass falls back to VB-CABLE (`glass mic install` opens VB-Audio's installer).
