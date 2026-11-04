// Owns the WebTransport session, frame reassembly, WebCodecs decode and drawing,
// so none of the video path touches the main thread.

import {
  AUDIO_HEADER_LEN, KIND_AUDIO, KIND_AUDIO_OPUS, KIND_KEYFRAME_REQUEST, KIND_PONG, KIND_VIDEO, micPacket, ping, readAudioHeader, readPong, readVideoHeader, KIND_CURSOR, readCursor,
} from './proto.js';
import { VideoStream, probeCodecs, probeDecoders, setDecoderStep } from './video-stream.js';
import { RtcTransport } from './rtc-transport.js';

const PING_INTERVAL_MS = 500;
const CLOCK_SAMPLES = 16;
const RECONNECT_MIN_MS = 250;
const RECONNECT_MAX_MS = 5000;
// Pongs arrive every PING_INTERVAL_MS, so this long without any datagram means the
// session is dead even if the transport hasn't noticed (e.g. after iPadOS suspends the tab).
const SILENCE_TIMEOUT_MS = 3000;

const log = (level, msg, data) => self.postMessage({ type: 'log', level, msg, data });
self.addEventListener('unhandledrejection', (e) =>
  log('error', `worker unhandled rejection: ${e.reason?.message ?? e.reason}`, { stack: e.reason?.stack }));

const STREAM_INPUT = 0x49; // 'I'
const STREAM_FILE = 0x46; // 'F'
const STREAM_LOSSLESS = 0x50; // 'P': a lossless picture (sharp text)
const STREAM_CLIPBOARD = 0x43; // 'C': a clipboard with an image, either way
const encoder = new TextEncoder();
const textDecoder = new TextDecoder();

let url;
let certHash;
let token;
let device; // e.g. "Safari on iPad", shown on the host
let rtcPort; // remote access: WebRTC over the main thread instead of WebTransport
let link; // magic link code; traded for a guest pass on first sign-in
let pass; // guest pass
let wideColor = false; // the screen shows Display P3: draw video in it
// This viewer draws the host's pointer itself (cursor.js), offered in hello.
let localCursor = false;
let codecs = ['h264']; // what this browser decodes, offered in hello (HEVC if hardware, Opus)
let rejected = false;
let controlWriter;
let audioPort; // MessagePort of the main thread's AudioWorkletNode
let audioOn = false;
let visible = true;
let audioPackets = 0;
let audioLost = 0;
let lastAudioSeq;
let audioUnderruns = 0;
let audioPeak = 0; // 0..32768, since the last stats
let audioCodec = ''; // 'pcm' or 'opus': what the host sends
let opusDecoder; // WebCodecs AudioDecoder for KIND_AUDIO_OPUS
let micOn = false;
let micSeq = 0;
let transport;
let lastRxMs = 0;
let kill; // ends the current session's read loop even if the transport never reports closing
let wake; // resolves the reconnect backoff early
let denied = false; // the host's owner said no; don't knock again on our own
let datagramWriter;
let inputWriter;

// Host clock offset (µs), taken from the lowest-RTT recent ping.
const clockSamples = [];
let clockOffsetUs = 0;
// Until the first pong, the host clock offset is unknown and latency can't be measured.
let clockSynced = false;
let rttMs = 0;

const stats = { frames: 0, bytes: 0, latencySamples: 0, latencySum: 0, latencyMax: 0, dropped: 0, lossless: 0 };

const streamEnv = {
  log,
  post: (msg) => self.postMessage(msg),
  stats,
  hostNowUs: () => (clockSynced ? performance.now() * 1000 + clockOffsetUs : undefined),
  requestKeyframe: (index) => datagramWriter?.write(new Uint8Array([KIND_KEYFRAME_REQUEST, index])).catch(() => {}),
};
const streams = []; // VideoStream per host stream number
function videoStream(index) {
  return (streams[index] ??= new VideoStream(index, streamEnv));
}

