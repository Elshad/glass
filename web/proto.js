// Mirrors crates/glass-proto. All integers little-endian.

export const KIND_VIDEO = 1;
export const KIND_PING = 2;
export const KIND_PONG = 3;
export const KIND_KEYFRAME_REQUEST = 4;
export const KIND_AUDIO = 5;
export const KIND_MIC = 6;
export const KIND_AUDIO_OPUS = 7; // system audio as one Opus packet, same header as KIND_AUDIO
export const AUDIO_HEADER_LEN = 16;
export const FLAG_KEYFRAME = 1;
export const FLAG_HEVC = 2;
export const FLAG_REFRESH = 4;
export const VIDEO_HEADER_LEN = 20;
export const INPUT_EVENT_LEN = 12;

export const Input = { MOVE: 1, DOWN: 2, UP: 3, SCROLL: 4, KEY_DOWN: 5, KEY_UP: 6, PAD_BUTTON: 7, PAD_AXIS: 8, PAD_CONNECTED: 9, MOVE_BY: 10, PEN: 11 };

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
