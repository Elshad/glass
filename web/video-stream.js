// One display's video: fragment reassembly, loss detection, WebCodecs decode and drawing.
// The worker owns one per streamed display.

import { fletcher16, FLAG_HEVC, FLAG_KEYFRAME, FLAG_REFRESH, VIDEO_HEADER_LEN } from './proto.js';

const KEYFRAME_RETRY_MS = 250;

// Safari's hardware decoder occasionally rejects valid frames (checksums match). If a
// configuration fails repeatedly, every stream steps down to the next one.
const DECODER_LADDER = [
  { name: 'hw-lowlatency', optimizeForLatency: true, hardwareAcceleration: 'prefer-hardware' },
  { name: 'hw', optimizeForLatency: false, hardwareAcceleration: 'prefer-hardware' },
  { name: 'sw', optimizeForLatency: true, hardwareAcceleration: 'prefer-software' },
  // No hint: whatever the browser has (e.g. Chromium on Linux without VA-API).
  { name: 'auto', optimizeForLatency: true },
];
const LADDER_WINDOW_MS = 120_000;
const LADDER_FAILURES = 2;
const ladder = { step: 0, failures: [] };

/**
 * Skips configurations this browser can't do at all. "prefer-hardware" fails outright
 * where there's no hardware decoder (common on Linux), rather than falling back.
 */
export async function probeDecoders(log, codec = 'avc1.640033') {
  const supported = [];
  for (const config of DECODER_LADDER) {
    const { supported: ok } = await VideoDecoder.isConfigSupported({ codec, ...config }).catch(() => ({}));
    if (ok) supported.push(config.name);
  }
  const first = DECODER_LADDER.findIndex((c) => supported.includes(c.name));
  if (first > ladder.step) ladder.step = first;
  log(first < 0 ? 'error' : 'info', 'decoder support', { supported, using: DECODER_LADDER[ladder.step]?.name });
}

// HEVC Main, level 4.1: if this decodes in hardware, the host may send HEVC.
const HEVC_PROBE = 'hvc1.1.6.L123.B0';
// HEVC Range Extensions, Main 4:2:2 10 (profile 4; max 12/10-bit, max 4:2:2, lower bit rate).
export const HEVC_REXT_PROBE = 'hvc1.4.10.L123.9D.08';

/** The codecs to offer the host in hello: always H.264, HEVC where it's hardware-decoded. */
export async function probeCodecs() {
  const codecs = ['h264'];
  if (typeof VideoDecoder === 'undefined') return codecs;
  const { supported } = await VideoDecoder.isConfigSupported({ codec: HEVC_PROBE, hardwareAcceleration: 'prefer-hardware' }).catch(() => ({}));
  if (supported) codecs.push('hevc');
  // HEVC Main 4:2:2 10 (Range Extensions), what the host sends for sharp text.
  if (supported) {
    const rext = await VideoDecoder.isConfigSupported({ codec: HEVC_REXT_PROBE, hardwareAcceleration: 'prefer-hardware' }).catch(() => ({}));
    if (rext.supported) codecs.push('hevc-rext');
  }
  return codecs;
}

/** Restores a fallback found in an earlier session (remembered per device by main.js). */
export function setDecoderStep(step) {
  ladder.step = Math.max(0, Math.min(step | 0, DECODER_LADDER.length - 1));
}

export class VideoStream {
  /**
   * @param index   stream number (matches the host's)
   * @param env     { log, post, requestKeyframe(index), hostNowUs(), stats }
   */
  constructor(index, env) {
    this.index = index;
    this.env = env;
    this.ctx = undefined;
    this.pending = new Map(); // frameId -> { parts, received, keyframe, captureUs, checksum }
    this.acks = new Map(); // capture time -> frameId, for frames being decoded
    this.reset();
    this.decoder = undefined;
    this.codec = undefined;
    this.currentSps = undefined;
    this.lastChunk = undefined;
    this.sinceKeyframe = 0;
    this.lastKeyframeRequest = 0;
  }

  /** `wide`: this screen shows Display P3, so draw in it (a P3 stream isn't clipped). */
  setCanvas(canvas, wide = false) {
    this.colorSpace = wide ? 'display-p3' : 'srgb';
    try {
      this.ctx = canvas.getContext('2d', { alpha: false, desynchronized: true, colorSpace: this.colorSpace });
    } catch {
      this.colorSpace = 'srgb';
      this.ctx = canvas.getContext('2d', { alpha: false, desynchronized: true });
    }
  }

