// Plays 48 kHz audio posted by worker.js through this node's port: s16 interleaved PCM
// packets, or float planes (Opus, decoded by WebCodecs).
// Keeps a small jitter buffer: starts once TARGET is queued, trims back to TARGET if the
// queue grows past MAX (so latency can't creep up), and re-primes after an underrun.

const SOURCE_RATE = 48000;
const TARGET_MS = 40;
const MAX_MS = 150;
const CAPACITY = SOURCE_RATE; // 1 s per channel

class GlassAudio extends AudioWorkletProcessor {
  constructor() {
    super();
    this.channels = [new Float32Array(CAPACITY), new Float32Array(CAPACITY)];
    this.read = 0; // fractional read position, in source frames
    this.write = 0;
    this.playing = false;
    this.step = SOURCE_RATE / sampleRate; // source frames per output frame
    this.underruns = 0;
    this.port.onmessage = ({ data }) => {
      if (data.planes) this.pushPlanes(data.planes);
      else this.push(new Int16Array(data.pcm), data.channels);
    };
  }

  queued() {
    return this.write - this.read;
  }

  push(pcm, channels) {
    const frames = pcm.length / channels;
    for (let f = 0; f < frames; f++) {
      const i = (this.write + f) % CAPACITY;
      const left = pcm[f * channels] / 32768;
      this.channels[0][i] = left;
      this.channels[1][i] = channels > 1 ? pcm[f * channels + 1] / 32768 : left;
    }
    this.wrote(frames);
  }

  pushPlanes([left, right = left]) {
    for (let f = 0; f < left.length; f++) {
      const i = (this.write + f) % CAPACITY;
      this.channels[0][i] = left[f];
      this.channels[1][i] = right[f];
    }
    this.wrote(left.length);
  }

  wrote(frames) {
    this.write += frames;
    const max = (MAX_MS / 1000) * SOURCE_RATE;
    if (this.queued() > max) this.read = this.write - (TARGET_MS / 1000) * SOURCE_RATE;
  }

  process(_inputs, outputs) {
    const out = outputs[0];
    const n = out[0].length;
    if (!this.playing) {
      if (this.queued() < (TARGET_MS / 1000) * SOURCE_RATE) return true;
      this.playing = true;
    }
    for (let i = 0; i < n; i++) {
      if (this.queued() < 2) {
        // Underrun: output silence and wait for the buffer to refill.
        this.playing = false;
        this.underruns++;
        this.port.postMessage({ underruns: this.underruns });
        break;
      }
      // Linear interpolation handles output rates other than 48 kHz.
      const pos = Math.floor(this.read);
      const frac = this.read - pos;
      for (let c = 0; c < out.length; c++) {
        const src = this.channels[Math.min(c, 1)];
        const a = src[pos % CAPACITY];
        const b = src[(pos + 1) % CAPACITY];
        out[c][i] = a + (b - a) * frac;
      }
      this.read += this.step;
    }
    return true;
  }
}

registerProcessor('glass-audio', GlassAudio);
