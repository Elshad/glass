// Mirrors crates/glass-proto. All integers little-endian.

export const KIND_VIDEO = 1;
export const KIND_PING = 2;
export const KIND_PONG = 3;
export const KIND_KEYFRAME_REQUEST = 4;
export const KIND_NACK = 10;
export const KIND_AUDIO = 5;
export const KIND_MIC = 6;
export const KIND_AUDIO_OPUS = 7; // system audio as one Opus packet, same header as KIND_AUDIO
export const KIND_CURSOR = 9;
export const CURSOR_VISIBLE = 1;
export const CURSOR_YOURS = 2;
export const CURSOR_BAKED = 4;
export const CURSOR_OFF_STREAM = 0xff;
// 7 is Opus audio, 9 the cursor.
export const KIND_FRAME_ACK = 8;
export const AUDIO_HEADER_LEN = 16;
export const FLAG_KEYFRAME = 1;
export const FLAG_HEVC = 2;
export const FLAG_REFRESH = 4;
export const VIDEO_HEADER_LEN = 20;
export const INPUT_EVENT_LEN = 12;

export const Input = { MOVE: 1, DOWN: 2, UP: 3, SCROLL: 4, KEY_DOWN: 5, KEY_UP: 6, PAD_BUTTON: 7, PAD_AXIS: 8, PAD_CONNECTED: 9, MOVE_BY: 10, PEN: 11, TEXT: 12 };
/** Text bytes in one TEXT event, and the most one piece of text may carry. */
export const TEXT_CHUNK = INPUT_EVENT_LEN - 2;
export const MAX_TEXT_BYTES = 4096;

/** One mic packet: same layout as host audio (kind, channels, seq, capture time, s16 PCM). */
export function micPacket(seq, captureUs, channels, pcm) {
  const buf = new ArrayBuffer(AUDIO_HEADER_LEN + pcm.length * 2);
  const view = new DataView(buf);
  view.setUint8(0, KIND_MIC);
  view.setUint8(1, channels);
  view.setUint32(4, seq, true);
  view.setBigUint64(8, BigInt(Math.max(0, Math.floor(captureUs))), true);
  for (let i = 0; i < pcm.length; i++) view.setInt16(AUDIO_HEADER_LEN + i * 2, pcm[i], true);
  return new Uint8Array(buf);
}

/** The header of an audio datagram (KIND_AUDIO, KIND_AUDIO_OPUS, KIND_MIC). */
export function readAudioHeader(view) {
  return {
    kind: view.getUint8(0),
    channels: view.getUint8(1),
    seq: view.getUint32(4, true),
    captureUs: Number(view.getBigUint64(8, true)),
  };
}

export const STREAM_SHIFT = 4;

/** A frame decoded and drawn: the host paces video by these (flow and bitrate). */
export function frameAck(stream, frameId) {
  const buf = new ArrayBuffer(8);
  const view = new DataView(buf);
  view.setUint8(0, KIND_FRAME_ACK);
  view.setUint8(1, stream);
  view.setUint32(4, frameId, true);
  return new Uint8Array(buf);
}

export function readVideoHeader(view) {
  const flags = view.getUint8(1);
  return {
    flags: flags & 0x0f,
    stream: flags >> STREAM_SHIFT,
    fragIdx: view.getUint16(2, true),
    fragCount: view.getUint16(4, true),
    checksum: view.getUint16(6, true),
    frameId: view.getUint32(8, true),
    captureUs: Number(view.getBigUint64(12, true)),
  };
}

/** Where the host's pointer is (glass_proto::CursorPos): x, y 0..1 of `stream`. */
export function readCursor(view) {
  return {
    stream: view.getUint8(1),
    flags: view.getUint8(2),
    shape: view.getUint32(4, true),
    x: view.getFloat32(8, true),
    y: view.getFloat32(12, true),
  };
}

/** Asks for fragments of a frame again (none: the whole frame). See glass_proto::nack. */
export function nack(stream, frameId, fragments = []) {
  const out = new Uint8Array(8 + fragments.length * 2);
  const v = new DataView(out.buffer);
  v.setUint8(0, KIND_NACK);
  v.setUint8(1, stream);
  v.setUint16(2, fragments.length, true);
  v.setUint32(4, frameId, true);
  fragments.forEach((f, i) => v.setUint16(8 + i * 2, f, true));
  return out;
}

export function ping(nowMs) {
  const buf = new ArrayBuffer(16);
  const view = new DataView(buf);
  view.setUint8(0, KIND_PING);
  view.setFloat64(8, nowMs, true);
  return new Uint8Array(buf);
}

export function readPong(view) {
  return { clientMs: view.getFloat64(8, true), hostUs: Number(view.getBigUint64(16, true)) };
}

export function inputEvent(kind, { button = 0, keycode = 0, a = 0, b = 0 } = {}) {
  const buf = new ArrayBuffer(INPUT_EVENT_LEN);
  const view = new DataView(buf);
  view.setUint8(0, kind);
  view.setUint8(1, button);
  view.setUint16(2, keycode, true);
  view.setFloat32(4, a, true);
  view.setFloat32(8, b, true);
  return new Uint8Array(buf);
}

/**
 * A pen's state (Apple Pencil, stylus) for the mouse events that follow: `near` false when
 * it leaves; pressure 0..1, tilt in degrees (-90..90), twist (barrel rotation) in degrees.
 */
export function penEvent(pen) {
  return inputEvent(Input.PEN, penFields(pen));
}

/** The fields of a pen event, for `inputEvent(Input.PEN, …)`. */
export function penFields({ near, pressure = 0, tiltX = 0, tiltY = 0, twist = 0 }) {
  const t = (v) => Math.max(-90, Math.min(90, Math.round(v))) & 0xff;
  return { button: near ? 1 : 0, keycode: t(tiltX) | (t(tiltY) << 8), a: Math.max(0, Math.min(1, pressure)), b: twist };
}

/**
 * Text for the host to type as is (IME, emoji, accented letters), as TEXT events: chunks of
 * whole characters, in pieces of at most MAX_TEXT_BYTES whose last chunk is marked.
 * Mirrors glass_proto::text_events.
 */
export function textEvents(text) {
  const enc = new TextEncoder();
  const events = [];
  let chunk = [];
  let piece = 0;
  const chars = [...text].map((c) => enc.encode(c));
  chars.forEach((c, i) => {
    chunk.push(...c);
    piece += c.length;
    const next = chars[i + 1]?.length ?? 0;
    const last = !next || piece + next > MAX_TEXT_BYTES;
    if (last || chunk.length + next > TEXT_CHUNK) {
      const ev = new Uint8Array(INPUT_EVENT_LEN);
      ev[0] = Input.TEXT;
      ev[1] = chunk.length | (last ? 0x80 : 0);
      ev.set(chunk, 2);
      events.push(ev);
      chunk = [];
      if (last) piece = 0;
    }
  });
  return events;
}

/** Fletcher-16 (mod 255); mirrors glass_proto::fletcher16. */
export function fletcher16(data) {
  let a = 0;
  let b = 0;
  for (let i = 0; i < data.length; ) {
    const end = Math.min(i + 4096, data.length);
    for (; i < end; i++) {
      a += data[i];
      b += a;
    }
    a %= 255;
    b %= 255;
  }
  return (b << 8) | a;
}