  /** New session: frame ids restart, so forget reassembly state. */
  reset() {
    this.pending.clear();
    this.lastDecodedId = undefined;
    this.waitingForKeyframe = true;
    this.dropLossless();
  }

  /**
   * Lossless refresh (sharp text): the host's exact picture of a still screen, a PNG of
   * the part `h` (x, y, w, h of fw × fh) that changed since its last one. It's kept on top
   * of the video until the screen really changes; frames that only refine the same picture
   * (FLAG_REFRESH) are drawn under it. False if it couldn't be used (the host then sends a
   * whole picture next).
   */
  onLossless(h, bitmap) {
    const ctx = this.ctx;
    const whole = h.x === 0 && h.y === 0 && h.w === h.fw && h.h === h.fh;
    // Older than what's on screen, or a part without the picture it belongs to.
    const stale = h.capture_us < this.changedUs;
    const base = this.backing && this.backing.canvas.width === h.fw && this.backing.canvas.height === h.fh;
    if (!ctx || stale || (!base && !whole)) {
      bitmap.close();
      return false;
    }
    if (!base) this.backing = new OffscreenCanvas(h.fw, h.fh).getContext('2d', { alpha: false, colorSpace: this.colorSpace ?? 'srgb' });
    this.backing.drawImage(bitmap, h.x, h.y);
    bitmap.close();
    this.holding = true;
    this.drawBacking();
    return true;
  }

  drawBacking() {
    const { canvas } = this.ctx;
    this.ctx.drawImage(this.backing.canvas, 0, 0, canvas.width, canvas.height);
  }

  dropLossless() {
    this.holding = false;
    this.backing = undefined;
    this.changedUs = 0;
    this.refreshes = new Set(); // capture times of frames that only refine the picture
  }

  /** Gives up the hardware decoder (hidden tab or stream no longer shown). */
  release() {
    try { this.decoder?.close(); } catch {}
    this.decoder = undefined;
    this.currentSps = undefined;
    this.reset();
  }

  onFragment(h, bytes) {
    this.env.stats.bytes += bytes.byteLength;
    let frame = this.pending.get(h.frameId);
    if (!frame) {
      frame = {
        parts: new Array(h.fragCount),
        received: 0,
        keyframe: (h.flags & FLAG_KEYFRAME) !== 0,
        hevc: (h.flags & FLAG_HEVC) !== 0,
        refresh: (h.flags & FLAG_REFRESH) !== 0,
        captureUs: h.captureUs,
        checksum: h.checksum,
      };
      this.pending.set(h.frameId, frame);
    }
    if (frame.parts[h.fragIdx]) return;
    // Copy, don't view: the browser may reuse the datagram's buffer for later datagrams.
    frame.parts[h.fragIdx] = bytes.slice(VIDEO_HEADER_LEN);
    if (++frame.received === h.fragCount) this.onFrame(h.frameId, frame);
  }

  onFrame(id, frame) {
    // Anything older and still incomplete is lost.
    for (const key of this.pending.keys()) if (key <= id) this.pending.delete(key);
    // A whole frame that won't be drawn (waiting for a keyframe) has still arrived: say so,
    // or the host takes the wait for a keyframe as a queue on the link.
    const skip = () => this.env.ack?.(this.index, id);

    const contiguous = this.lastDecodedId !== undefined && id === ((this.lastDecodedId + 1) >>> 0);
    // A delta in the other codec (the host just switched) can't follow this decoder.
    const sameCodec = this.hevc === frame.hevc;
    if (!frame.keyframe && (this.waitingForKeyframe || !contiguous || !sameCodec)) {
      this.env.stats.dropped++;
      this.requestKeyframe();
      skip();
      return;
    }

    const data = join(frame.parts);
    if (fletcher16(data) !== frame.checksum) {
      // Corrupted in transit or reassembly: never feed it to the decoder.
      this.env.log('error', 'frame checksum mismatch', {
        stream: this.index, id, size: data.byteLength, parts: frame.parts.length, keyframe: frame.keyframe,
      });
      this.env.stats.dropped++;
      this.requestKeyframe();
      skip();
      return;
    }
    if ((frame.keyframe && !this.ensureDecoder(data, frame.hevc)) || !this.decoder) {
      skip();
      return;
    }
    this.waitingForKeyframe = false;
    this.lastDecodedId = id;
    this.lastChunk = { type: frame.keyframe ? 'key' : 'delta', size: data.byteLength, id, nals: nalTypes(data, frame.hevc) };
    this.sinceKeyframe = frame.keyframe ? 0 : this.sinceKeyframe + 1;
    if (frame.refresh) {
      if (this.refreshes.size > 64) this.refreshes.clear();
      this.refreshes.add(frame.captureUs);
    }
    // Acked once drawn (by capture time, which the decoded frame carries).
    if (this.acks.size > 64) this.acks.clear();
    this.acks.set(frame.captureUs, id);
    this.decoder.decode(new EncodedVideoChunk({ type: frame.keyframe ? 'key' : 'delta', timestamp: frame.captureUs, data }));
  }

