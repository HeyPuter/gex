// Peggle Deluxe (PopCap, an i386 Windows program) in headless Chromium: the
// game and an x86 Wine prefix bundled into web/nt.html, its Direct3D (Wine's
// wined3d) on OpenGL through opengl32 -> /dev/gex-gl onto Mesa's gex_wgpu
// driver over WebGPU in the compositor's worker.
//
//   node tests/games/browser-peggle.mjs [--peggle DIR] [--shot DIR]
//        [--minutes N] [--measure S] [--no-input] [--keep]
//
// DIR (PEGGLE_DIR, default work/games/peggle) holds a copy of the game as
// shipped; Peggle.exe (PopCap's wrapper) runs unmodified. The prefix is GEX_WINE_PREFIX (tools/nt-stage-wine.sh);
// a prefix without Peggle's userdata starts at the name entry, which is what
// the input script (tests/games/peggle-input.txt) expects.
//
// The input is GEX_NT_INPUT's (frame-timed, in the kernel, the same script
// the native run uses), so a run is the same in both places. It enters a
// name, starts Stage One, dismisses its instructions and shoots a ball.
// Reports the time to the first frame, the time to gameplay, and the frame
// rate (GL presents, GEX_NT_STATS) over --measure seconds after it
// (default 60), with screenshots every 30 s and gameplay.png at the end.

import { existsSync, readFileSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
import { bundle, openPage, sleep, log, WEB, ROOT } from '../nt/browser-harness.mjs';

const argv = process.argv.slice(2);
const opt = (name, dflt) => {
  const i = argv.indexOf(name);
  return i >= 0 ? argv[i + 1] : dflt;
};
const shotDir = opt('--shot', null);
const minutes = Number(opt('--minutes', 40));
const measure = Number(opt('--measure', 60));
const keep = argv.includes('--keep');
const peggle = opt('--peggle', process.env.PEGGLE_DIR || join(ROOT, '../../work/games/peggle'));

const script = readFileSync(join(ROOT, 'tests/games/peggle-input.txt'), 'utf8');
/** The input script, one `frame:action` per line, `#` comments; and the
 * frame gameplay starts at (its `# gameplay N` line). */
const input = argv.includes('--no-input') ? ''
  : script.split('\n').map((l) => l.replace(/#.*/, '').trim()).filter(Boolean).join(';');
const gameplayFrame = Number(/^# gameplay (\d+)/m.exec(script)?.[1] ?? 0);

async function main() {
  for (const f of ['gex-wgpu-replay.js', 'gex-wgpu-replay.wasm']) {
    if (!existsSync(join(WEB, 'public/gex', f))) throw new Error(`no web/public/gex/${f}: run web/build-mesa-wasm.sh`);
  }
  if (!existsSync(join(peggle, 'Peggle.exe'))) throw new Error(`no ${peggle}/Peggle.exe`);
  const tar = bundle('peggle', 'x86', { peggle }, [join(peggle, 'Peggle.exe')]);
  const env = ['GEX_NT_STATS=1', ...(input ? [`GEX_NT_INPUT=${input}`] : []),
    ...(process.env.GEX_NT_PAGE_ENV ? [process.env.GEX_NT_PAGE_ENV] : [])].join(',');
  const t0 = Date.now();
  // The screen is 1920x1080 as natively, so the script's client
  // coordinates are the same.
  const { evaluate, state, shot, consoleLines } = await openPage(
    `tar=/gex/nt/${tar.split('/').pop()}&exe=/peggle/Peggle.exe&cwd=/peggle&gl=1&glbackend=wgpu`
      + `&size=1920x1080&trapstack=1${process.env.GEX_NT_PAGE_FLAGS ?? ''}&env=${encodeURIComponent(env)}`,
    { width: 1920, height: 1080, shotDir },
  );
  const stats = () => {
    for (let i = consoleLines.length - 1; i >= 0; i--) {
      const m = /gex-nt: stats t=(\d+) presents=(\d+)/.exec(consoleLines[i]);
      if (m) return { t: Number(m[1]), n: Number(m[2]) };
    }
    return { t: 0, n: 0 };
  };
  const end = t0 + minutes * 60000;
  let first = null;
  let play = null;
  let shots = 0;
  let nextShot = Date.now() + 30000;
  for (;;) {
    const s = await state();
    if (s?.phase === 'failed' || s?.phase === 'done') {
      log(`output: ${s.out.slice(-2000)}`);
      throw new Error(`Peggle stopped (${s.phase}, status ${s.status}) ${s.error ?? ''}`);
    }
    const threw = consoleLines.find((l) => /\[gex\] pid \d+:\d+ threw:|execute fault|\[harness\] the page crashed/.test(l));
    if (threw) throw new Error(`a thread died: ${threw.slice(0, 600)}`);
    const p = stats();
    if (!first && p.n > 0) {
      first = { ...p, wall: Date.now() };
      console.log(`ok   first frame ${((first.wall - t0) / 1000).toFixed(0)} s after the page opened`);
    }
    if (!play && p.n > 0 && p.n >= gameplayFrame) {
      play = { ...p, wall: Date.now() };
      console.log(`ok   gameplay (frame ${gameplayFrame}) ${((play.wall - t0) / 1000).toFixed(0)} s after the page opened`);
    }
    if (Date.now() >= nextShot) {
      nextShot += 30000;
      await shot(`t${String(++shots).padStart(3, '0')}`);
      log(`${((Date.now() - t0) / 1000).toFixed(0)} s: ${p.n} frames`);
      const gl = (await evaluate('globalThis.__gexCompositor?.describe()', true))?.gl;
      if (gl) log(`compositor gl: ${gl}`);
    }
    if (play && p.t - play.t >= measure * 1000) {
      const secs = (p.t - play.t) / 1000;
      await shot('gameplay');
      console.log(`ok   gameplay: ${((p.n - play.n) / secs).toFixed(1)} frames/s over ${secs.toFixed(0)} s`);
      break;
    }
    if (Date.now() > end) throw new Error(`only ${p.n} frames after ${minutes} min`);
    await sleep(1000);
  }
  if (keep) {
    log('--keep: leaving it running; ^C to stop');
    await new Promise(() => {});
  }
}

main()
  .then(() => process.exit(0))
  .catch((e) => {
    console.log(`FAIL ${e.message}`);
    process.exit(1);
  });