self.onmessage = async ({ data }) => {
  if (data.type === 'init') {
    ({ url, certHash, token, link, pass, rtcPort, device } = data);
    localCursor = Boolean(data.localCursor);
    wideColor = Boolean(data.wideColor);
    setDecoderStep(data.decoderStep ?? 0);
    await probeDecoders(log);
    codecs = await probeCodecs();
    // Opus system audio (~100 kbps instead of 1.5 Mbps of PCM) where WebCodecs decodes it;
    // without AudioDecoder (Safari < 26) the host keeps sending PCM.
    if (await decodesOpus()) codecs.push('opus');
    setInterval(() => datagramWriter?.write(ping(performance.now())).catch(() => {}), PING_INTERVAL_MS);
    setInterval(tick, 1000);
    run();
  } else if (data.type === 'canvas') {
    videoStream(data.stream).setCanvas(data.canvas, wideColor);
  } else if (data.type === 'streams') {
    // Streams the host no longer sends give up their decoders.
    for (const s of streams.slice(data.count)) s?.release();
  } else if (data.type === 'input') {
    inputWriter?.write(data.bytes).catch(() => {});
  } else if (data.type === 'control') {
    sendControl(data.msg);
  } else if (data.type === 'localCursor') {
    if (localCursor === data.on) return;
    localCursor = data.on;
    sendControl({ t: 'localCursor', on: localCursor });
  } else if (data.type === 'sendFile') {
    sendFile(data.file, data.dir).catch((e) => log('error', `send file failed: ${e.message ?? e}`));
  } else if (data.type === 'sendClipboard') {
    sendClipboard(data.clip).catch((e) => log('error', `send clipboard failed: ${e.message ?? e}`));
  } else if (data.type === 'switchHost') {
    url = data.url;
    rejected = false;
    // End the current session; the reconnect loop picks up the new URL immediately.
    kill?.('switching machine');
    wake?.();
  } else if (data.type === 'token') {
    token = data.token;
    rejected = false;
    transport?.close();
    wake?.();
  } else if (data.type === 'visible') {
    setVisible(data.visible);
  } else if (data.type === 'audioPort') {
    audioPort = data.port;
    // Underruns are normal when a sound ends; count them for the stats instead of logging.
    audioPort.onmessage = ({ data: m }) => { audioUnderruns = m.underruns; };
  } else if (data.type === 'micPort') {
    // 5 ms mono packets from the main thread's mic worklet, sent as datagrams.
    data.port.onmessage = ({ data: pcm }) => {
      if (!micOn || !datagramWriter) return;
      const captureUs = clockSynced ? performance.now() * 1000 + clockOffsetUs : 0;
      micSeq = (micSeq + 1) >>> 0;
      datagramWriter.write(micPacket(micSeq, captureUs, 1, new Int16Array(pcm))).catch(() => {});
    };
  } else if (data.type === 'mic') {
    micOn = data.on;
    sendControl({ t: 'mic', on: micOn });
  } else if (data.type === 'audioToggle') {
    audioOn = !audioOn;
    sendControl({ t: 'audio', on: audioOn });
    self.postMessage({ type: 'audio', on: audioOn });
  } else if (data.type === 'askAgain') {
    denied = false;
    wake?.();
  } else if (data.type === 'leave') {
    leaving = true;
    kill?.('page closing');
    try { transport?.close(); } catch {}
  } else if (data.type === 'suspend') {
    // Another tab of this browser shows this computer now: stay off until told otherwise.
    suspended = true;
    kill?.('another tab took over');
    try { transport?.close(); } catch {}
  } else if (data.type === 'resume') {
    suspended = false;
    wake?.();
  } else if (data.type === 'wake') {
    leaving = false;
    // Tab became visible or network came back: skip the remaining backoff, and drop a
    // session that went quiet while suspended instead of waiting for the watchdog.
    if (transport && performance.now() - lastRxMs > PING_INTERVAL_MS * 2) kill?.('woke with a stale session');
    wake?.();
  }
};

// The page is going away (reload, navigation): close the session now rather than leave
// the host to notice, and don't reconnect unless the page comes back (bfcache).
let leaving = false;
let suspended = false;

async function run() {
  let delay = RECONNECT_MIN_MS;
  for (;;) {
    while (leaving || suspended) await new Promise((resolve) => { wake = resolve; });
    // After a rejection, retrying would only be rejected again; wait for a new token.
    while (rejected) {
      self.postMessage({ type: 'status', text: 'waiting for access token' });
      await new Promise((resolve) => { wake = resolve; });
    }
    // Turned away by the person at the computer: asking again is the user's call.
    while (denied) {
      self.postMessage({ type: 'status', text: 'not let in' });
      await new Promise((resolve) => { wake = resolve; });
    }
    try {
      await connect();
      delay = RECONNECT_MIN_MS;
      self.postMessage({ type: 'status', level: 'warn', text: 'disconnected, reconnecting…' });
    } catch (e) {
      self.postMessage({ type: 'status', level: 'error', text: `connection failed: ${e.message ?? e}, retrying…` });
    }
    resetSession();
    await new Promise((resolve) => {
      wake = resolve;
      setTimeout(resolve, delay);
    });
    wake = undefined;
    delay = Math.min(delay * 2, RECONNECT_MAX_MS);
  }
}