  requestKeyframe() {
    this.waitingForKeyframe = true;
    const now = performance.now();
    if (now - this.lastKeyframeRequest < KEYFRAME_RETRY_MS) return;
    this.lastKeyframeRequest = now;
    this.env.requestKeyframe(this.index);
  }

  ensureDecoder(annexB, hevc = false) {
    const sps = findNal(annexB, hevc ? HEVC_SPS : 7, hevc);
    if (!sps) return false;
    const hex = (b) => b.toString(16).padStart(2, '0');
    // A new SPS (e.g. after a display switch changes resolution, or a codec switch) gets
    // a fresh decoder.
    const spsKey = (hevc ? 'h' : 'a') + Array.from(sps, hex).join('');
    if (this.decoder && this.decoder.state === 'configured' && spsKey === this.currentSps) return true;
    this.currentSps = spsKey;

    try { this.decoder?.close(); } catch {}
    this.hevc = hevc;
    this.codec = hevc ? hevcCodec(sps) : `avc1.${hex(sps[1])}${hex(sps[2])}${hex(sps[3])}`;
    this.decoder = new VideoDecoder({ output: (f) => this.draw(f), error: (e) => this.onDecodeError(e) });
    this.decoder.configure({ codec: this.codec, ...DECODER_LADDER[ladder.step] });
    this.env.post({ type: 'codec', stream: this.index, codec: `${this.codec} ${DECODER_LADDER[ladder.step].name}` });
    return true;
  }

  onDecodeError(e) {
    const now = performance.now();
    ladder.failures = ladder.failures.filter((t) => now - t < LADDER_WINDOW_MS);
    ladder.failures.push(now);
    if (ladder.failures.length >= LADDER_FAILURES) this.stepDownDecoder();
    this.env.post({ type: 'status', level: 'error', text: `decoder error: ${e.message}` });
    this.env.log('error', 'decoder error detail', {
      stream: this.index, name: e.name, lastChunk: this.lastChunk, sinceKeyframe: this.sinceKeyframe,
      queue: this.decoder?.decodeQueueSize, codec: this.codec,
      config: DECODER_LADDER[ladder.step].name, failuresInWindow: ladder.failures.length,
    });
    this.currentSps = undefined;
    this.decoder = undefined;
    this.requestKeyframe();
  }

  async stepDownDecoder() {
    for (let next = ladder.step + 1; next < DECODER_LADDER.length; next++) {
      const config = { codec: this.codec, ...DECODER_LADDER[next] };
      const { supported } = await VideoDecoder.isConfigSupported(config).catch(() => ({}));
      if (supported) {
        this.env.log('warn', 'decoder fallback', { from: DECODER_LADDER[ladder.step].name, to: DECODER_LADDER[next].name });
        ladder.step = next;
        ladder.failures = [];
        this.env.post({ type: 'decoderStep', step: next });
        // Rebuild with the new config at the next keyframe.
        this.currentSps = undefined;
        this.requestKeyframe();
        return;
      }
      this.env.log('info', `decoder config ${DECODER_LADDER[next].name} unsupported`);
    }
  }

