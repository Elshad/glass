# tsfetch

HTTP requests from a browser tab to services on a tailnet, with nothing installed on the visitor's computer.

The tab becomes an ephemeral Tailscale node: Tailscale compiled to WebAssembly, with its own WireGuard and TCP/IP stack. `fetch()` sends the request through that stack and hands back an ordinary `Response`.

```js
import { createTailscaleClient } from "./tsfetch.js";

const tailscale = createTailscaleClient({
  allow: ["100.80.10.20:3000"],
  getAuthKey: () => fetch("/api/tailnet-key", { method: "POST" }).then((r) => r.text()),
});

await tailscale.connect();

const response = await tailscale.fetch("http://100.80.10.20:3000/api/status");
const data = await response.json();

await tailscale.disconnect();
```

```ts
interface TailscaleClient {
  connect(): Promise<void>;
  disconnect(): Promise<void>;
  fetch(url: string | URL, options?: RequestInit): Promise<Response>;
}
```

All options are documented in [tsfetch.d.ts](tsfetch.d.ts).

## Build

```sh
./build.sh
```

Needs Go. It writes `dist/tsfetch.wasm` (about 28 MB, 5 MB with brotli) and `dist/wasm_exec.js`. Serve both beside `tsfetch.js`, or point `wasmURL` and `wasmExecURL` at them. Serve the `.wasm` as `application/wasm` and compressed.

To try it: `python3 -m http.server` in this folder, then open `/example/`.

## What fetch supports

`method`, `headers`, `body` (anything `window.fetch` takes), `signal` and `redirect`. The response body streams.

It is not the browser's fetch, so:

- **No CORS.** The target never sees a browser request, so its CORS policy doesn't come into it, and no `Origin` header is added. Your own page's protections (CSP, XSS hygiene) matter more, not less: script running in your page can reach whatever the node can.
- **No cookies and no cache.** Send credentials as headers.
- **Headers the browser forbids are allowed**, except `Host`, which always follows the URL.
- **`https://` is verified against Let's Encrypt roots only**, which is what `*.ts.net` certificates use. Plain `http://` to a tailnet address is still encrypted end to end by WireGuard.
- A streamed request body is read to the end before sending. HTTP/1.1 only.

## Security

**Auth keys.** Don't ship a key in the page. `getAuthKey` should call your backend, which checks who the visitor is and then mints a key through the Tailscale API (`POST /api/v2/tailnet/-/keys`, with an OAuth client that has the `auth_keys` scope):

```json
{
  "capabilities": { "devices": { "create": {
    "reusable": false, "ephemeral": true, "preauthorized": true, "tags": ["tag:browser"]
  } } },
  "expirySeconds": 120
}
```

A single-use key is spent as soon as the node registers. Or skip keys and pass `onLoginURL` so the person signs in to Tailscale themselves.

**ACLs are the real boundary.** Give `tag:browser` access only to the services browser visitors should reach:

```json
{ "grants": [{ "src": ["tag:browser"], "dst": ["tag:api"], "ip": ["tcp:3000"] }] }
```

**The `allow` list is a second fence.** It is fixed when the client is created and checked inside the WebAssembly module on every connection, redirects included, so a URL that came from user input can't walk the node to something else the ACLs happen to permit. It runs in the visitor's browser, so it guards against mistakes in your page, not against the visitor: they hold the node's keys and can do whatever the ACLs allow.

**Ephemeral.** Node keys live in memory only. `disconnect()` logs the node out, which removes it from the tailnet at once; a tab that is just closed leaves a node that Tailscale removes on its own once it has been offline for a while (half an hour or more).

## Limits

- Browsers can't send UDP, so traffic goes through Tailscale's DERP relays over WebSocket. It never becomes a direct connection. Expect relay latency and throughput: fine for API calls, not for bulk transfer.
- The tailnet must allow DERP, and the page's CSP must allow `wasm-unsafe-eval` plus `connect-src` to `controlplane.tailscale.com` and `*.tailscale.com` (the relays).
- Each connected tab is a node on the tailnet and counts against device limits while it's connected.

## Layout

| | |
|---|---|
| `tsfetch.js`, `tsfetch.d.ts` | The client |
| `wasm/main_js.go` | The Tailscale node and the fetch bridge, derived from Tailscale's `cmd/tsconnect/wasm` (BSD-3-Clause) |
| `wasm/allow.go` | The destination allowlist and its tests |
| `wasm/tagsgen` | Prints the build tags that leave unused Tailscale features out |
| `example/` | A page to try it by hand |
