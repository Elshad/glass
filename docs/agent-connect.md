# Agents from anywhere: Glass as a Vercel Connect connector

As built, 2026-10-25. Planned work is listed at the end.

An agent running anywhere (Vercel, claude.ai, ChatGPT, a server) asks to use your computer. You approve it once with one tap, and from then on it works without prompts, except for what is truly risky. Glass follows the MCP authorization spec and what Vercel Connect expects of a provider, so `vercel connect create https://<machine>:8443/mcp` works, as do claude.ai and ChatGPT connectors and MCP Inspector.

## Principles

1. **The host is the authority.** Each Glass host is its own OAuth authorization server and resource server. glassdesk.dev never holds grants, keys or tokens, so a breach there can't hand anyone your Mac. This also keeps Glass account-free.
2. **Ask once, then stay out of the way.** You approve once per agent. After that there are no prompts, only a visible banner, except for actions the agent confirms.
3. **Enforce on the host, at each input event.** Scopes and rules are checked when input is injected, never taken from the model.

## Reach

- **On the tailnet:** `https://<machine>.<tailnet>.ts.net/mcp`. This is the normal Glass listener.
- **From anywhere:** Tailscale Funnel on port 8443 (`glass agents public on`, or the switch in the Mac app).
  - Funnel points at a separate listener on `127.0.0.1:18443` that serves only the OAuth and MCP routes. The viewer, console and files are never published.
  - Everything arriving through Funnel comes from loopback, so it never counts as an owner device.

## Endpoints

| Path | Purpose |
|---|---|
| `/mcp` | MCP over streamable HTTP (JSON responses; GET answers 405). One JSON-RPC message per request: batches get 400. Notifications and client responses get 202. Unknown tools are a JSON-RPC error. Without a token: 401 with `WWW-Authenticate: Bearer resource_metadata="…"`. |
| `/.well-known/oauth-protected-resource/mcp` | RFC 9728 |
| `/.well-known/oauth-authorization-server` | RFC 8414. Advertises `client_id_metadata_document_supported`, revocation, `scopes_supported` and `authorization_details_types_supported`. |
| `/oauth/register`, `/oauth/register/<id>` | DCR (RFC 7591) and management (RFC 7592). The registration token rotates on every read or update. |
| `/oauth/authorize` | The consent page |
| `/oauth/token` | `authorization_code` (PKCE S256), `refresh_token`, `client_credentials` |
| `/oauth/revoke` | RFC 7009. A client can only revoke its own tokens. |

**Protocol details:**

- Client authentication: `none`, `client_secret_basic` or `client_secret_post`, or `private_key_jwt` (ES256 or RS256). Assertions may be addressed to the issuer, the token endpoint or the revocation endpoint. Key sets are fetched again once when a signature doesn't verify, so key rotation works.
- Client ID metadata documents are fetched over https only:
  - public addresses only, with the checked address pinned for the request (curl `--resolve`)
  - 64 KB at most
  - cached for 10 minutes, and only once the document checks out
- Redirect URIs:
  - https, loopback http (any port, compared by parsed parts), or a native app's reverse-DNS scheme
  - matched exactly
  - the consent page names the host the code actually goes to
- Browser clients get CORS on the metadata, registration, token and revocation endpoints. `/mcp` accepts browser pages only from loopback origins, and they still need a token.
- `MCP-Protocol-Version`: `initialize` answers with the client's version if Glass speaks it (2025-03-26, 2025-06-18 or 2025-11-25); otherwise it answers with the latest.
- Tools carry `readOnlyHint` and `destructiveHint` annotations.

## Scopes and limits

| Scope | Allows |
|---|---|
| `computer:view` | Screenshots, `snapshot`, `wait_for_change`, `record`, `list_displays` |
| `computer:use` | Pointer and keyboard. Includes `computer:view`. |

- No scope, or `*`, means both. A call outside the grant gets 403 `insufficient_scope` naming what's missing (MCP step-up).
- An agent can limit itself to some apps with RFC 9396 `authorization_details`: `[{"type": "glass_computer", "apps": ["com.figma.Desktop"]}]`.
  - Anything malformed is refused, never read as "no limit".
  - The granted details come back in every token response.
  - Linux can't tell which app input reaches, so app limits are refused there.

## Tokens