  draw(videoFrame) {
    const ctx = this.ctx;
    if (ctx) {
      const { displayWidth: w, displayHeight: h } = videoFrame;
      if (ctx.canvas.width !== w || ctx.canvas.height !== h) {
        // What colour the stream says it's in (Display P3 from a wide-colour display).
        const c = videoFrame.colorSpace;
        this.env.log('info', 'video colour', { stream: this.index, primaries: c?.primaries, transfer: c?.transfer, matrix: c?.matrix, canvas: this.colorSpace });
        ctx.canvas.width = w;
        ctx.canvas.height = h;
        this.env.post({ type: 'size', stream: this.index, width: w, height: h });
      }
      // A real change ends a lossless picture; a refinement of the same one doesn't.
      if (!this.refreshes.delete(videoFrame.timestamp)) {
        this.holding = false;
        this.changedUs = Math.max(this.changedUs, videoFrame.timestamp);
      }
      ctx.drawImage(videoFrame, 0, 0);
      if (this.holding) this.drawBacking();
    }
    const hostNow = this.env.hostNowUs();
    // Tell the host it's on screen: it sends the next frame when the link has room
    // (instead of queuing), and sets the bitrate by how long frames take to get here.
    const id = this.acks.get(videoFrame.timestamp);
    if (id !== undefined) {
      this.acks.delete(videoFrame.timestamp);
      this.env.ack?.(this.index, id);
    }
    videoFrame.close();

    const stats = this.env.stats;
    stats.frames++;
    if (hostNow === undefined) return;
    const latencyMs = (hostNow - videoFrame.timestamp) / 1000;
    stats.latencySamples++;
    stats.latencySum += latencyMs;
    stats.latencyMax = Math.max(stats.latencyMax, latencyMs);
  }
}

function join(parts) {
  let len = 0;
  for (const p of parts) len += p.byteLength;
  const out = new Uint8Array(len);
  let off = 0;
  for (const p of parts) {
    out.set(p, off);
    off += p.byteLength;
  }
  return out;
}

const HEVC_SPS = 33;
const nalType = (header, hevc) => (hevc ? (header >> 1) & 0x3f : header & 0x1f);

/** NAL unit types in an Annex B buffer, e.g. "7,8,5" or "1,1,1" (multi-slice). */
export function nalTypes(buf, hevc = false) {
  const types = [];
  for (let i = 0; i + 4 < buf.length; i++) {
    if (buf[i] === 0 && buf[i + 1] === 0 && buf[i + 2] === 0 && buf[i + 3] === 1) {
      types.push(nalType(buf[i + 4], hevc));
      i += 4;
    }
  }
  return types.join(',');
}

/** Returns the first NAL unit of `type` (header included) in an Annex B buffer. */
export function findNal(buf, type, hevc = false) {
  for (let i = 0; i + 4 < buf.length; i++) {
    if (buf[i] === 0 && buf[i + 1] === 0 && buf[i + 2] === 0 && buf[i + 3] === 1) {
      if (nalType(buf[i + 4], hevc) === type) {
        let end = i + 4;
        while (end + 3 < buf.length && !(buf[end] === 0 && buf[end + 1] === 0 && buf[end + 2] === 0 && buf[end + 3] === 1)) end++;
        return buf.subarray(i + 4, end + 3 < buf.length ? end : buf.length);
      }
    }
  }
  return undefined;
}

/** Drops emulation-prevention bytes (00 00 03 → 00 00) to read a NAL unit's fields. */
function unescape(nal) {
  const out = [];
  let zeros = 0;
  for (const b of nal) {
    if (zeros >= 2 && b === 3) {
      zeros = 0;
      continue;
    }
    zeros = b === 0 ? zeros + 1 : 0;
    out.push(b);
  }
  return out;
}

/**
 * The WebCodecs codec string for an HEVC SPS (2-byte NAL header first), from its
 * profile_tier_level, e.g. "hvc1.1.6.L123.B0" (ISO/IEC 14496-15 Annex E).
 */
export function hevcCodec(sps) {
  const b = unescape(sps);
  // b[2]: vps id, max sub-layers, temporal nesting. profile_tier_level starts at b[3].
  const space = b[3] >> 6;
  const tier = (b[3] >> 5) & 1;
  const profile = b[3] & 0x1f;
  const compat = ((b[4] << 24) | (b[5] << 16) | (b[6] << 8) | b[7]) >>> 0;
  let reversed = 0;
  for (let i = 0; i < 32; i++) reversed = ((reversed << 1) | ((compat >>> i) & 1)) >>> 0;
  const constraints = b.slice(8, 14);
  while (constraints.length && constraints[constraints.length - 1] === 0) constraints.pop();
  const level = b[14];
  const prefix = ['', 'A', 'B', 'C'][space];
  return [
    `hvc1.${prefix}${profile}`,
    reversed.toString(16).toUpperCase(),
    `${tier ? 'H' : 'L'}${level}`,
    ...constraints.map((c) => c.toString(16).toUpperCase()),
  ].join('.');
}
