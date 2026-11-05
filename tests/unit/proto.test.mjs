// The browser's wire format must match crates/glass-proto byte for byte.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import {
  fletcher16, inputEvent, penEvent, textEvents, Input, INPUT_EVENT_LEN, MAX_TEXT_BYTES, TEXT_CHUNK, ping, readPong, readVideoHeader, KIND_PING, KIND_PONG, VIDEO_HEADER_LEN, readCursor, KIND_CURSOR, CURSOR_VISIBLE, CURSOR_YOURS, frameAck, KIND_FRAME_ACK, nack,
} from '../../web/proto.js';

test('fletcher16 matches the Rust implementation', () => {
  const enc = new TextEncoder();
  assert.equal(fletcher16(enc.encode('abcde')), 0xc8f0);
  assert.equal(fletcher16(enc.encode('abcdef')), 0x2057);
  assert.equal(fletcher16(new Uint8Array(0)), 0);
  // Long inputs cross the 4096-byte reduction boundary.
  const big = new Uint8Array(10_000).fill(255);
  assert.ok(fletcher16(big) >= 0 && fletcher16(big) <= 0xffff);
});

test('input events are 12 bytes, little-endian', () => {
  const e = inputEvent(Input.MOVE, { button: 1, a: 0.25, b: 0.75 });
  assert.equal(e.length, INPUT_EVENT_LEN);
  const v = new DataView(e.buffer);
  assert.equal(v.getUint8(0), 1);
  assert.equal(v.getUint8(1), 1);
  assert.equal(v.getFloat32(4, true), 0.25);
  assert.equal(v.getFloat32(8, true), 0.75);
  const k = new DataView(inputEvent(Input.KEY_DOWN, { keycode: 0x37 }).buffer);
  assert.equal(k.getUint8(0), 5);
  assert.equal(k.getUint16(2, true), 0x37);
});

test('video header parses kind, stream nibble, ids and checksum', () => {
  const buf = new Uint8Array(VIDEO_HEADER_LEN);
  const v = new DataView(buf.buffer);
  v.setUint8(0, 1);
  v.setUint8(1, (2 << 4) | 1); // stream 2, keyframe
  v.setUint16(2, 3, true);
  v.setUint16(4, 9, true);
  v.setUint16(6, 0xbeef, true);
  v.setUint32(8, 77, true);
  v.setBigUint64(12, 123456789n, true);
  assert.deepEqual(readVideoHeader(v), {
    flags: 1, stream: 2, fragIdx: 3, fragCount: 9, checksum: 0xbeef, frameId: 77, captureUs: 123456789,
  });
});

test('ping / pong round trip', () => {
  const p = ping(1234.5);
  assert.equal(p[0], KIND_PING);
  const pong = new Uint8Array(24);
  pong[0] = KIND_PONG;
  pong.set(p.subarray(8, 16), 8);
  new DataView(pong.buffer).setBigUint64(16, 99n, true);
  assert.deepEqual(readPong(new DataView(pong.buffer)), { clientMs: 1234.5, hostUs: 99 });
});

test('mic packets match the host audio layout', async () => {
  const { micPacket, KIND_MIC, AUDIO_HEADER_LEN } = await import('../../web/proto.js');
  const p = micPacket(9, 77.9, 1, new Int16Array([1, -2, 300]));
  assert.equal(p.length, AUDIO_HEADER_LEN + 6);
  const v = new DataView(p.buffer);
  assert.equal(v.getUint8(0), KIND_MIC);
  assert.equal(v.getUint8(1), 1);
  assert.equal(v.getUint32(4, true), 9);
  assert.equal(v.getBigUint64(8, true), 77n);
  assert.deepEqual([0, 1, 2].map((i) => v.getInt16(AUDIO_HEADER_LEN + i * 2, true)), [1, -2, 300]);
  // A full 5 ms packet fits one datagram.
  assert.ok(micPacket(0, 0, 1, new Int16Array(240)).length <= 1200);
});

test('opus audio datagrams match glass_proto::opus_packet', async () => {
  const { readAudioHeader, KIND_AUDIO_OPUS, KIND_AUDIO, KIND_MIC, AUDIO_HEADER_LEN } = await import('../../web/proto.js');
  assert.equal(KIND_AUDIO_OPUS, 7);
  assert.equal(new Set([KIND_AUDIO, KIND_MIC, KIND_AUDIO_OPUS, KIND_PING, KIND_PONG]).size, 5);
  // Same bytes as the Rust test opus_packet_layout.
  const p = new Uint8Array([7, 2, 0, 0, 0xd4, 0xc3, 0xb2, 0xa1, 0x87, 0xd6, 0x12, 0, 0, 0, 0, 0, 0xfc, 1, 2, 3]);
  assert.deepEqual(readAudioHeader(new DataView(p.buffer)), { kind: KIND_AUDIO_OPUS, channels: 2, seq: 0xa1b2c3d4, captureUs: 1234567 });
  assert.deepEqual([...p.subarray(AUDIO_HEADER_LEN)], [0xfc, 1, 2, 3]);
});

