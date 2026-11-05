// The pointer's on-screen size (web/cursor.js).
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { cursorScale, MIN_CURSOR_PX, MAX_CURSOR_PX } from '../../web/cursor.js';

test('the pointer follows the display scale, kept to 24–128 px', () => {
  // A Retina image (2 px a point) on a display shown at 1.5 CSS px a point.
  assert.equal(cursorScale({ w: 32, h: 48, scale: 2 }, 1.5), 0.75);
  // Shrunk into a small window: never below a normal pointer.
  assert.equal(48 * cursorScale({ w: 32, h: 48, scale: 2 }, 0.1), MIN_CURSOR_PX);
  // Zoomed far in: never past what browsers show.
  assert.equal(48 * cursorScale({ w: 32, h: 48, scale: 1 }, 10), MAX_CURSOR_PX);
});
