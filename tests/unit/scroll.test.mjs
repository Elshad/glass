import { test } from 'node:test';
import assert from 'node:assert/strict';
import { pointsPerPx, scrollBatcher, wheelPixels } from '../../web/scroll.js';

test('scrolling scales to the desktop shown smaller or larger', () => {
  // A 1512-point Mac on a 390 px phone: about 3.9 points per px.
  assert.equal(pointsPerPx(1512, 378), 4);
  assert.equal(pointsPerPx(1280, 2560), 0.5);
  // Unknown sizes leave deltas as they are.
  assert.equal(pointsPerPx(undefined, 400), 1);
  assert.equal(pointsPerPx(1512, 0), 1);
});

test('wheel lines and pages become pixels', () => {
  assert.deepEqual(wheelPixels({ deltaMode: 0, deltaX: 2, deltaY: -100 }), { dx: 2, dy: -100 });
  assert.deepEqual(wheelPixels({ deltaMode: 1, deltaX: 0, deltaY: 3 }), { dx: 0, dy: 48 });
  assert.deepEqual(wheelPixels({ deltaMode: 2, deltaX: 0, deltaY: 1 }), { dx: 0, dy: 400 });
});

test('scrolls in one frame go as one, and fractions wait for more', () => {
  const sent = [];
  const frames = [];
  const s = scrollBatcher((dx, dy) => sent.push([dx, dy]), (f) => frames.push(f));
  s.add(1, 2);
  s.add(0.5, 3);
  assert.equal(frames.length, 1, 'one frame scheduled');
  frames.shift()();
  assert.deepEqual(sent, [[1.5, 5]]);
  s.add(0.2, 0);
  frames.shift()();
  assert.equal(sent.length, 1, 'too little to send');
  s.add(0.2, 0);
  frames.shift()();
  assert.equal(sent.length, 1);
  s.add(0.2, 0);
  frames.shift()();
  assert.ok(Math.abs(sent[1][0] - 0.6) < 1e-9);
});

test('flush sends what is waiting before a click', () => {
  const sent = [];
  const frames = [];
  const s = scrollBatcher((dx, dy) => sent.push([dx, dy]), (f) => frames.push(f));
  s.add(0, 10);
  s.flush();
  assert.deepEqual(sent, [[0, 10]]);
  frames.shift()(); // the frame that was scheduled finds nothing to send
  assert.equal(sent.length, 1);
});
