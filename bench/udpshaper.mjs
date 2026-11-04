// A userspace UDP bottleneck, so shaped runs need no sudo. Viewers send to <listen>; it
// forwards to the host at 127.0.0.1:<target>, one upstream socket per viewer address.
//   node bench/udpshaper.mjs <listen> <target> <mbps> <rtt_ms> [loss_pct] [queue_ms]
// Host -> viewer: serialised at <mbps> through a drop-tail queue of <queue_ms> (default
// 200), then <rtt_ms>/2 of delay and <loss_pct> random loss. Viewer -> host: <rtt_ms>/2.
// Node's timers fire to about 1 ms, so it's coarser than dummynet or netem (shape.sh).
// Adapted from voxl (MIT, github.com/tembo/voxl): bench/shaper.py, for UDP instead of TCP.

import { createSocket } from 'node:dgram';

const [listen, target, mbps, rtt, lossPct = '0', queueMs = '200'] = process.argv.slice(2);
if (!target) {
  console.error('usage: udpshaper.mjs <listen> <target> <mbps> <rtt_ms> [loss_pct] [queue_ms]');
  process.exit(2);
}
const bps = Number(mbps) * 1e6;
const delayMs = Number(rtt) / 2;
const loss = Number(lossPct) / 100;
const queueLimitMs = Number(queueMs);
const counts = { down: 0, up: 0, queueDrops: 0, lossDrops: 0 };

/** Packets out in order, each at its time (FIFO: times only grow). */
function line(send) {
  const queue = [];
  let timer;
  const pump = () => {
    timer = undefined;
    const now = performance.now();
    while (queue.length && queue[0].at <= now + 0.5) send(queue.shift().bytes);
    if (queue.length) timer = setTimeout(pump, Math.max(0, queue[0].at - now));
  };
  return (bytes, at) => {
    queue.push({ bytes, at });
    if (!timer) timer = setTimeout(pump, Math.max(0, at - performance.now()));
  };
}

// Dual stack: a browser resolving localhost may use ::1 or 127.0.0.1.
const front = createSocket({ type: 'udp6', ipv6Only: false });
const peers = new Map(); // "address port" -> { back, down }

function peer(address, port) {
  const key = `${address} ${port}`;
  let p = peers.get(key);
  if (p) return p;
  const back = createSocket('udp4');
  const down = line((bytes) => front.send(bytes, port, address));
  const up = line((bytes) => back.send(bytes, Number(target), '127.0.0.1'));
  let free = 0; // when the bottleneck is next idle (ms)
  back.on('message', (bytes) => {
    const now = performance.now();
    const start = Math.max(free, now);
    if (bps && start - now > queueLimitMs) {
      counts.queueDrops++;
      return;
    }
    if (bps) free = start + (bytes.length * 8 * 1000) / bps;
    if (loss && Math.random() < loss) {
      counts.lossDrops++;
      return;
    }
    counts.down++;
    down(bytes, (bps ? free : now) + delayMs);
  });
  back.bind(0, '127.0.0.1');
  p = { back, up };
  peers.set(key, p);
  return p;
}

front.on('message', (bytes, { address, port }) => {
  counts.up++;
  peer(address, port).up(bytes, performance.now() + delayMs);
});
front.on('error', (e) => {
  console.error(`udpshaper: ${e.message}`);
  process.exit(1);
});
front.bind(Number(listen), '::', () => console.log(`udpshaper :${listen} -> 127.0.0.1:${target}, ${mbps} Mbps, ${rtt} ms rtt, ${lossPct}% loss`));

const report = () => console.log(`udpshaper ${JSON.stringify(counts)}`);
process.on('SIGTERM', () => { report(); process.exit(0); });
process.on('SIGINT', () => { report(); process.exit(0); });
