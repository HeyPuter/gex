// Noita in headless Chromium (docs/nt-layer.md §10.2, milestone (c)): the
// game and an x86 Wine prefix bundled into web/nt.html, its window in the
// page's compositor, its OpenGL on Mesa's gex_wgpu driver over WebGPU in the
// compositor's worker.
//
//   node tests/nt/browser-noita.mjs [--shot DIR] [--minutes N] [--frames N]
//        [--noita DIR] [--keep]
//
// The native recipe's steps (MERGE-STATUS S12): at frame 12 a click at
// (85,661) dismisses the release notes, and the menu is up by ~frame 100.
// Frames are SwapBuffers, counted by GEX_NT_STATS' once-a-second line.
// Screenshots go to DIR every 30 s and at the end (menu.png).
//
// Needs everything browser-gui.mjs does, plus web/public/gex/gex-wgpu-replay.*
// (web/build-mesa-wasm.sh).

import { existsSync } from 'node:fs';
import { join } from 'node:path';
import { bundle, openPage, sleep, log, WEB } from './browser-harness.mjs';

const argv = process.argv.slice(2);
const opt = (name, dflt) => {
  const i = argv.indexOf(name);
  return i >= 0 ? argv[i + 1] : dflt;
};
const shotDir = opt('--shot', null);
const minutes = Number(opt('--minutes', 30));
const wantFrames = Number(opt('--frames', 100));
const noita = opt('--noita', join(process.env.HOME, 'src/noita-wasm/Noita'));
const keep = argv.includes('--keep');

async function main() {
  for (const f of ['gex-wgpu-replay.js', 'gex-wgpu-replay.wasm']) {
    if (!existsSync(join(WEB, 'public/gex', f))) throw new Error(`no web/public/gex/${f}: run web/build-mesa-wasm.sh`);
  }
  const tar = bundle('noita', 'x86', { noita }, [join(noita, 'noita.exe')]);
  const t0 = Date.now();
  const { browser, page, evaluate, state, shot, consoleLines } = await openPage(
    `tar=/gex/nt/${tar.split('/').pop()}&exe=/noita/noita.exe&cwd=/noita&gl=1&glbackend=wgpu`
      + `&size=1282x748&env=GEX_NT_STATS=1`,
    { width: 1300, height: 900, shotDir },
  );
  const presents = () => {
    for (let i = consoleLines.length - 1; i >= 0; i--) {
      const m = /gex-nt: stats t=(\d+) presents=(\d+)/.exec(consoleLines[i]);
      if (m) return { t: Number(m[1]), n: Number(m[2]) };
    }
    return { t: 0, n: 0 };
  };
  const window_ = async () => {
    const d = await evaluate('globalThis.__gexCompositor?.describe()', true);
    const m = (d?.list ?? []).map((l) => /role=1 (\d+)x(\d+)@(-?\d+),(-?\d+).*"(.*)"/.exec(l)).find((x) => x && /Noita/.test(x[5]));
    return m ? { w: +m[1], h: +m[2], x: +m[3], y: +m[4] } : null;
  };
  const click = async (x, y) => {
    for (const type of ['mouseMoved', 'mousePressed', 'mouseReleased']) {
      await browser.send('Input.dispatchMouseEvent', { type, x, y, button: 'left', clickCount: 1, buttons: type === 'mousePressed' ? 1 : 0 }, page);
    }
  };
  const end = t0 + minutes * 60000;
  let clicked = false;
  let nextShot = Date.now() + 30000;
  let shots = 0;
  for (;;) {
    const s = await state();
    if (s?.phase === 'failed' || s?.phase === 'done') {
      log(`the program ended: phase ${s.phase}, status ${s.status}, ${s.error ?? ''}`);
      log(`output: ${s.out.slice(-2000)}`);
      throw new Error(`Noita stopped (${s.phase})`);
    }
    const p = presents();
    if (!clicked && p.n >= 12) {
      const w = await window_();
      if (w) {
        log(`frame ${p.n} at ${(p.t / 1000).toFixed(0)} s: dismissing the release notes (window ${w.w}x${w.h}@${w.x},${w.y})`);
        await click(w.x + 85, w.y + 661);
        clicked = true;
      }
    }
    if (Date.now() >= nextShot) {
      nextShot += 30000;
      await shot(`t${String(++shots).padStart(3, '0')}`);
      log(`${((Date.now() - t0) / 1000).toFixed(0)} s: ${p.n} frames`);
    }
    if (clicked && p.n >= wantFrames) {
      await sleep(3000);
      await shot('menu');
      console.log(`ok   Noita reaches frame ${p.n} (the menu) after ${((Date.now() - t0) / 1000).toFixed(0)} s in the page`);
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
