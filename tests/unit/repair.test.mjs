// A lost fragment is asked for again; frames behind it wait instead of the whole keyframe
// being thrown away and requested anew (web/video-stream.js).
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { VideoStream } from '../../web/video-stream.js';
import { FLAG_KEYFRAME, VIDEO_HEADER_LEN } from '../../web/proto.js';

function setup() {
  const sent = { nacks: [], keyframes: 0, decoded: [] };
  const s = new VideoStream(0, {
    log() {}, post() {}, stats: { bytes: 0, dropped: 0 }, hostNowUs: () => 0,
    requestKeyframe: () => { sent.keyframes++; },
    nackOn: () => true,
    nack: (_, id, fragments) => sent.nacks.push([id, fragments]),
  });
  // Decoding needs a browser: note the order frames reach it, keep the bookkeeping.
  s.decodeFrame = (id, frame) => {
    sent.decoded.push(id);
    s.waitingForKeyframe = false;
    s.lastDecodedId = id;
  };
  return { s, sent };
}

const frag = (s, frameId, fragIdx, fragCount, keyframe = false) => s.onFragment(
  { frameId, fragIdx, fragCount, flags: keyframe ? FLAG_KEYFRAME : 0, captureUs: frameId, checksum: 0 },
  new Uint8Array(VIDEO_HEADER_LEN + 1),
);
const pause = (ms) => new Promise((r) => setTimeout(r, ms));

test('a keyframe missing a fragment is repaired, and later frames wait for it', async () => {
  const { s, sent } = setup();
  frag(s, 1, 0, 3, true);
  frag(s, 1, 2, 3, true); // fragment 1 lost
  frag(s, 2, 0, 1); // a newer frame must not throw the keyframe away
  await pause(70);
  s.repair(20);
  assert.deepEqual(sent.nacks, [[1, [1]]]);
  assert.equal(sent.keyframes, 0, 'no new keyframe while this one can still be repaired');
  assert.deepEqual(sent.decoded, []);
  frag(s, 1, 1, 3, true);
  assert.deepEqual(sent.decoded, [1, 2], 'in order once it arrives');
  frag(s, 1, 1, 3, true); // a late duplicate
  assert.deepEqual(sent.decoded, [1, 2]);
  assert.equal(s.pending.size, 0);
});

test('a delta lost altogether is asked for whole; a host without it means a new keyframe', async () => {
  const { s, sent } = setup();
  frag(s, 1, 0, 1, true);
  frag(s, 3, 0, 1); // 2 never came
  s.repair(20);
  assert.deepEqual(sent.nacks, [[2, []]]);
  assert.deepEqual(sent.decoded, [1]);
  frag(s, 2, 0, 1);
  assert.deepEqual(sent.decoded, [1, 2, 3]);
  frag(s, 5, 0, 1); // 4 never came, and the host no longer has it
  s.repair(20);
  s.frameGone(4);
  assert.equal(sent.keyframes, 1);
  frag(s, 6, 0, 2, true);
  frag(s, 6, 1, 2, true);
  assert.deepEqual(sent.decoded, [1, 2, 3, 6]);
});

test('a stalled gap gives up after a while, and asking for keyframes backs off', async () => {
  const { s, sent } = setup();
  frag(s, 1, 0, 1, true);
  frag(s, 3, 0, 1);
  s.repair(1);
  s.gapSince = performance.now() - 1000;
  s.repair(1);
  assert.equal(sent.keyframes, 1);
  assert.equal(s.waitingForKeyframe, true);
  assert.equal(s.keyframeRetry, 500, 'the next request waits longer');
  s.requestKeyframe();
  assert.equal(sent.keyframes, 1, 'too soon');
});

test('a complete keyframe restarts the chain past a gap', () => {
  const { s, sent } = setup();
  frag(s, 1, 0, 1, true);
  frag(s, 4, 0, 1, true);
  assert.deepEqual(sent.decoded, [1, 4]);
  frag(s, 2, 0, 1); // too late to matter
  assert.deepEqual(sent.decoded, [1, 4]);
  assert.equal(s.pending.size, 0);
});

test('with nothing at all arriving, it asks for a keyframe after a moment', () => {
  const { s, sent } = setup();
  s.repair(20);
  assert.equal(sent.keyframes, 0, 'too early: one is on its way');
  s.waitingSince = performance.now() - 2000;
  s.repair(20);
  assert.equal(sent.keyframes, 1);
});

test('a keyframe the host can no longer send is replaced at once', () => {
  const { s, sent } = setup();
  frag(s, 1, 0, 3, true);
  s.frameGone(1);
  assert.equal(s.pending.size, 0);
  assert.equal(sent.keyframes, 1);
  frag(s, 2, 0, 1, true);
  assert.deepEqual(sent.decoded, [2]);
});
