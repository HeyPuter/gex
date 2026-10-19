// /dev/dsp's output in the page: the global kernel's audio ring
// (shims/kernel/src/audio.rs) played by an AudioWorklet.
//
// The kernel's OSS device is the clock (it drains by the wall clock whether
// or not anything plays), so nothing here paces the guest: the worklet reads
// what was written, resampled from the stream's rate to the context's, and
// plays silence when the ring runs dry. It keeps its own read position and
// never writes the ring: a reader that falls behind by more than the ring
// (a background tab) skips to the newest frames, and one that is far ahead
// of what it needs drops back to a short latency.
//
// Browsers start an AudioContext suspended until the page has a user
// gesture: the first pointer or key event anywhere resumes it.
//
// The single-thread (no SharedArrayBuffer) build plays nothing yet.

const WORKLET = `
const LOW = 0.04, HIGH = 0.3, BACK = 0.08;
class GexDsp extends AudioWorkletProcessor {
  constructor() {
    super();
    this.words = null;
    this.port.onmessage = (e) => {
      const { buffer, at } = e.data;
      this.words = new Uint32Array(buffer, at, 8);
      this.cap = this.words[3];
      this.frames = new Float32Array(buffer, at + 32, this.cap * 2);
      this.ri = null;
    };
    this.fr = 0; this.gen = -1; this.waiting = true;
    this.played = 0; this.under = 0; this.sumsq = 0; this.n = 0; this.cross = 0; this.last = 0; this.tick = 0;
  }
  process(_inputs, outputs) {
    const L = outputs[0][0], R = outputs[0][1] ?? L, n = L.length;
    if (!this.words) return true;
    const w = Atomics.load(this.words, 1);
    const rate = this.words[2] || sampleRate;
    const gen = Atomics.load(this.words, 5);
    if (this.ri === null || gen !== this.gen) { this.gen = gen; this.ri = w; this.fr = 0; this.waiting = true; }
    let avail = (w - this.ri) >>> 0;
    // Lapped (or wrapped past): the newest frames only.
    if (avail > this.cap) { this.ri = (w - Math.floor(BACK * rate)) >>> 0; avail = (w - this.ri) >>> 0; }
    // Far behind the writer: back to a short latency.
    if (avail > HIGH * rate) { this.ri = (w - Math.floor(BACK * rate)) >>> 0; avail = (w - this.ri) >>> 0; this.fr = 0; }
    if (this.waiting && avail < LOW * rate) { L.fill(0); R.fill(0); return true; }
    this.waiting = false;
    const step = rate / sampleRate, f = this.frames, cap = this.cap;
    for (let i = 0; i < n; i++) {
      if (((w - this.ri) >>> 0) < 2) {
        L.fill(0, i); R.fill(0, i); this.under++; this.waiting = true; break;
      }
      const a = (this.ri % cap) * 2, b = ((this.ri + 1) % cap) * 2, t = this.fr;
      const l = f[a] * (1 - t) + f[b] * t, r = f[a + 1] * (1 - t) + f[b + 1] * t;
      L[i] = l; R[i] = r;
      this.sumsq += l * l; this.n++;
      if ((l >= 0) !== (this.last >= 0)) this.cross++;
      this.last = l;
      this.fr += step;
      while (this.fr >= 1) { this.fr -= 1; this.ri = (this.ri + 1) >>> 0; }
    }
    this.played += n;
    // About twice a second: what was played, for the page's checks.
    if (++this.tick % 200 === 0 && this.n) {
      this.port.postMessage({ rms: Math.sqrt(this.sumsq / this.n), hz: this.cross / 2 / (this.n / sampleRate), frames: this.n, underruns: this.under, rate, played: this.played });
      this.sumsq = 0; this.n = 0; this.cross = 0;
    }
    return true;
  }
}
registerProcessor('gex-dsp', GexDsp);
`;

let started = false;

/** Play the ring at `at` in the global kernel's `memory` (a shared
 *  WebAssembly.Memory). Once per page; later calls do nothing. */
export function startAudio({ memory, at }) {
  if (started || !at || !memory || typeof AudioContext !== 'function' || typeof AudioWorkletNode !== 'function') return;
  if (typeof SharedArrayBuffer !== 'function' || !(memory.buffer instanceof SharedArrayBuffer)) return;
  started = true;
  const ctx = new AudioContext({ latencyHint: 'interactive' });
  const info = { ctx, last: null, history: [] };
  globalThis.__gexAudio = info;
  const resume = () => {
    if (ctx.state !== 'running') ctx.resume().catch(() => {});
  };
  for (const ev of ['pointerdown', 'mousedown', 'keydown', 'touchend']) addEventListener(ev, resume, { capture: true, passive: true });
  resume();
  const url = URL.createObjectURL(new Blob([WORKLET], { type: 'text/javascript' }));
  ctx.audioWorklet.addModule(url).then(() => {
    URL.revokeObjectURL(url);
    const node = new AudioWorkletNode(ctx, 'gex-dsp', { numberOfInputs: 0, numberOfOutputs: 1, outputChannelCount: [2] });
    node.port.onmessage = (e) => {
      info.last = e.data;
      info.history.push(e.data);
      if (info.history.length > 64) info.history.shift();
    };
    node.port.postMessage({ buffer: memory.buffer, at });
    node.connect(ctx.destination);
    info.node = node;
  }).catch((err) => console.warn('[gex] audio: the player did not start:', err?.message ?? err));
}
