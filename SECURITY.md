# Security

## Reporting a vulnerability

Report vulnerabilities privately on GitHub: open the repository's **Security** tab and
choose **Report a vulnerability**
([direct link](https://github.com/Ntarasiuk/glass/security/advisories/new)). Please don't
open a public issue. Say which version (`glass --version`) and platform you found it on,
and how to reproduce it. We'll confirm we got it and keep you posted while we fix it.

## Supported versions

Fixes go into the latest release, so upgrade to get them (`glass update`).

## What Glass protects, and what it leaves to you

Whoever gets into Glass gets the computer's screen and, unless their access is view-only,
its keyboard, mouse, clipboard and files. So:

- **Who gets in:** the owner's own devices (Tailscale says the device is signed in as the
  computer's owner), anyone with the access token, and people with a one-time link,
  single sign-on, LDAP or an access rule, each with the limits it was given. Agents get
  only what the owner approved, and every step is logged.
- **The network:** Glass listens on every address (`0.0.0.0`) unless `glass config bind`
  says otherwise. With a Tailscale HTTPS certificate everything is encrypted; without one,
  the web page is plain HTTP, so keep Glass on your tailnet or behind a TLS proxy.
- **Remote access:** off until you make a link or run `glass remote on`. glassdesk.dev (or
  your own relay) only introduces the two sides; the host signs its half, and the
  picture and input go over an encrypted WebRTC connection between them.
- **Other sites:** owner devices sign in by who they are, so a page on another site,
  open in a browser on one of them, must not be able to act for them. Glass only answers
  for names that can't be pointed at it by someone else (IP addresses, localhost, local
  and tailnet names, this computer's own names, or ones `glass config allowed-hosts`
  adds), which keeps out DNS rebinding. It turns away requests and WebTransport sessions
  from other sites' pages unless `glass config allowed-origins` names them, and JSON APIs
  need a JSON content type, which pages elsewhere can't send without asking first.
- **Secrets:** the token, keys and passwords are stored owner-only, and the token never
  sits in a cookie (cookies carry an HMAC of it).

Out of scope: someone who controls your Tailscale account or one of your signed-in
devices, an administrator of the computer itself, and software already running on it.
