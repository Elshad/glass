# Latency and FPS bench

Catches frame-rate and latency regressions, end to end: a `glass --test` host draws a
workload, Chrome views it through a link that's shaped or not, and the viewer reads a
marker back from every decoded frame. Modelled on [voxl](https://github.com/tembo/voxl)'s
`bench/` (MIT).

Nothing here captures the screen: the host runs in test mode, whose fake displays are a
generated picture (`crates/glass-host/src/testmode.rs`), so it's safe on a Mac that's
running Glass for real.

## How it works

- **Load generator.** `GLASS_TEST_BENCH=anim|scroll` makes the test displays draw a page
  of text-like lines with a 16 × 3 marker of black and white cells in the top-left corner:
  a 16-bit frame counter, the host clock in ms (24 bits) and an 8-bit check.
  - `anim`: a box moving and changing colour over the still page (part of the screen moves).
  - `scroll`: the page scrolling 6 px a frame (all of it moves).
  - `GLASS_TEST_SIZE=WxH` sets the displays' size (default for the bench: 1280×720).
- **Reader.** With `localStorage['glass.bench'] = '1'`, the viewer's worker hands each
  decoded `VideoFrame` of the first stream to `web/bench.js` before closing it. It reads
  the marker, counts frames with a new counter (unique FPS) and takes latency as the host
  clock now (Glass's ping/pong clock sync, the same one the HUD's latency uses) minus the
  marker's time: host draw to viewer draw, encode, network and decode included. Once a
  second it posts a summary the page keeps in `window.glassBench`. Off by default: normal
  viewers never load the probe's work.
- **Driver.** `bench/run.mjs` starts a test host per run, shapes the link, opens the viewer
  in headless Google Chrome (Playwright, from `tests/e2e`), waits for video, warms up and
  then collects the per-second summaries.
- **Shaping.** UDP, since Glass's video goes over QUIC (WebTransport) or WebRTC.
  - `user` (default, no sudo): `bench/udpshaper.mjs`, a userspace bottleneck proxy. The
    driver points the viewer at it by rewriting `config.json`'s `wtPort`. Timers are ~1 ms,
    so it's a little coarser than the kernel.
  - `kernel` (sudo): `bench/shape.sh` shapes the host's UDP port on loopback. macOS: a pf
    anchor (`com.glass.bench`) with dummynet pipes (`dnctl`/`pfctl`). Linux: `tc netem` on
    `lo`, in two extra `prio` bands so other loopback traffic isn't touched.
  - Host to viewer: the rate, half the RTT, the loss and a drop-tail queue (200 ms in
    `user`, 100 packets in `kernel`). Viewer to host: half the RTT.

## Setup

```sh
cargo build --release -p glass-host   # or GLASS_BIN=/path/to/glass-host
(cd tests/e2e && npm ci)              # Playwright; uses your installed Google Chrome
(cd site && npm ci)                   # only for --path rtc (wrangler dev)
```

## Running

```sh
node bench/run.mjs                                   # every workload × link, WebTransport
node bench/run.mjs --workload scroll --link lan,1m150 --secs 30
node bench/run.mjs --shaper kernel                   # dummynet / netem; asks for sudo
node bench/run.mjs --path rtc --link lan             # WebRTC data channel, as a Tesla gets it
node bench/run.mjs --json /tmp/bench.json            # also write the results as JSON
```

Options: `--workload anim,scroll`, `--link lan,10m40,5m60,1m150`, `--path wt|rtc`,
`--shaper user|kernel`, `--secs 20` (measured), `--warmup 5`, `--size 1280x720`,
`--fps 60`, `--json <file>`. `HEADFUL=1` shows the browser.

| Link    | Rate     | RTT    | Loss (host → viewer) |
|---------|----------|--------|----------------------|
| `lan`   | loopback | –      | –                    |
| `10m40` | 10 Mbps  | 40 ms  | 0                    |
| `5m60`  | 5 Mbps   | 60 ms  | 0                    |
| `1m150` | 1 Mbps   | 150 ms | 1% (Tesla-like LTE)  |

Each run prints a row:

- `fps`: unique frames a second, averaged; `worst`: the worst second.
- `p50 ms`, `p99 ms`: marker latency.
- `glass ms`: Glass's own latency (frame capture timestamp to draw), as a cross-check.
- `Mbps`: what the viewer received. `dropped`: frames lost or undecodable.

The JSON also has decoded frames a second (repeats included), marker misreads
(`badMarkers`), the RTT the viewer measured and, with `user`, the shaper's packet counts.

### The WebRTC path

`--path rtc` runs the remote-access path the way `tests/e2e/remote.spec.mjs` does: a local
introducer (`wrangler dev`), `glass link --anywhere`, and video over the WebRTC data
channel (`GLASS_REMOTE_RTP=0`, `glass.rtp` = 0), which is what browsers without WebRTC
video tracks, Teslas among them, get. The test host binds its peer connection to one UDP
port (`GLASS_TEST_RTC_PORT`, test mode only) so `--shaper kernel` can shape just that.
The userspace shaper can't sit in that path (ICE connects straight to the host), so shaped
`rtc` runs need `kernel`. There's no TURN locally: the introducer only hands out TURN
servers with Cloudflare credentials, so the relayed path isn't covered.

### Kernel shaping by hand

```sh
sudo bench/shape.sh up <port> <mbps> <rtt_ms> [loss_pct] [queue_pkts]
sudo bench/shape.sh down
```

macOS: `up` loads `/etc/pf.conf` plus the bench anchor and enables pf with a reference
(`pfctl -E`); `down` flushes the anchor, deletes the two pipes, reloads `/etc/pf.conf` and
gives the reference back. If your `pf.conf` has `set skip on lo0`, loopback isn't shaped.
Linux: `up` replaces `lo`'s root qdisc and `down` deletes it.

## Caveats

- Host and viewer share one machine: the encoder (OpenH264 in test mode, not VideoToolbox
  or NVENC) and Chrome's decoder compete for CPU, and absolute numbers depend on the
  machine. Compare runs on the same machine.
- Over RTP tracks (`<video>` elements) the worker sees no frames, so the probe only covers
  WebTransport and the data channel.