test('gamepad events carry pad, index and value', () => {
  const e = inputEvent(Input.PAD_AXIS, { button: 2, keycode: 3, a: -0.5 });
  const v = new DataView(e.buffer);
  assert.deepEqual([v.getUint8(0), v.getUint8(1), v.getUint16(2, true), v.getFloat32(4, true)], [8, 2, 3, -0.5]);
});

test('pen events match glass_proto::InputEvent::Pen byte for byte', () => {
  const e = penEvent({ near: true, pressure: 0.5, tiltX: -45, tiltY: 30, twist: 270 });
  // Same bytes as the Rust test pen_wire_layout.
  assert.deepEqual([...e.slice(0, 4)], [11, 1, 0xd3, 0x1e]);
  const v = new DataView(e.buffer);
  assert.equal(v.getFloat32(4, true), 0.5);
  assert.equal(v.getFloat32(8, true), 270);
  // Out of range is clamped, not wrapped.
  const c = penEvent({ near: false, pressure: 3, tiltX: 200, tiltY: -200 });
  assert.deepEqual([...c.slice(0, 4)], [11, 0, 90, 0xa6]);
  assert.equal(new DataView(c.buffer).getFloat32(4, true), 1);
});

test('text events match glass_proto::text_events', () => {
  const [e] = textEvents('é!');
  // Same bytes as text_wire_layout in crates/glass-proto.
  assert.deepEqual([...e.slice(0, 5)], [12, 0x80 | 3, 0xc3, 0xa9, 0x21]);
  assert.ok(e.slice(5).every((b) => b === 0));
  assert.equal(e.length, INPUT_EVENT_LEN);
  assert.deepEqual(textEvents(''), []);
});

test('text events hold whole characters and put back together', () => {
  const dec = new TextDecoder('utf-8', { fatal: true });
  const join = (events) => {
    const pieces = [];
    let bytes = [];
    for (const e of events) {
      assert.equal(e[0], Input.TEXT);
      const len = e[1] & 0x7f;
      assert.ok(len <= TEXT_CHUNK);
      dec.decode(e.slice(2, 2 + len)); // throws if a character was split
      bytes.push(...e.slice(2, 2 + len));
      if (e[1] & 0x80) { pieces.push(dec.decode(new Uint8Array(bytes))); bytes = []; }
    }
    return pieces;
  };
  for (const t of ['日本語の入力', '👍🏽 ok', '한국어', 'café', '0123456789']) assert.deepEqual(join(textEvents(t)), [t]);
  assert.equal(textEvents('😀😀😀').length, 2);
  const long = 'é'.repeat(MAX_TEXT_BYTES);
  const pieces = join(textEvents(long));
  assert.ok(pieces.length >= 2 && pieces.every((p) => new TextEncoder().encode(p).length <= MAX_TEXT_BYTES));
  assert.equal(pieces.join(''), long);
});

test('cursor positions match glass_proto::CursorPos byte for byte', () => {
  // Same bytes as the Rust test cursor_roundtrip_and_layout.
  const b = new Uint8Array(16);
  b.set([KIND_CURSOR, 1, CURSOR_VISIBLE | CURSOR_YOURS, 0, 7, 0, 0, 0]);
  const v = new DataView(b.buffer);
  v.setFloat32(8, 0.25, true);
  v.setFloat32(12, 0.5, true);
  assert.deepEqual(readCursor(v), { stream: 1, flags: 3, shape: 7, x: 0.25, y: 0.5 });
});

test('frame acks are kind 8, stream, two spare bytes, then the frame id', () => {
  // Same bytes as glass_proto::frame_ack(2, 0xdeadbeef).
  assert.equal(KIND_FRAME_ACK, 8);
  assert.deepEqual([...frameAck(2, 0xdeadbeef)], [8, 2, 0, 0, 0xef, 0xbe, 0xad, 0xde]);
});

test('nacks match glass_proto::nack byte for byte', () => {
  // Same bytes as nack_layout in crates/glass-proto.
  assert.deepEqual([...nack(2, 0xa1b2c3d4, [1, 0x0203])], [10, 2, 2, 0, 0xd4, 0xc3, 0xb2, 0xa1, 1, 0, 3, 2]);
  assert.deepEqual([...nack(0, 7)], [10, 0, 0, 0, 7, 0, 0, 0]);
});
