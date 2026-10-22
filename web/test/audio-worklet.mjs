// The page's audio player (web/src/audio.js's worklet) starting a stream.
//
//   node web/test/audio-worklet.mjs
//
// Checked, on the worklet's own code over a ring laid out as
// shims/kernel/src/audio.rs lays it out: a stream that keeps adding frames
// waits for the 6 ms start; one whose writer has stopped adding (a device
// buffer shorter than the start, full, its writer waiting for room) starts
// once it has added nothing for as long, so a 128-frame and a 64-frame ASIO
// buffer play; and one that never had two frames does not start.
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';

const src = readFileSync(new URL('../src/audio.js', import.meta.url), 'utf8');
const m = src.match(/const WORKLET = `([\s\S]*?)`;\n/);
assert.ok(m, 'audio.js has its WORKLET');
// The template literal's text, as the page hands it to addModule.
const code = (0, eval)('`' + m[1] + '`');

const RATE = 44100;
let Proc = null;
class AudioWorkletProcessor {
  constructor() {
    this.port = { onmessage: null, postMessage: () => {} };
  }
}
new Function('AudioWorkletProcessor', 'registerProcessor', 'sampleRate', code)(
  AudioWorkletProcessor, (_name, c) => { Proc = c; }, RATE);
assert.ok(Proc, 'the worklet registers its processor');

// The ring: 32 header words, 8 slots of 16 words, then each slot's frames
// and the capture ring.
const HDR = 32, SW = 16, SLOTS = 8, CAP = 16384, CCAP = 16384;
const OFF = (HDR + SLOTS * SW) * 4;

function player() {
  const buffer = new SharedArrayBuffer(OFF + SLOTS * CAP * 8 + CCAP * 8);
  const W = new Uint32Array(buffer, 0, HDR + SLOTS * SW);
  W[2] = SLOTS; W[3] = CAP; W[8] = CCAP;
  const b = HDR; // slot 0
  W[b] = 7; W[b + 1] = RATE; W[b + 2] = 1;
  const p = new Proc();
  p.port.onmessage({ data: { buffer, at: 0, clock: true, loop: false } });
  const L = new Float32Array(128), R = new Float32Array(128);
  // A first quantum takes the slot at its writer's position (nothing yet).
  p.process([], [[L, R]]);
  return {
    write(n) { Atomics.add(W, b + 3, n); },
    quantum() { p.process([], [[L, R]]); },
    read() { return W[b + 4]; },
  };
}

// A writer adding 64 frames each quantum: 6 ms (265 frames) before a read.
{
  const p = player();
  let q = 0;
  for (; q < 20 && p.read() === 0; q++) { p.write(64); p.quantum(); }
  assert.ok(q >= 4, `started after ${q} quanta, ${q * 64} frames written (wanted the 6 ms start)`);
  assert.ok(q * 64 >= 265, `started with ${q * 64} frames written`);
}

// Device buffers of 2 x 64 and 2 x 128 frames, full and their writer waiting
// for room: started once nothing was added for 6 ms (3 quanta of 128).
for (const frames of [128, 256]) {
  const p = player();
  p.write(frames);
  let q = 0;
  for (; q < 20 && p.read() === 0; q++) p.quantum();
  assert.ok(q > 0 && q <= 4, `a full ${frames}-frame buffer was read after ${q} quanta`);
}

// One frame (what a player that ran dry keeps): never read.
{
  const p = player();
  p.write(1);
  for (let q = 0; q < 50; q++) p.quantum();
  assert.equal(p.read(), 0, 'a lone frame is not played');
}

console.log('ok audio-worklet: the 6 ms start, and a stalled short buffer plays');