- **Access tokens:** opaque (`gla_…`), 10 minutes, `expires_in` always sent. The host checks its grant table on every request, and again before each input event, so revoking takes effect immediately.
- **Refresh tokens:**
  - **Rotated on every use.** Reusing an older one (a stolen copy) revokes the grant.
  - **Grace for retries:** the one just replaced may come back within 30 seconds (a retry or a lost response). It's refused, and that's all.
  - **Expiry:** 30 days of no use, 180 days at most.
- **Codes:** 60 seconds, single use. A replayed code revokes the grant it made, even mid-exchange.
- **One grant per agent:** approving again replaces the earlier grant.

## Approving

- **On one of your other devices**, via the tailnet address (tailnet identity is the owner's), the page has an **Approve** button. It works a moment after the page is seen and focused, so a stray key or click can't trigger it.
- **Everywhere else** the page shows a number:
  - This covers the computer itself, anything through Funnel, and Linux behind `tailscale serve`.
  - Glass shows four numbers to pick from, in the viewer and in the Mac app (Home and AI agents). The macOS notification and the push say where to answer.
  - The right number approves. A wrong one denies the request, and that agent can't ask again for 10 minutes.
- **What's refused:**
  - an answer from a viewer on the computer itself within 10 seconds of an agent acting
  - any approval while a guest is connected
  - Glass's own app as a target for agent input
- **One pending request per agent**: a new one replaces it, and a request whose page closed is dropped after 45 seconds. Limits are per agent: 3 requests a minute, 20 overall. Owner devices aren't limited.

**Apps (`client_credentials`, Vercel Connect's `app` subject):**

- An app can't get tokens until it's installed on the computer with `glass agents install <client>` or the Mac app's Install button. That works for dynamic registrations and metadata-document clients alike.
- Asking sends one notification, and at most one every 10 minutes.
- Revoking an app's grant uninstalls it.

## Enforcement (each action)

- The grant is live and covers the action, and agents aren't stopped or paused. Stop and Pause survive restarts.
- The app input would reach is checked: what's under the pointer (Accessibility hit test), or what has the keyboard (Spotlight and Control Center included).
  - **Protected apps:**
    - password managers and keychains
    - System Settings and the system's authentication prompts
    - terminals
    - Screen Sharing
    - Glass
  - **Password fields** (secure input) are never typed into.
  - **App-limited agents** reach only their apps, never system UI.
- Screenshots are refused while a protected app, or an app outside the agent's limit, is in front.
- A person using the computer, at its own keyboard and mouse or through a viewer, holds agents for 5 seconds after they stop.
  - An agent from another device waits up to 20 seconds, then is told to try again.
  - Just looking doesn't wait.
- **Hourly budget:** 1,500 actions per agent, one per tool call (a follow-up screenshot is free). Going over notifies the owner once, and Resume gives every agent a fresh hour.
- **Questions to the person:** `confirm` and `request_help` hold an HTTP agent at most 20 s by default (50 s at most), then come back "pending", and `check_answer` collects the answer.
  - Question ids are random.
  - The same question asked again reuses the one already up, and an agent can have 3 open at most.
- **The audit log** gets one line per action: grant, agent, action, detail, the app in front before the action, and the result. Typed text is never recorded.
  - Lines are hash-chained, and every writer reads the chain's end under a lock.
  - It rotates at 8 MB, carrying the chain on.
  - `glass agents verify` checks it.

## Controls

- **Mac app, AI agents:**
  - agents on/off (also in Settings, the console's settings and `glass config agents`; off stops publishing with Funnel too)
  - the MCP address (with copy)
  - publishing with Funnel
  - install requests
  - agents from other devices, with Revoke
  - what agents did lately
- **`glass agents`:** `list`, `revoke ID|all`, `install CLIENT`, `log`, `verify`, `on`, `off`, `public on|off`.
- `glass uninstall` stops publishing.

## Planned

- the agent's own virtual display, so it never shares your screen
- "always allow this for this agent in this app"
- anomaly pauses beyond the hourly budget
- a physical stop key
- DPoP
- `jwt-bearer`
- moving the local `glass mcp` onto grants (it still uses the owner token)
- grant management in the viewer and `/admin/` (for Linux and phones), and a Revoke action on the "agent approved" notification
- blacking out protected windows in screenshots, instead of refusing while they're in front
- the relay route (SNI passthrough at glassdesk.dev) as an alternative to Funnel

## Open questions

- Connect's inbound webhook signing and device-code support aren't documented yet.
- Whether Connect forwards `authorization_details` to custom providers.