function resetSession() {
  kill = undefined;
  try { transport?.close(); } catch {}
  transport = datagramWriter = inputWriter = controlWriter = undefined;
  for (const s of streams) s?.reset();
  clockSamples.length = 0;
  clockSynced = false;
}

/** Resolves when the session ends; throws if it never came up. */
async function connect() {
  const options = certHash
    ? { serverCertificateHashes: [{ algorithm: 'sha-256', value: new Uint8Array(certHash) }] }
    : {};
  log('info', 'connecting', rtcPort ? { remote: true } : { url, pinned: !!certHash });
  transport = rtcPort ? new RtcTransport(rtcPort) : new WebTransport(url, options);
  transport.closed.catch(() => {});
  await transport.ready;
  lastRxMs = performance.now();
  log('info', 'transport ready', {
    createWritable: typeof transport.datagrams.createWritable === 'function',
    writable: !!transport.datagrams.writable,
    maxDatagramSize: transport.datagrams.maxDatagramSize,
  });
  self.postMessage({ type: 'status', text: 'connected' });
  self.postMessage({ type: 'connected' });

  // Current spec (Safari) uses createWritable(); Chromium still exposes `writable`.
  const writable = transport.datagrams.createWritable?.() ?? transport.datagrams.writable;
  datagramWriter = writable.getWriter();
  // Sync the clock right away instead of waiting for the ping interval.
  datagramWriter.write(ping(performance.now())).catch(() => {});

  // Control stream first: the host ignores everything else until hello is accepted.
  const control = await transport.createBidirectionalStream();
  controlWriter = control.writable.getWriter();
  sendControl({ t: 'hello', token, link, pass, device, codecs, ua: navigator.userAgent, cursor: localCursor });
  if (audioOn) sendControl({ t: 'audio', on: true });
  if (micOn) sendControl({ t: 'mic', on: true });
  if (!visible) sendControl({ t: 'video', on: false });
  readControl(control.readable).catch((e) => log('warn', `control read ended: ${e.message ?? e}`));

  const inputStream = await transport.createUnidirectionalStream();
  inputWriter = inputStream.getWriter();
  inputWriter.write(new Uint8Array([STREAM_INPUT]));
  acceptIncoming(transport).catch(() => {});

  const reader = transport.datagrams.readable.getReader();
  const killed = new Promise((resolve) => { kill = resolve; });
  for (;;) {
    let result;
    try {
      result = await Promise.race([reader.read(), killed.then((reason) => ({ killed: reason }))]);
    } catch (e) {
      log('warn', `datagram read ended: ${e.message ?? e}`);
      break;
    }
    if (result.killed) {
      log('warn', `session killed: ${result.killed}`);
      reader.releaseLock?.();
      break;
    }
    const { value, done } = result;
    if (done) break;
    lastRxMs = performance.now();
    const view = new DataView(value.buffer, value.byteOffset, value.byteLength);
    const kind = view.getUint8(0);
    if (kind === KIND_VIDEO) {
      const h = readVideoHeader(view);
      videoStream(h.stream).onFragment(h, value);
    }
    else if (kind === KIND_AUDIO) onAudio(view, value);
    else if (kind === KIND_AUDIO_OPUS) onOpus(view, value);
    else if (kind === KIND_PONG) onPong(view);
    else if (kind === KIND_CURSOR && value.byteLength >= 16) self.postMessage({ type: 'cursor', pos: readCursor(view) });
  }
}

/**
 * iPadOS has few hardware decoders; a hidden tab gives its up and pauses video on the
 * host, then resumes from a fresh keyframe when shown again.
 */
function setVisible(next) {
  if (next === visible) return;
  visible = next;
  sendControl({ t: 'video', on: visible });
  if (!visible) for (const s of streams) s?.release();
  log('info', `video ${visible ? 'resumed' : 'paused'} (tab ${visible ? 'visible' : 'hidden'})`);
}

