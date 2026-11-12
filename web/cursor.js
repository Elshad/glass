// The host's pointer, drawn here instead of in the video: a CSS cursor over the display it's
// on, so it moves with this device's mouse at once rather than a round trip later. When
// something else moves it (another viewer, an agent, the person at the computer), or this
// device has no mouse over it (touch, pointer lock), it's drawn where the host says it is.
// See crates/glass-host/src/cursor.rs.
//
// Adapted from voxl (MIT, github.com/tembo/voxl): web/src/client/cursor.ts.

import { CURSOR_BAKED, CURSOR_VISIBLE, CURSOR_YOURS } from './proto.js';

// On-screen size bounds in CSS pixels, for the image's longer side: a desktop scaled down
// into a small window keeps a normal-sized pointer (24 is the usual one), and browsers
// won't show cursor images much past 128.
export const MIN_CURSOR_PX = 24;
export const MAX_CURSOR_PX = 128;

/**
 * CSS pixels per image pixel: the display's scale on screen (`cssPerPoint`; the image has
 * `scale` pixels to a point), its longer side kept within 24–128 px.
 */
export function cursorScale({ w, h, scale }, cssPerPoint) {
  const size = Math.max(w, h, 1);
  return Math.min(Math.max(cssPerPoint / (scale || 1), MIN_CURSOR_PX / size), MAX_CURSOR_PX / size);
}

// A sharp image on high-density screens, where the browser takes image-set() for cursors.
const IMAGE_SET = typeof CSS === 'undefined'
  ? undefined
  : ['image-set', '-webkit-image-set'].find((f) => CSS.supports?.('cursor', `${f}(url("data:,") 2x) 0 0, default`));

/**
 * @param screens     the element holding the displays ([data-stream] canvases or videos)
 * @param overlay     an element to draw the pointer with when it isn't this device's mouse
 * @param contentRect (element) => the picture's rect in client pixels (letterboxed), or undefined
 * @param onWanted    (on) => void: whether to ask the host to leave the pointer out of the video
 */
