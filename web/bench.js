// Bench probe (debug only, for bench/): reads the marker a test host's bench workload
// (GLASS_TEST_BENCH) draws into the top-left of every frame, a frame counter and the host
// clock in ms, from each decoded VideoFrame. Gives unique frames a second and end-to-end
// latency (host draw to viewer draw) on the host clock, via the worker's ping/pong sync.
// Off unless the page sets localStorage `glass.bench` = 1.
// Adapted from voxl (MIT, github.com/tembo/voxl): bench/measure.mjs's marker reader.

// Keep in step with testmode.rs (MARKER_COLS, MARKER_ROWS, MARKER_SCALE, marker_bits).
const COLS = 16;
const ROWS = 3;
const SCALE = 64;
const MS_WRAP = 0x1000000;

export class BenchProbe {
  constructor() {
    this.canvas = undefined;
    this.ctx = undefined;
    this.last = undefined; // last frame counter seen
    this.flush();
  }

  /** One decoded frame, before it's closed. `hostNowUs`: the host clock now, or undefined. */
  read(frame, hostNowUs) {
    this.decoded++;
    const cell = Math.max(4, Math.floor(frame.displayWidth / SCALE));
    const w = cell * COLS;
    const h = cell * ROWS;
    if (!this.ctx || this.canvas.width !== w) {
      this.canvas = new OffscreenCanvas(w, h);
      this.ctx = this.canvas.getContext('2d', { willReadFrequently: true, alpha: false });
    }
    this.ctx.drawImage(frame, cell, cell, w, h, 0, 0, w, h);
    const px = this.ctx.getImageData(0, 0, w, h).data;
    let lo = 0; // bits 0..31
    let hi = 0; // bits 32..47
    for (let bit = 0; bit < COLS * ROWS; bit++) {
      const x = (bit % COLS) * cell + (cell >> 1);
      const y = Math.floor(bit / COLS) * cell + (cell >> 1);
      const i = (y * w + x) * 4;
      if (px[i] + px[i + 1] + px[i + 2] > 382) {
        if (bit < 32) lo |= 1 << bit;
        else hi |= 1 << (bit - 32);
      }
    }
    lo >>>= 0;
    const counter = lo & 0xffff;
    const ms = (lo >>> 16) | ((hi & 0xff) << 16);
    const check = (hi >>> 8) & 0xff;
    const want = (counter ^ (counter >> 8) ^ ms ^ (ms >> 8) ^ (ms >> 16) ^ 0xa5) & 0xff;
    if (check !== want) {
      this.bad++;
      return;
    }
    if (counter === this.last) return; // the same picture again (a repeat or refresh)
    this.last = counter;
    this.unique++;
    if (hostNowUs === undefined) return;
    let lat = (Math.floor(hostNowUs / 1000) % MS_WRAP) - ms;
    if (lat < -MS_WRAP / 2) lat += MS_WRAP;
    // A clock that hasn't settled yet, or a misread that passed the check.
    if (lat > -50 && lat < 10_000) this.lat.push(lat);
  }

  /** What was seen since the last flush: { decoded, unique, bad, lat: [ms…] }. */
  flush() {
    const out = { decoded: this.decoded, unique: this.unique, bad: this.bad, lat: this.lat };
    this.decoded = 0;
    this.unique = 0;
    this.bad = 0;
    this.lat = [];
    return out;
  }
}
