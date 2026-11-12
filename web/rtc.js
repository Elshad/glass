// Remote access over WebRTC (glassdesk.dev/app/#r=<host key>). RTCPeerConnection exists
// only on the main thread, so the connection lives here and is relayed to the worker over
// a MessagePort; the worker sees a WebTransport-shaped object (rtc-transport.js).
//
// The introducer (glassdesk.dev) only passes the offer and answer. The host signs its
// answer together with our offer, and we check that against the key in the link, so a
// compromised introducer can't put anyone else in the middle: DTLS then pins the host's
// certificate fingerprint from that signed answer.

const STUN = 'stun:stun.cloudflare.com:3478';
const CHUNK = 16 * 1024;
const BUFFER_HIGH = 1024 * 1024;
const BUFFER_LOW = 256 * 1024;
const GATHER_TIMEOUT_MS = 3000;
const CONNECT_TIMEOUT_MS = 20_000;
// With a WebSocket to fall back to, a direct path that hasn't formed by now isn't coming.
const CONNECT_TIMEOUT_WITH_FALLBACK_MS = 8000;
const WS_OPEN_TIMEOUT_MS = 10_000;
// Acks often: everything sent and not yet acked counts against the host's video queue.
const WS_ACK_EVERY = 16 * 1024;
const WS_ACK_MS = 20;
// The bridge's message kinds (crates/glass-host/src/wsbridge.rs).
const WS_DGRAM = 0, WS_OPEN = 1, WS_DATA = 2, WS_END = 3, WS_ACK = 4;
// This tab went through the WebSocket before: its network has no direct path, so retries
// go straight there.
const WS_PREF_KEY = 'glass.ws';

