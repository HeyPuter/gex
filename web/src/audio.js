// /dev/dsp's output in the page: the global kernel's audio rings
// (shims/kernel/src/audio.rs), mixed and played by an AudioWorklet.
//
// Each stream writing has a slot; the worklet resamples each slot from its
// stream's rate to the context's and sums them. It is the clock while it
// plays: it publishes each slot's read position and a heartbeat each render
// quantum, and the kernel's OSS device counts what a stream wrote and it
// has not read as queued, so the guest writes at the audio hardware's pace
// and keeps only what it chooses queued (a 6 ms start, against 40 ms on the
// wall clock). When it stops (no gesture yet, a background tab) the device
// goes back to the wall clock; a reader that then falls behind skips to the
// newest frames. It also writes the context's rate and latency into the
// ring's header, which the device reports (SNDCTL_DSP_GEX_INFO).
//
// Browsers start an AudioContext suspended until the page has a user
// gesture: the first pointer or key event anywhere resumes it.
//
// The single-thread (no SharedArrayBuffer) build plays nothing yet.
//
// Latency, measured: each report (`__gexAudio.last`) has `backlog` (ms of
// frames in the rings when a quantum starts, the most of any slot: avg,
// min, max over the report), and the context's own `base` and `output`
// latencies (ms). `?audioclock=0` leaves the device on the wall clock, as it
// was, to compare; `?audiolatency=S` gives the context a latency hint of S
// seconds instead of 'interactive'; `?audioloop=1` copies what is played
// into the capture ring, a digital loopback that /dev/dsp reads (the
// software round trip, for a test to measure).

const WORKLET = `
// Layout: shims/kernel/src/audio.rs (version 2).
const HDR = 32, SW = 16, OFF = (HDR + 8 * SW) * 4;
const HIGH = 0.3, BACK = 0.08;
class GexDsp extends AudioWorkletProcessor {
  constructor() {
    super();
    this.words = null;
    this.port.onmessage = (e) => {
      const { buffer, at, clock, loop } = e.data;
      if (e.data.latencyUs !== undefined) { if (this.words) Atomics.store(this.words, 6, e.data.latencyUs); return; }
      // ?audioclock=0: not the device's clock (it keeps the wall clock), and
      // the larger start the drift needs: as before, for measuring.
      this.clock = clock !== false;
      this.low = this.clock ? 0.006 : 0.04;
      this.loop = !!loop;
      this.words = new Uint32Array(buffer, at, HDR + 8 * SW);
      this.nslots = this.words[2]; this.cap = this.words[3]; this.ccap = this.words[8];
      this.slots = [];
      for (let s = 0; s < this.nslots; s++) {
        this.slots.push({ base: HDR + s * SW, f: new Float32Array(buffer, at + OFF + s * this.cap * 8, this.cap * 2), ri: null, fr: 0, gen: -1, waiting: true, lastW: 0, still: 0 });
      }
      this.capture = new Float32Array(buffer, at + OFF + this.nslots * this.cap * 8, this.ccap * 2);
      Atomics.store(this.words, 5, sampleRate);
      Atomics.store(this.words, 9, 128);
    };
    this.played = 0; this.under = 0; this.sumsq = 0; this.n = 0; this.cross = 0; this.last = 0; this.tick = 0;
    // The rings' backlog when a quantum starts (frames written and not yet
    // played, the most of any slot playing): the software part of the
    // output latency, measured.
    this.bsum = 0; this.bmin = Infinity; this.bmax = 0; this.bn = 0; this.active = 0;
  }
  // Add slot \`sl\`'s next \`n\` frames, resampled, into L/R. Whether it played.
  mix(sl, L, R, n) {
    const W = this.words, b = sl.base;
    const w = Atomics.load(W, b + 3);
    const rate = W[b + 1] || sampleRate;
    const gen = Atomics.load(W, b + 2);
    if (sl.ri === null || gen !== sl.gen) { sl.gen = gen; sl.ri = w; sl.fr = 0; sl.waiting = true; sl.lastW = w; sl.still = 0; }
    let avail = (w - sl.ri) >>> 0;
    // Lapped (or wrapped past), or far behind the writer: the newest frames, a short latency.
    if (avail > this.cap || avail > HIGH * rate) { sl.ri = (w - Math.floor(BACK * rate)) >>> 0; avail = (w - sl.ri) >>> 0; sl.fr = 0; }
    // Frames of the context's time since the writer last added any.
    sl.still = w === sl.lastW ? sl.still + n : 0;
    sl.lastW = w;
    // Waiting for the start's worth, unless the writer has added nothing
    // for as long: a device buffer shorter than the start holds all it can
    // (the writer waits for room), and only playing frees it.
    if (sl.waiting && avail < this.low * rate && !(avail >= 2 && sl.still >= this.low * sampleRate)) { Atomics.store(W, b + 4, sl.ri); return false; }
    sl.waiting = false;
    const ms = avail * 1000 / rate;
    if (ms > this.qmax) this.qmax = ms;
    const step = rate / sampleRate, f = sl.f, cap = this.cap;
    for (let i = 0; i < n; i++) {
      if (((w - sl.ri) >>> 0) < 2) {
        // Ran dry: an underrun only while its stream still has the slot.
        if (W[b] !== 0) { this.under++; Atomics.add(W, b + 6, 1); }
        sl.waiting = true; break;
      }
      const a = (sl.ri % cap) * 2, c = ((sl.ri + 1) % cap) * 2, t = sl.fr;
      L[i] += f[a] * (1 - t) + f[c] * t; R[i] += f[a + 1] * (1 - t) + f[c + 1] * t;
      sl.fr += step;
      while (sl.fr >= 1) { sl.fr -= 1; sl.ri = (sl.ri + 1) >>> 0; }
    }
    Atomics.store(W, b + 4, sl.ri);
    return true;
  }
  process(_inputs, outputs) {
    const L = outputs[0][0], R = outputs[0][1] ?? L, n = L.length;
    if (!this.words) return true;
    const W = this.words;
    if (this.clock) Atomics.add(W, 4, 1);
    L.fill(0); if (R !== L) R.fill(0);
    this.qmax = -1;
    let any = 0;
    for (const sl of this.slots) if (this.mix(sl, L, R, n)) any++;
    if (any) {
      this.bsum += this.qmax; this.bn++; if (this.qmax < this.bmin) this.bmin = this.qmax; if (this.qmax > this.bmax) this.bmax = this.qmax;
      for (let i = 0; i < n; i++) {
        const l = L[i];
        this.sumsq += l * l; this.n++;
        if ((l >= 0) !== (this.last >= 0)) this.cross++;
        this.last = l;
      }
      this.played += n;
      this.active = any;
    }
    // ?audioloop=1: what is played goes back in as capture (a digital
    // loopback, for the round trip).
    if (this.loop) {
      let cw = W[7];
      const C = this.capture, cc = this.ccap;
      for (let i = 0; i < n; i++) { const k = (cw % cc) * 2; C[k] = L[i]; C[k + 1] = R[i]; cw = (cw + 1) >>> 0; }
      Atomics.store(W, 7, cw);
    }
    // About twice a second: what was played, for the page's checks.
    if (++this.tick % 200 === 0 && this.n) {
      const backlog = this.bn ? { avg: +(this.bsum / this.bn).toFixed(1), min: +this.bmin.toFixed(1), max: +this.bmax.toFixed(1) } : null;
      this.port.postMessage({ rms: Math.sqrt(this.sumsq / this.n), hz: this.cross / 2 / (this.n / sampleRate), frames: this.n, underruns: this.under, rate: sampleRate, played: this.played, streams: this.active, backlog });
      this.sumsq = 0; this.n = 0; this.cross = 0;
      this.bsum = 0; this.bmin = Infinity; this.bmax = 0; this.bn = 0;
    }
    return true;
  }
}
registerProcessor('gex-dsp', GexDsp);
`;

