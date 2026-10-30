// /dev/dsp in the page: what a program plays reaches the global kernel's
// audio ring (shims/kernel/src/audio.rs), which the page's AudioWorklet plays
// (web/src/audio.js). Under run-node.mjs, with its tap (GEX_AUDIO_TAP), its
// stand-in player (GEX_AUDIO_PLAYER) and loopback (GEX_AUDIO_LOOP):
//   dsp-tone    441 Hz S16 stereo at 44.1 kHz, closed, then 882 Hz switched
//               to F32 mono at 48 kHz: the second stream took the device
//               (rate 48000, four generations) and its newest quarter second
//               is the 882 Hz tone at half amplitude (RMS 0.354);
//   dsp-pace    a second of audio written as fast as the device takes it,
//               with the player at 4x and 0.5x: paced to the player, not the
//               wall clock (about 0.2 s and 1.6 s; the wall clock's 0.8 s);
//   dsp-impulse an impulse's round trip with the 256-frame buffer it asks for:
//               under 20 ms (the old fixed 32 KiB buffer: about 170 ms);
//   dsp-mix     two streams at once: mixed, loopback RMS 0.25 (one: 0.177);
//   dsp-mmap    a mapped DMA buffer, as Quake-engine games use one: 441 Hz at
//               half amplitude reaches the ring (RMS 0.354, 22050 Hz).
// The single-thread flavour has no ring yet.
import { guest, pageLegs } from '../lib/guest.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], timeout: 900 };

/** The tap's `[audio] key value ...` line as an object. */
function tap(stderr) {
  const line = stderr.split('\n').find((l) => l.startsWith('[audio]')) ?? '';
  const f = line.split(/\s+/);
  const v = {};
  for (let i = 1; i + 1 < f.length; i += 2) v[f[i]] = Number(f[i + 1]);
  return { line, v };
}

export default async function (t) {
  const g = guest(t);
  const [leg] = pageLegs(g);
  const cc = (name, libs = []) => g.cc(`${name}.c`, { glibc: true, flags: ['-static'], libs });
  const run = (prog, args, env) => g.run(leg.seed, prog, args, { env: { ...leg.env, ...env }, jspi: true });

  let r = await run(cc('dsp-tone', ['-lm']), [], { GEX_AUDIO_TAP: '1' });
  let { line, v } = tap(r.stderr);
  t.check(`dsp-tone: ${line || 'no tap'}`, r.code === 0 && r.guest.split('\n').includes('dsp-tone ok')
    && v.written > 0 && v.rate === 48000 && v.gen >= 4 && v.rms > 0.32 && v.rms < 0.39 && v.hz > 850 && v.hz < 915, `exit ${r.code}`);

  const pace = cc('dsp-pace');
  for (const [speed, lo, hi] of [['4', 0, 600], ['0.5', 1200, 100000]]) {
    r = await run(pace, [], { GEX_AUDIO_PLAYER: speed });
    const ms = Number(/^dsp-pace ms (\d+)/m.exec(r.guest)?.[1] ?? NaN);
    t.check(`dsp-pace, player at ${speed}x: a second of audio taken in ${ms} ms (want ${lo}-${hi})`, r.code === 0 && ms >= lo && ms < hi, `exit ${r.code}`);
  }

  r = await run(cc('dsp-impulse'), [9, 2], { GEX_AUDIO_PLAYER: '1', GEX_AUDIO_LOOP: '1' });
  const med = Number(/^dsp-impulse rt ms \S+ (\S+) /m.exec(r.guest)?.[1] ?? NaN);
  t.check(`dsp-impulse, 256-frame buffer: median ${med} ms round trip (want under 20)`, r.code === 0 && med > 0 && med < 20, `exit ${r.code}`);

  r = await run(cc('dsp-mix', ['-lm']), [], { GEX_AUDIO_PLAYER: '1', GEX_AUDIO_LOOP: '1' });
  const rms = Number(/^dsp-mix rms (\S+)/m.exec(r.guest)?.[1] ?? NaN);
  t.check(`dsp-mix: two streams mixed, loopback RMS ${rms} (want 0.25)`, r.code === 0 && rms > 0.23 && rms < 0.27, `exit ${r.code}`);

  r = await run(cc('dsp-mmap', ['-lm']), [], { GEX_AUDIO_PLAYER: '1', GEX_AUDIO_TAP: '1' });
  ({ line, v } = tap(r.stderr));
  t.check(`dsp-mmap: ${line || 'no tap'}`, r.code === 0 && r.guest.split('\n').includes('dsp-mmap ok')
    && v.written > 0 && v.rate === 22050 && v.rms > 0.32 && v.rms < 0.39 && v.hz > 420 && v.hz < 460, `exit ${r.code}`);
}