export function createCursor({ screens, overlay, contentRect, onWanted }) {
  const shapes = new Map(); // id -> { w, h, hx, hy, scale, src, urls }
  let streams = []; // DisplayInfo per stream: its size in points
  let pos; // the latest from the host
  let hostSupports = false;
  let wanted = true; // not trackpad mode (it has its own pointer)
  let localOver; // the display this device's mouse (or pen) is over
  let locked = false; // pointer lock: the local position means nothing
  let both = false; // this device's own arrow stays, and the host's is drawn where it is
  const applied = new WeakMap(); // element -> the cursor value it has
  let overlayKey = '';
  let frame;

  const schedule = () => {
    if (!frame) frame = requestAnimationFrame(apply);
  };

  function urlFor(shape, s) {
    const dpr = IMAGE_SET ? Math.min(3, window.devicePixelRatio || 1) : 1;
    const cw = Math.max(1, Math.round(shape.w * s));
    const ch = Math.max(1, Math.round(shape.h * s));
    const key = `${cw}x${ch}@${dpr}`;
    let u = shape.urls.get(key);
    if (u) return u;
    const c = document.createElement('canvas');
    c.width = Math.max(1, Math.round(cw * dpr));
    c.height = Math.max(1, Math.round(ch * dpr));
    const cx = c.getContext('2d');
    cx.imageSmoothingQuality = 'high';
    cx.drawImage(shape.src, 0, 0, c.width, c.height);
    const url = c.toDataURL('image/png');
    const hx = Math.min(cw - 1, Math.round(shape.hx * s));
    const hy = Math.min(ch - 1, Math.round(shape.hy * s));
    const image = dpr > 1 ? `${IMAGE_SET}(url("${url}") ${dpr}x)` : `url("${url}")`;
    u = { url, cw, ch, hx, hy, css: `${image} ${hx} ${hy}, default` };
    if (shape.urls.size >= 8) shape.urls.clear();
    shape.urls.set(key, u);
    return u;
  }

  function apply() {
    frame = undefined;
    const p = pos;
    const active = hostSupports && wanted && p && !(p.flags & CURSOR_BAKED);
    const shape = active ? shapes.get(p.shape) : undefined;
    let drawn; // { rect, u } when the overlay shows
    for (const el of screens.querySelectorAll('[data-stream]')) {
      // '' is the stylesheet's `none`: the pointer is in the video.
      let value = '';
      if (active) {
        const info = streams[Number(el.dataset.stream)];
        const rect = Number(el.dataset.stream) === p.stream ? contentRect(el) : undefined;
        if (!rect || !info) {
          // Not where the pointer is: an ordinary arrow until it gets here.
          value = 'default';
        } else if (!(p.flags & CURSOR_VISIBLE)) {
          value = 'none';
        } else if (shape) {
          const u = urlFor(shape, cursorScale(shape, rect.width / Math.max(1, info.points_width)));
          value = u.css;
          if (both || !(p.flags & CURSOR_YOURS) || localOver !== el || locked) drawn = { rect, u };
        } else {
          value = 'default';
        }
        // Both pointers: this device's own arrow as usual; the host's is the overlay.
        if (both) value = 'default';
      }
      if (applied.get(el) !== value) {
        el.style.cursor = value;
        applied.set(el, value);
      }
    }
    if (!drawn) {
      if (overlayKey) {
        overlay.classList.add('hidden');
        overlayKey = '';
      }
      return;
    }
    const { rect, u } = drawn;
    const x = Math.round(rect.left + p.x * rect.width - u.hx);
    const y = Math.round(rect.top + p.y * rect.height - u.hy);
    const key = `${u.url.length}|${u.cw}|${u.ch}|${x}|${y}`;
    if (key === overlayKey) return;
    if (!overlayKey) overlay.classList.remove('hidden');
    if (overlay.dataset.url !== u.url) {
      overlay.dataset.url = u.url;
      overlay.style.backgroundImage = `url("${u.url}")`;
    }
    overlay.style.width = `${u.cw}px`;
    overlay.style.height = `${u.ch}px`;
    overlay.style.transform = `translate(${x}px, ${y}px)`;
    overlayKey = key;
  }

  // Which display this device's own pointer is over (a finger isn't a pointer to draw).
  const track = (e) => {
    const over = e.pointerType === 'touch' ? undefined : e.target?.closest?.('#screens > [data-stream]') ?? undefined;
    if (over !== localOver) {
      localOver = over;
      schedule();
    }
  };
  screens.addEventListener('pointerover', track, { passive: true });
  screens.addEventListener('pointermove', track, { passive: true });
  screens.addEventListener('pointerdown', track, { passive: true });
  screens.addEventListener('pointerleave', () => {
    localOver = undefined;
    schedule();
  });
  addEventListener('resize', schedule);
  document.addEventListener('pointerlockchange', () => {
    locked = Boolean(document.pointerLockElement && screens.contains(document.pointerLockElement));
    schedule();
  });

  return {
    /** A `cursor` message: the pointer's image. */
    setShape(msg) {
      const bytes = Uint8ClampedArray.from(atob(msg.rgba), (c) => c.charCodeAt(0));
      if (!msg.w || !msg.h || bytes.length < msg.w * msg.h * 4) return;
      const src = document.createElement('canvas');
      src.width = msg.w;
      src.height = msg.h;
      src.getContext('2d').putImageData(new ImageData(bytes.subarray(0, msg.w * msg.h * 4), msg.w, msg.h), 0, 0);
      shapes.set(msg.id, { w: msg.w, h: msg.h, hx: msg.hx, hy: msg.hy, scale: msg.scale, src, urls: new Map() });
      // Keep the last few: a datagram can still name the one before.
      for (const id of shapes.keys()) if (shapes.size > 4) shapes.delete(id);
      schedule();
    },
    /** A KIND_CURSOR datagram (proto.js readCursor). */
    setPosition(p) {
      pos = p;
      schedule();
    },
    setStreams(list) {
      streams = list ?? [];
      schedule();
    },
    setHostSupports(on) {
      hostSupports = on;
      schedule();
    },
    /** Off in trackpad mode: it draws its own pointer, and the host puts its back in the video. */
    setWanted(on) {
      wanted = on;
      onWanted(on);
      schedule();
    },
    /**
     * Both pointers: this device's own stays as it is, and the host's is drawn where the
     * host says it is (a round trip behind), so the two can be told apart.
     */
    setBoth(on) {
      both = on;
      schedule();
    },
    /** A new session: image ids start over. */
    reset() {
      shapes.clear();
      pos = undefined;
      schedule();
    },
    /** The displays moved or resized (zoom, layout). */
    refresh: schedule,
  };
}