export function toB64url(bytes) {
  let s = '';
  for (const b of bytes) s += String.fromCharCode(b);
  return btoa(s).replace(/\+/g, '-').replace(/\//g, '_').replace(/=+$/, '');
}

export function fromB64url(text) {
  return Uint8Array.from(atob(text.replace(/-/g, '+').replace(/_/g, '/')), (c) => c.charCodeAt(0));
}

/** The introducer's id for a host: the first 16 bytes of SHA-256 of its public key. */
export async function hostIdFor(hostKey) {
  const digest = new Uint8Array(await crypto.subtle.digest('SHA-256', fromB64url(hostKey)));
  return toB64url(digest.slice(0, 16));
}

/** Displays a connection can carry as video tracks (the host's MAX_STREAMS). */
const VIDEO_TRACKS = 4;

/**
 * Whether to ask for video as RTP tracks: needs H.264 (packetization-mode 1) on the
 * receiving side, and can be switched off (`glass.rtp` = 0) if tracks failed before.
 */
function wantRtpVideo() {
  try { if (sessionStorage.getItem('glass.rtp') === '0') return false; } catch {}
  const codecs = RTCRtpReceiver.getCapabilities?.('video')?.codecs ?? [];
  return codecs.some((c) => /h264/i.test(c.mimeType) && /packetization-mode=1/.test(c.sdpFmtpLine ?? ''));
}

/** H.264 first, High profile before Baseline (the host sends High), then its RTX. */
function codecPreferences() {
  const codecs = RTCRtpReceiver.getCapabilities('video').codecs;
  const profile = (c) => (/profile-level-id=(\w{4})/i.exec(c.sdpFmtpLine ?? '')?.[1] ?? '').toLowerCase();
  const rank = (c) => ['6400', '640c', '4d00', '42e0', '4200'].indexOf(profile(c)) >>> 0;
  const h264 = codecs
    .filter((c) => /h264/i.test(c.mimeType) && /packetization-mode=1/.test(c.sdpFmtpLine ?? ''))
    .sort((a, b) => rank(a) - rank(b));
  const rtx = codecs.filter((c) => /rtx/i.test(c.mimeType));
  return [...h264, ...rtx];
}

/**
 * Serves the worker's RtcTransport over `port`, connecting on request. Video arrives as
 * RTP tracks when both sides support it: `onVideo(stream, track)` for each, and
 * `onMode(rtp)` once the answer is in (false: video comes over the data channel).
 */
export function serveRtc({ port, hostKey, signalUrl, wsUrl, log, onVideo = () => {}, onMode = () => {} }) {
  // Each connection attempt has a generation (from the worker); everything posted carries
  // it, so a late message from an old connection can't reach a new one.
  let session; // { gen, pc, channels: Map<id, channel>, senders, dgram }
  const api = {};
  let nextIncoming = -1;
  let latestGen;

  const post = (gen, msg, transfer) => port.postMessage({ ...msg, gen }, transfer ?? []);

  /** `notify`: tell the worker (only when the connection dropped by itself). */
  function teardown(reason, notify) {
    if (!session) return;
    const s = session;
    session = undefined;
    for (const ch of s.channels.values()) try { ch.close(); } catch {}
    try { s.pc?.close(); } catch {}
    if (s.ws) {
      clearInterval(s.ackTimer);
      try { s.ws.close(); } catch {}
    }
    if (notify) post(s.gen, { t: 'closed', reason });
  }

  // ---- The WebSocket fallback: the same session through the tunnel that serves this page ----

  function wsSend(s, kind, id, body) {
    const head = id === undefined ? 1 : 5;
    const buf = new Uint8Array(head + (body?.byteLength ?? 0));
    buf[0] = kind;
    if (id !== undefined) new DataView(buf.buffer).setInt32(1, id, true);
    if (body) buf.set(body, head);
    s.ws.send(buf);
  }

  /** Tells the host how much has arrived, so it knows what's still on the way. */
  function wsAck(s) {
    if (s.received === s.acked || s.ws.readyState !== WebSocket.OPEN) return;
    const buf = new Uint8Array(9);
    buf[0] = WS_ACK;
    new DataView(buf.buffer).setBigUint64(1, BigInt(s.received), true);
    s.ws.send(buf);
    s.acked = s.received;
  }

  function onWsMessage(s, buf) {
    s.received += buf.byteLength;
    if (s.received - s.acked >= WS_ACK_EVERY) wsAck(s);
    const view = new DataView(buf);
    const kind = view.getUint8(0);
    if (kind === WS_DGRAM) {
      const data = buf.slice(1);
      post(s.gen, { t: 'dgram', data }, [data]);
      return;
    }
    if (buf.byteLength < 5) return;
    const id = view.getInt32(1, true);
    if (kind === WS_OPEN) post(s.gen, { t: 'incoming', id });
    else if (kind === WS_DATA) {
      const data = buf.slice(5);
      post(s.gen, { t: 'data', id, data }, [data]);
    } else if (kind === WS_END) post(s.gen, { t: 'end', id });
  }

  /** Sends a stream's bytes as the socket drains, acking them back to the worker. */
  function wsSender(s, id) {
    const queue = [];
    let unacked = 0;
    let timer;
    const flush = () => {
      timer = undefined;
      if (s.ws.readyState !== WebSocket.OPEN) return;
      while (queue.length && s.ws.bufferedAmount < BUFFER_HIGH) {
        const item = queue.shift();
        if (item === 'end') wsSend(s, WS_END, id);
        else wsSend(s, WS_DATA, id, item);
      }
      if (s.ws.bufferedAmount < BUFFER_HIGH && unacked) {
        post(s.gen, { t: 'ack', id, bytes: unacked });
        unacked = 0;
      }
      // WebSocket has no "drained" event: look again shortly.
      if ((queue.length || unacked) && !timer) timer = setTimeout(flush, 10);
    };
    return {
      data(bytes) {
        for (let i = 0; i < bytes.byteLength; i += CHUNK) queue.push(bytes.subarray(i, i + CHUNK));
        unacked += bytes.byteLength;
        flush();
      },
      end() { queue.push('end'); flush(); },
    };
  }

  function connectWs(gen) {
    return new Promise((resolve, reject) => {
      const ws = new WebSocket(wsUrl);
      ws.binaryType = 'arraybuffer';
      const s = { gen, ws, channels: new Map(), senders: new Map(), received: 0, acked: 0 };
      const fail = () => reject(new Error("Couldn't reach this computer through the web either."));
      const timer = setTimeout(() => { try { ws.close(); } catch {} fail(); }, WS_OPEN_TIMEOUT_MS);
      ws.onopen = () => {
        clearTimeout(timer);
        // A newer attempt started meanwhile: this one is stale.
        if (gen !== latestGen) { try { ws.close(); } catch {} fail(); return; }
        session = s;
        s.ackTimer = setInterval(() => wsAck(s), WS_ACK_MS);
        resolve();
      };
      ws.onmessage = ({ data }) => { if (data instanceof ArrayBuffer && data.byteLength) onWsMessage(s, data); };
      ws.onclose = () => {
        clearTimeout(timer);
        if (session === s) teardown('connection closed', true);
        else fail();
      };
    });
  }

  async function connectViaWs(gen, t0) {
    teardown('reconnecting', false);
    await connectWs(gen);
    try { sessionStorage.setItem(WS_PREF_KEY, '1'); } catch {}
    onMode(false);
    log('info', 'remote: connected', { via: 'websocket', connectMs: Math.round(performance.now() - t0) });
    post(gen, { t: 'open' });
  }

  function preferWs() {
    try { return Boolean(wsUrl) && sessionStorage.getItem(WS_PREF_KEY) === '1'; } catch { return false; }
  }

  function wireChannel(gen, id, ch) {
    ch.binaryType = 'arraybuffer';
    ch.bufferedAmountLowThreshold = BUFFER_LOW;
    ch.onmessage = ({ data }) => {
      if (typeof data === 'string') {
        if (data === 'end') post(gen, { t: 'end', id });
        return;
      }
      post(gen, { t: 'data', id, data }, [data]);
    };
    ch.onclose = () => post(gen, { t: 'end', id });
  }

  /** Sends on a channel once it's open, acking bytes back to the worker as they drain. */
  function sender(gen, id, ch) {
    const queue = [];
    let unacked = 0;
    const flush = () => {
      while (queue.length && ch.readyState === 'open' && ch.bufferedAmount < BUFFER_HIGH) {
        const item = queue.shift();
        if (item === 'end') ch.send('end');
        else ch.send(item);
      }
      if (ch.bufferedAmount < BUFFER_HIGH && unacked) {
        post(gen, { t: 'ack', id, bytes: unacked });
        unacked = 0;
      }
    };
    ch.addEventListener('open', flush);
    ch.addEventListener('bufferedamountlow', flush);
    return {
      data(bytes) {
        for (let i = 0; i < bytes.byteLength; i += CHUNK) queue.push(bytes.subarray(i, i + CHUNK));
        unacked += bytes.byteLength;
        flush();
      },
      end() { queue.push('end'); flush(); },
    };
  }

  async function connect(gen) {
    const tStart = performance.now();
    if (preferWs()) {
      try { await connectViaWs(gen, tStart); return; } catch {}
    }
    teardown('reconnecting', false);
    const ws = new WebSocket(signalUrl);
    const inbox = [];
    let wake;
    ws.onmessage = ({ data }) => { inbox.push(JSON.parse(data)); wake?.(); };
    const closed = new Promise((resolve) => { ws.onclose = ws.onerror = () => { resolve(); wake?.(); }; });
    const next = async (timeoutMs) => {
      const deadline = Date.now() + timeoutMs;
      while (!inbox.length) {
        if (ws.readyState >= WebSocket.CLOSING) throw new Error("couldn't reach glassdesk.dev");
        if (Date.now() > deadline) throw new Error('the computer didn’t answer');
        await Promise.race([new Promise((r) => { wake = r; }), closed, sleep(deadline - Date.now())]);
      }
      return inbox.shift();
    };
    try {
      const hello = await next(10_000);
      if (hello.t !== 'hello') throw new Error('unexpected reply from glassdesk.dev');
      if (!hello.online) throw fatal('This computer is offline, or remote access is turned off on it.');
      const pc = new RTCPeerConnection({ iceServers: [{ urls: STUN }, ...(hello.iceServers ?? [])] });
      session = { gen, pc, channels: new Map(), senders: new Map() };
      const s = session;
      const dgram = pc.createDataChannel('d', { ordered: false, maxRetransmits: 0 });
      dgram.binaryType = 'arraybuffer';
      dgram.onmessage = ({ data }) => { if (typeof data !== 'string') post(gen, { t: 'dgram', data }, [data]); };
      s.dgram = dgram;
      // Video as RTP tracks: one recvonly transceiver per possible display, in stream order.
      const transceivers = [];
      if (wantRtpVideo()) {
        const prefs = codecPreferences();
        for (let i = 0; i < VIDEO_TRACKS; i++) {
          const t = pc.addTransceiver('video', { direction: 'recvonly' });
          try { t.setCodecPreferences(prefs); } catch {}
          transceivers.push(t);
        }
      }
      pc.ontrack = (e) => {
        const i = transceivers.indexOf(e.transceiver);
        // A video m-line the host left unbound fires without a stream: ignore it.
        if (i < 0 || !e.streams.length) return;
        // Show frames as they arrive: a remote desktop wants latency, not smoothing.
        try {
          if ('jitterBufferTarget' in e.receiver) e.receiver.jitterBufferTarget = 0;
          else e.receiver.playoutDelayHint = 0;
        } catch {}
        onVideo(i, e.track);
      };
      pc.ondatachannel = ({ channel }) => {
        const id = nextIncoming--;
        s.channels.set(id, channel);
        wireChannel(gen, id, channel);
        post(gen, { t: 'incoming', id });
      };
      pc.onconnectionstatechange = () => {
        if (['failed', 'closed'].includes(pc.connectionState) && session === s) teardown(`connection ${pc.connectionState}`, true);
      };

      const t0 = performance.now();
      await pc.setLocalDescription(await pc.createOffer());
      await gathered(pc);
      const tGathered = performance.now();
      const offer = pc.localDescription.sdp;
      // No public address at all (UDP blocked here): no direct path can form, so don't wait
      // for one when the WebSocket can carry the session.
      if (wsUrl && !/ typ (srflx|relay)/.test(offer)) throw new Error('no UDP path from this network');
      ws.send(JSON.stringify({ t: 'offer', sdp: offer }));
      const reply = await next(15_000);
      const tAnswered = performance.now();
      if (reply.t === 'error') throw new Error(reply.msg);
      if (reply.t !== 'answer') throw new Error('unexpected reply from glassdesk.dev');
      if (!(await verifyAnswer(hostKey, offer, reply.sdp, reply.sig))) {
        throw fatal("Couldn't verify this computer's identity; not connecting.");
      }
      await pc.setRemoteDescription({ type: 'answer', sdp: reply.sdp });
      onMode(transceivers.some((t) => t.currentDirection === 'recvonly'));
      ws.close();
      await opened(pc, dgram, wsUrl ? CONNECT_TIMEOUT_WITH_FALLBACK_MS : CONNECT_TIMEOUT_MS);
      log('info', 'remote: connected', {
        relay: await usesRelay(pc),
        gatherMs: Math.round(tGathered - t0),
        answerMs: Math.round(tAnswered - tGathered),
        connectMs: Math.round(performance.now() - tAnswered),
      });
      post(gen, { t: 'open' });
    } catch (e) {
      try { ws.close(); } catch {}
      if (session?.gen === gen) teardown('failed', false);
      if (wsUrl && !e.fatal && gen === latestGen) {
        log('info', 'remote: no direct path; trying the WebSocket', { reason: e.message ?? String(e) });
        try { await connectViaWs(gen, tStart); return; } catch {}
      }
      post(gen, { t: 'failed', msg: e.message ?? String(e) });
    }
  }

  port.onmessage = ({ data: m }) => {
    if (m.t === 'connect') { latestGen = m.gen; connect(m.gen); return; }
    const s = session;
    if (!s || m.gen !== s.gen) return;
    if (m.t === 'close') { teardown('closed by client', false); return; }
    if (s.ws) { onWorkerMessageWs(s, m); return; }
    if (m.t === 'dgram') {
      if (s.dgram.readyState === 'open' && s.dgram.bufferedAmount < BUFFER_LOW) s.dgram.send(m.data);
    } else if (m.t === 'open-stream') {
      const ch = s.pc.createDataChannel(m.kind, { ordered: true });
      s.channels.set(m.id, ch);
      wireChannel(s.gen, m.id, ch);
      s.senders.set(m.id, sender(s.gen, m.id, ch));
    } else if (m.t === 'data') {
      senderFor(s, m.id)?.data(new Uint8Array(m.data));
    } else if (m.t === 'end') {
      senderFor(s, m.id)?.end();
    }
  };

  function onWorkerMessageWs(s, m) {
    if (m.t === 'dgram') {
      if (s.ws.readyState === WebSocket.OPEN && s.ws.bufferedAmount < BUFFER_LOW) wsSend(s, WS_DGRAM, undefined, new Uint8Array(m.data));
    } else if (m.t === 'open-stream') {
      wsSend(s, WS_OPEN, m.id, new Uint8Array([m.kind === 'bi' ? 0 : 1]));
      s.senders.set(m.id, wsSender(s, m.id));
    } else if (m.t === 'data') {
      senderForWs(s, m.id).data(new Uint8Array(m.data));
    } else if (m.t === 'end') {
      senderForWs(s, m.id).end();
    }
  }

  function senderForWs(s, id) {
    let snd = s.senders.get(id);
    if (!snd) s.senders.set(id, (snd = wsSender(s, id)));
    return snd;
  }

  /** The live connection's stats (for the HUD), if connected over WebRTC. */
  api.stats = () => session?.pc?.getStats();

  function senderFor(s, id) {
    let snd = s.senders.get(id);
    if (!snd && s.channels.has(id)) s.senders.set(id, (snd = sender(s.gen, id, s.channels.get(id))));
    return snd;
  }
  return api;
}

export async function verifyAnswer(hostKey, offerSdp, answerSdp, sig) {
  try {
    const key = await crypto.subtle.importKey('raw', fromB64url(hostKey), { name: 'ECDSA', namedCurve: 'P-256' }, false, ['verify']);
    const data = new TextEncoder().encode(`glass-rtc-v1\n${offerSdp}\n--\n${answerSdp}`);
    return await crypto.subtle.verify({ name: 'ECDSA', hash: 'SHA-256' }, key, fromB64url(sig), data);
  } catch {
    return false;
  }
}

function gathered(pc) {
  if (pc.iceGatheringState === 'complete') return Promise.resolve();
  return Promise.race([
    new Promise((resolve) => pc.addEventListener('icegatheringstatechange', () => { if (pc.iceGatheringState === 'complete') resolve(); })),
    sleep(GATHER_TIMEOUT_MS),
  ]);
}

/** An error no other way of connecting would get past. */
function fatal(msg) {
  const e = new Error(msg);
  e.fatal = true;
  return e;
}

function opened(pc, channel, timeoutMs) {
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error('Couldn’t make a direct connection to this computer.')), timeoutMs);
    const check = () => {
      if (channel.readyState === 'open') { clearTimeout(timer); resolve(); }
      else if (['failed', 'closed'].includes(pc.connectionState)) { clearTimeout(timer); reject(new Error('Couldn’t make a direct connection to this computer.')); }
    };
    channel.addEventListener('open', check);
    pc.addEventListener('connectionstatechange', check);
    check();
  });
}

/** Whether traffic goes through a TURN relay (for the logs: direct is the goal). */
async function usesRelay(pc) {
  try {
    const stats = await pc.getStats();
    for (const s of stats.values()) {
      if (s.type === 'candidate-pair' && s.nominated && s.state === 'succeeded') {
        return stats.get(s.localCandidateId)?.candidateType === 'relay' || stats.get(s.remoteCandidateId)?.candidateType === 'relay';
      }
    }
  } catch {}
  return undefined;
}

const sleep = (ms) => new Promise((r) => setTimeout(r, Math.max(0, ms)));
