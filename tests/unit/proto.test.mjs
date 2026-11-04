// The browser's wire format must match crates/glass-proto byte for byte.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import {
  fletcher16, inputEvent, penEvent, Input, INPUT_EVENT_LEN, ping, readPong, readVideoHeader, KIND_PING, KIND_PONG, VIDEO_HEADER_LEN,
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
