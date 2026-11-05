// Scrolling in the remote desktop's own units, batched per frame.
// Adapted from voxl (MIT, github.com/tembo/voxl): web/src/client/pointer.ts.

/** A DOM wheel event's deltas in CSS px. */
export function wheelPixels(e) {
  const unit = e.deltaMode === 1 ? 16 : e.deltaMode === 2 ? 400 : 1; // DOM_DELTA_LINE, _PAGE
  return { dx: e.deltaX * unit, dy: e.deltaY * unit };
}

/**
 * Desktop points per CSS px for a display `pointsWidth` points wide shown `cssWidth` CSS px
 * wide: a phone showing a Mac at a quarter of its size scrolls 4 points per px, so the
 * content keeps up with the fingers. 1 when either is unknown.
 */
export function pointsPerPx(pointsWidth, cssWidth) {
  return pointsWidth > 0 && cssWidth > 0 ? pointsWidth / cssWidth : 1;
}

/**
 * Adds up scrolling and sends it once per frame (`schedule`, requestAnimationFrame), so a
 * 120 Hz trackpad doesn't send twice what the host can draw. `flush` sends what's waiting
 * now, for events that must come after it (a click).
 */
export function scrollBatcher(send, schedule = (f) => requestAnimationFrame(f)) {
  let dx = 0;
  let dy = 0;
  let queued = false;
  function flush() {
    queued = false;
    // Too little to move anything: keep it for the next frame.
    if (Math.abs(dx) < 0.5 && Math.abs(dy) < 0.5) return;
    send(dx, dy);
    dx = 0;
    dy = 0;
  }
  return {
    add(x, y) {
      dx += x;
      dy += y;
      if (queued) return;
      queued = true;
      schedule(() => { if (queued) flush(); });
    },
    flush,
  };
}
