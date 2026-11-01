# Signing Windows releases with SignPath

SignPath Foundation signs open-source projects' Windows programs for free. The certificate is
theirs, so Windows shows the publisher as **SignPath Foundation**. Terms:
https://signpath.org/terms.

## Where Glass stands

| Requirement (signpath.org/terms) | Status |
|---|---|
| OSI license, no proprietary code | ✅ MIT; the Windows zip and MSI hold only Glass's own programs (VB-CABLE and ViGEmBus are optional, installed by the user) |
| Actively maintained | ✅ |
| **Already released in the form to be signed** | ❌ no Windows release yet: publish an unsigned pre-release first (below) |
| Functionality described on the download page | ✅ release notes + docs "Windows host (preview)" |
| Code signing policy on the home and download pages | 🟡 in the docs and every release's notes; glassdesk.dev only shows it once the site deploys from main |
| Team roles: authors, reviewers, approvers | 🟡 one maintainer in every role; ask SignPath if that's fine (below) |
| MFA on GitHub and SignPath for every team member | ❓ check github.com/settings/security |
| Uninstallation, no unannounced system changes | ✅ Settings › Apps removes everything; setup asks before the firewall rule and the service |
| Privacy policy for data sent elsewhere | ✅ docs "Security and privacy" |
| No hacking tools ("circumvent security measures") | 🟡 remote control is the product; the SYSTEM service is opt-in, admin-installed and documented. Ask (below) |
| Built from source on GitHub-hosted runners, no manual overrides | ✅ release.yml |
| Product name and version set in each program | ✅ crates/glass-host/build.rs |

## 1. Publish an unsigned Windows pre-release

From the `windows-preview` branch (main isn't touched, and a version with a suffix is a
GitHub pre-release that doesn't deploy the site, so installers and updates keep serving the
current release):

1. Set `version = "0.11.0-windows.1"` in Cargo.toml (then `cargo check` to update Cargo.lock)
   and add a `## 0.11.0-windows.1` entry to CHANGELOG.md describing the Windows host.
2. Commit, tag `v0.11.0-windows.1`, push both.

The release notes get the code signing policy and say the Windows builds are unsigned.

Installed computers never see it. Glass doesn't update by itself (someone clicks Update), and
updates come from glassdesk.dev/version.json and glassdesk.dev/releases, not GitHub. The
release doesn't deploy the site, and site/build.mjs refuses to build at a pre-release
version, so a hand-run `npm run deploy` can't publish it either.

## 2. Ask SignPath first

Send to info@signpath.io:

> Hi, I maintain Glass (https://github.com/Ntarasiuk/glass, MIT), a browser-based remote
> desktop for macOS, Linux and now Windows, and plan to apply for SignPath Foundation
> signing for its Windows builds. Two questions before I do:
>
> 1. It's a single-maintainer project: contributions from others are reviewed by me, and I'd
>    approve signing requests. Is that acceptable, or do you need a second approver?
> 2. Glass is remote-control software. It shows a notice and a notification-area icon
>    while anyone is viewing, and only lets in devices on the owner's Tailscale network or with
>    a link the owner made (or with its access token). An optional, administrator-installed Windows service lets the
>    owner use the lock screen and UAC prompts remotely. Is this within your terms?
>
> Code signing policy: https://glassdesk.dev/docs#code-signing-policy

## 3. Apply

https://signpath.org/apply. The form only renders in a browser, so its exact questions
aren't known here. Likely answers:

- **Project:** Glass. https://github.com/Ntarasiuk/glass, https://glassdesk.dev
- **License:** MIT
- **Description:** A free, open-source, low-latency remote desktop. Install the host on a
  Mac, Linux or Windows computer and use it from any browser (H.264 over WebTransport, or
  WebRTC). Access is limited to the owner's Tailscale network or one-time links.
- **What's signed:** `glass.exe`, `glassw.exe` and the MSI, x64 and Arm64, built by GitHub
  Actions (.github/workflows/release.yml).
- **Team:** Nathan Tarasiuk (author, reviewer, approver)
- **Release:** the Windows pre-release from step 1

## 4. Set up SignPath (after acceptance)

1. Turn on MFA in SignPath.
2. Add the **GitHub.com** trusted build system to the organization.
3. Create the project, slug `glass`, repository `https://github.com/Ntarasiuk/glass`, and
   link it to the trusted build system.
4. Artifact configuration: paste [signpath.xml](signpath.xml) as the default.
5. Signing policy, slug `release-signing`: approval required, approver yourself; trusted
   build system verification on; allowed branches `refs/tags/v*` (add `windows-preview`
   while testing).
6. Create a CI user that can submit to that policy and generate its API token.
7. In GitHub (Settings › Secrets and variables › Actions): secret `SIGNPATH_API_TOKEN`
   (the token) and variable `SIGNPATH_ORGANIZATION_ID`. Optional variables:
   `SIGNPATH_PROJECT` and `SIGNPATH_POLICY` if the slugs differ.

The next release then waits for you to approve one signing request per architecture in
SignPath (up to two hours), and fails if one is denied.

Check the first run in SignPath: the paths inside the MSI in signpath.xml
(`**/glass.exe`) are a guess until SignPath has seen a real one.