function sendControl(msg) {
  if (!controlWriter) return;
  const body = encoder.encode(JSON.stringify(msg));
  const frame = new Uint8Array(4 + body.length);
  new DataView(frame.buffer).setUint32(0, body.length, true);
  frame.set(body, 4);
  controlWriter.write(frame).catch(() => {});
}

/** Reads exactly `n` bytes from a byte-stream reader, buffering leftovers. */
function byteReader(readable) {
  const reader = readable.getReader();
  let buf = new Uint8Array(0);
  return {
    async read(n) {
      while (buf.length < n) {
        const { value, done } = await reader.read();
        if (done) return undefined;
        const next = new Uint8Array(buf.length + value.length);
        next.set(buf);
        next.set(value, buf.length);
        buf = next;
      }
      const out = buf.slice(0, n);
      buf = buf.slice(n);
      return out;
    },
    /** Yields whatever is buffered, then the rest of the stream as-is. */
    async *rest() {
      if (buf.length) yield buf;
      buf = new Uint8Array(0);
      for (;;) {
        const { value, done } = await reader.read();
        if (done) return;
        yield value;
      }
    },
  };
}

async function readControl(readable) {
  const r = byteReader(readable);
  for (;;) {
    const len = await r.read(4);
    if (!len) return;
    const body = await r.read(new DataView(len.buffer).getUint32(0, true));
    if (!body) return;
    const msg = JSON.parse(textDecoder.decode(body));
    if (msg.t === 'pass') {
      // The link is used up; reconnects sign in with the pass.
      pass = msg.pass;
      link = undefined;
    }
    if (msg.t === 'denied') denied = true;
    if (msg.t === 'error' && msg.msg === 'unauthorized') {
      rejected = true;
      link = undefined;
      pass = undefined;
      self.postMessage({ type: 'unauthorized' });
    }
    self.postMessage({ type: 'control', msg });
  }
}

async function sendFile(file, dir) {
  if (!transport) throw new Error('not connected');
  const id = `${Date.now()}-${file.name}`;
  // `dir`: the file browser's folder (relative to the home folder) instead of Downloads.
  const header = encoder.encode(JSON.stringify({ name: file.name, size: file.size, ...(dir != null && { dir }) }));
  const prefix = new Uint8Array(5 + header.length);
  prefix[0] = STREAM_FILE;
  new DataView(prefix.buffer).setUint32(1, header.length, true);
  prefix.set(header, 5);

  const stream = await transport.createUnidirectionalStream();
  const writer = stream.getWriter();
  await writer.write(prefix);
  const reader = file.stream().getReader();
  let sent = 0;
  let lastReport = 0;
  for (;;) {
    const { value, done } = await reader.read();
    if (done) break;
    await writer.write(value);
    sent += value.byteLength;
    if (performance.now() - lastReport > 200) {
      lastReport = performance.now();
      self.postMessage({ type: 'fileProgress', id, name: file.name, done: sent, total: file.size, direction: 'up' });
    }
  }
  await writer.close();
  self.postMessage({ type: 'fileProgress', id, name: file.name, done: file.size, total: file.size, direction: 'up' });
  log('info', 'file sent', { name: file.name, size: file.size });
}

async function acceptIncoming(t) {
  const reader = t.incomingUnidirectionalStreams.getReader();
  for (;;) {
    const { value: stream, done } = await reader.read();
    if (done) return;
    receiveStream(stream).catch((e) => log('error', `receive stream failed: ${e.message ?? e}`));
  }
}

async function receiveStream(readable) {
  const r = byteReader(readable);
  const tag = await r.read(1);
  if (tag?.[0] === STREAM_FILE) return receiveFile(r);
  if (tag?.[0] === STREAM_LOSSLESS) return receiveLossless(r);
  if (tag?.[0] === STREAM_CLIPBOARD) return receiveClipboard(r);
}

/** The host clipboard's image, asked for with `clipboardImage`: header, then a PNG. */
async function receiveClipboard(r) {
  const len = await r.read(4);
  const header = JSON.parse(textDecoder.decode(await r.read(new DataView(len.buffer).getUint32(0, true))));
  const chunks = [];
  for await (const chunk of r.rest()) chunks.push(chunk);
  const image = new Blob(chunks, { type: header.mime || 'image/png' });
  if (image.size !== header.size) throw new Error(`clipboard image: got ${image.size} of ${header.size} bytes`);
  self.postMessage({ type: 'clipboardImage', image });
}