let started = false;

/** Play the ring at `at` in the global kernel's `memory` (a shared
 *  WebAssembly.Memory). Once per page; later calls do nothing. */
export function startAudio(opts) {
  if (started) return;
  if (playAudio(opts)) started = true;
}

/** Whether audio can play here: an AudioWorklet and shared memory. */
export function canPlayAudio(memory) {
  return typeof AudioContext === 'function' && typeof AudioWorkletNode === 'function'
    && typeof SharedArrayBuffer === 'function' && memory?.buffer instanceof SharedArrayBuffer;
}

/** A player of the ring (`startAudio`): `{ ctx, ready, info, close() }`,
 *  or null where audio cannot play. Every player is its own context; the
 *  rings are read by one at a time. */
export function playAudio({ memory, at, clock = true, latency = 'interactive', loop = false, context = null }) {
  if (!at || !canPlayAudio(memory)) return null;
  // `?audiolatency=S`: the context's latency hint in seconds (the browser
  // picks its nearest; 0 asks for its smallest), for measuring.
  // `context`: one the caller made (in a gesture's handler, so it may run).
  let ctx = context;
  if (!ctx) {
    try {
      ctx = new AudioContext({ latencyHint: latency });
    } catch {
      ctx = new AudioContext({ latencyHint: 'interactive' });
    }
  }
  const info = { ctx, last: null, history: [] };
  globalThis.__gexAudio = info;
  const resume = () => {
    if (ctx.state !== 'running') ctx.resume().catch(() => {});
  };
  const gestures = ['pointerdown', 'mousedown', 'keydown', 'touchend'];
  for (const ev of gestures) addEventListener(ev, resume, { capture: true, passive: true });
  resume();
  const url = URL.createObjectURL(new Blob([WORKLET], { type: 'text/javascript' }));
  info.close = () => {
    for (const ev of gestures) removeEventListener(ev, resume, { capture: true });
    info.node?.disconnect();
    if (globalThis.__gexAudio === info) globalThis.__gexAudio = null;
    return ctx.close().catch(() => {});
  };
  info.ready = ctx.audioWorklet.addModule(url).then(() => {
    URL.revokeObjectURL(url);
    const node = new AudioWorkletNode(ctx, 'gex-dsp', { numberOfInputs: 0, numberOfOutputs: 1, outputChannelCount: [2] });
    node.port.onmessage = (e) => {
      // The browser's part of the latency, as it reports it (s -> ms).
      e.data.base = +(1000 * (ctx.baseLatency ?? 0)).toFixed(1);
      e.data.output = +(1000 * (ctx.outputLatency ?? 0)).toFixed(1);
      // For the device's SNDCTL_DSP_GEX_INFO (an ASIO driver's latencies).
      node.port.postMessage({ latencyUs: Math.round(1000 * (e.data.base + e.data.output)) });
      info.last = e.data;
      info.history.push(e.data);
      if (info.history.length > 64) info.history.shift();
    };
    node.port.postMessage({ buffer: memory.buffer, at, clock, loop });
    node.port.postMessage({ latencyUs: Math.round(1e6 * ((ctx.baseLatency ?? 0) + (ctx.outputLatency ?? 0))) });
    node.connect(ctx.destination);
    info.node = node;
  }, (err) => {
    console.warn('[gex] audio: the player did not start:', err?.message ?? err);
    throw err;
  });
  info.ready.catch(() => {});
  return info;
}