/** A clipboard with an image, to the host (and pasted there with `paste`). */
async function sendClipboard({ text = '', html, image, paste = false }) {
  if (!transport) throw new Error('not connected');
  const header = encoder.encode(JSON.stringify({ text, html, mime: image.type, size: image.size, paste }));
  const prefix = new Uint8Array(5 + header.length);
  prefix[0] = STREAM_CLIPBOARD;
  new DataView(prefix.buffer).setUint32(1, header.length, true);
  prefix.set(header, 5);
  const stream = await transport.createUnidirectionalStream();
  const writer = stream.getWriter();
  await writer.write(prefix);
  await writer.write(new Uint8Array(await image.arrayBuffer()));
  await writer.close();
  log('info', 'clipboard image sent', { size: image.size, type: image.type, paste });
}

/** A lossless picture of part of a stream: header (x, y, w, h of fw × fh), then a PNG. */
async function receiveLossless(r) {
  const len = await r.read(4);
  const h = JSON.parse(textDecoder.decode(await r.read(new DataView(len.buffer).getUint32(0, true))));
  const chunks = [];
  for await (const chunk of r.rest()) chunks.push(chunk);
  const stream = streams[h.stream];
  if (!stream) return;
  const bitmap = await createImageBitmap(new Blob(chunks, { type: 'image/png' }));
  if (!stream.onLossless(h, bitmap)) {
    sendControl({ t: 'losslessReset' });
    return;
  }
  stats.lossless++;
  // One pixel of what's now on screen, so tests can check it's exact.
  const cx = h.x + (h.w >> 1);
  const cy = h.y + (h.h >> 1);
  const scale = stream.ctx.canvas.width / h.fw;
  const [red, green, blue] = stream.ctx.getImageData(Math.floor(cx * scale), Math.floor(cy * scale), 1, 1).data;
  log('info', 'lossless refresh', { stream: h.stream, x: h.x, y: h.y, w: h.w, h: h.h, fw: h.fw, fh: h.fh, pixel: [cx, cy, red, green, blue] });
}

async function receiveFile(r) {
  const len = await r.read(4);
  const header = JSON.parse(textDecoder.decode(await r.read(new DataView(len.buffer).getUint32(0, true))));
  const id = `${Date.now()}-${header.name}`;
  const chunks = [];
  let got = 0;
  let lastReport = 0;
  for await (const chunk of r.rest()) {
    chunks.push(chunk);
    got += chunk.byteLength;
    if (performance.now() - lastReport > 200) {
      lastReport = performance.now();
      self.postMessage({ type: 'fileProgress', id, name: header.name, done: got, total: header.streamed ? null : header.size, direction: 'down' });
    }
  }
  // A folder zipped on the fly has no size up front: it's whole if it ends with the zip's
  // end record (a broken remote stream just ends early).
  if (header.streamed ? !endsLikeZip(chunks) : got !== header.size) throw new Error(`${header.name}: cut short after ${got} bytes`);
  const file = new File(chunks, header.name);
  self.postMessage({ type: 'fileReceived', id, file });
  log('info', 'file received', { name: header.name, size: got });
}

/** The last 22 bytes start the zip's end-of-central-directory record (no comment). */
function endsLikeZip(chunks) {
  const tail = new Uint8Array(22);
  let need = 22;
  for (let i = chunks.length - 1; i >= 0 && need; i--) {
    const c = chunks[i];
    const n = Math.min(need, c.byteLength);
    tail.set(c.subarray(c.byteLength - n), need - n);
    need -= n;
  }
  return !need && tail[0] === 0x50 && tail[1] === 0x4b && tail[2] === 5 && tail[3] === 6;
}

function countAudio(seq, codec) {
  if (codec !== audioCodec) lastAudioSeq = undefined;
  audioCodec = codec;
  if (lastAudioSeq !== undefined && seq !== ((lastAudioSeq + 1) >>> 0)) audioLost++;
  lastAudioSeq = seq;
  audioPackets++;
}

function onAudio(view, bytes) {
  if (!audioPort || !audioOn) return;
  countAudio(view.getUint32(4, true), 'pcm');
  // Copy into a fresh, aligned buffer and hand it over without another copy.
  const pcm = bytes.slice(AUDIO_HEADER_LEN).buffer;
  // The loudest sample since the last stats (silence still comes as packets).
  const samples = new Int16Array(pcm);
  for (let i = 0; i < samples.length; i++) {
    const v = Math.abs(samples[i]);
    if (v > audioPeak) audioPeak = v;
  }
  audioPort.postMessage({ pcm, channels: view.getUint8(1) }, [pcm]);
}

const OPUS_CONFIG = { codec: 'opus', sampleRate: 48000, numberOfChannels: 2 };

async function decodesOpus() {
  if (typeof AudioDecoder === 'undefined') return false;
  const { supported } = await AudioDecoder.isConfigSupported(OPUS_CONFIG).catch(() => ({}));
  return Boolean(supported);
}

/** One Opus packet: decoded by WebCodecs, played by the same worklet as PCM. */
function onOpus(view, bytes) {
  if (!audioPort || !audioOn) return;
  const h = readAudioHeader(view);
  countAudio(h.seq, 'opus');
  if (opusDecoder?.state !== 'configured') {
    opusDecoder = new AudioDecoder({
      output: playDecoded,
      error: (e) => {
        log('warn', `opus decoder error: ${e.message ?? e}`);
        // Made afresh on the next packet.
        opusDecoder = undefined;
      },
    });
    opusDecoder.configure(OPUS_CONFIG);
  }
  // A decoder that has fallen behind only adds delay: skip until it catches up.
  if (opusDecoder.decodeQueueSize > 10) return;
  try {
    // Every Opus packet decodes on its own.
    opusDecoder.decode(new EncodedAudioChunk({ type: 'key', timestamp: h.captureUs, data: bytes.subarray(AUDIO_HEADER_LEN) }));
  } catch (e) {
    log('warn', `opus decode failed: ${e.message ?? e}`);
    opusDecoder = undefined;
  }
}

function playDecoded(audio) {
  const frames = audio.numberOfFrames;
  // f32-planar is the one format every browser must convert to.
  const planes = [];
  for (let c = 0; c < Math.min(audio.numberOfChannels, 2); c++) {
    const plane = new Float32Array(frames);
    audio.copyTo(plane, { planeIndex: c, format: 'f32-planar' });
    planes.push(plane);
  }
  audio.close();
  if (!audioPort || !audioOn || !planes.length) return;
  for (const plane of planes) {
    for (let i = 0; i < plane.length; i++) {
      const v = Math.abs(plane[i]) * 32768;
      if (v > audioPeak) audioPeak = v;
    }
  }
  audioPort.postMessage({ planes }, planes.map((p) => p.buffer));
}

function onPong(view) {
  const now = performance.now();
  const { clientMs, hostUs } = readPong(view);
  const rtt = now - clientMs;
  clockSamples.push({ rtt, offsetUs: hostUs - ((clientMs + now) / 2) * 1000 });
  if (clockSamples.length > CLOCK_SAMPLES) clockSamples.shift();
  const best = clockSamples.reduce((a, b) => (b.rtt < a.rtt ? b : a));
  clockOffsetUs = best.offsetUs;
  clockSynced = true;
  rttMs = rtt;
}

function tick() {
  if (kill && performance.now() - lastRxMs > SILENCE_TIMEOUT_MS) {
    kill(`no data for ${SILENCE_TIMEOUT_MS} ms`);
    kill = undefined;
  }
  reportStats();
}

function reportStats() {
  // Feeds the host's adaptive bitrate. Hidden tabs decode nothing, so they don't report.
  if (visible && (stats.frames || stats.dropped)) {
    sendControl({
      t: 'netStats',
      dropped: stats.dropped,
      frames: stats.frames,
      latency_ms: stats.latencySamples ? stats.latencySum / stats.latencySamples : null,
      // How much of the allowance was used: the host only raises it when it's needed.
      received_bps: stats.bytes * 8,
    });
  }
  self.postMessage({
    type: 'stats',
    fps: stats.frames,
    mbps: (stats.bytes * 8) / 1e6,
    latencyAvg: stats.latencySamples ? stats.latencySum / stats.latencySamples : 0,
    latencyMax: stats.latencyMax,
    rtt: rttMs,
    dropped: stats.dropped,
    audioPackets,
    audioLost,
    audioUnderruns,
    audioPeak: audioPeak / 32768,
    audioCodec,
    lossless: stats.lossless,
    // Showing a lossless picture now (until the screen changes).
    holding: streams.some((v) => v?.holding),
  });
  audioPackets = 0;
  audioLost = 0;
  audioPeak = 0;
  Object.assign(stats, { frames: 0, bytes: 0, latencySamples: 0, latencySum: 0, latencyMax: 0, dropped: 0, lossless: 0 });
}
