// Noita in headless Chromium (docs/nt-layer.md §10.2, milestone (c)): the
// game and an x86 Wine prefix bundled into web/nt.html, its window in the
// page's compositor, its OpenGL on Mesa's gex_wgpu driver over WebGPU in the
// compositor's worker.
//
//   node tests/nt/browser-noita.mjs [--gameplay] [--shot DIR] [--minutes N]
//        [--measure S] [--noita DIR] [--keep]
//
// The native recipe's steps (MERGE-STATUS S12), as CDP clicks at client
// coordinates, each a number of frames (SwapBuffers, GEX_NT_STATS' count)
// after the one before:
//   - the release notes' Ok at (85,661), at frame 12;
//   - "Nah. Destroy it" at (716,389) on the autosave question a prefix that
//     has run gameplay leaves behind (harmless on the title screen when there
//     is none); the page's copy of the save is the only one touched;
// and then the main menu passes the default mode. --gameplay goes on, with
// Noita's -single_threaded_loading, to New Game at (639,348) and the first
// game mode at (445,250), waits for the world (GEX_NT_PEEK=0x122374c*, the
// world's frame counter, starts counting -- the native baseline's scene) and
// reports the world-load time and the steady frame rate over --measure
// seconds (default 300).
//
// Screenshots go to DIR every 30 s and at the end (menu.png, gameplay.png).
// Needs everything browser-gui.mjs does, plus web/public/gex/gex-wgpu-replay.*
// (web/build-mesa-wasm.sh).

import { existsSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
import { bundle, openPage, sleep, log, WEB, summariseProfile } from './browser-harness.mjs';
import { aotCounts, aotLine } from './aot-share.mjs';

const argv = process.argv.slice(2);
const opt = (name, dflt) => {
  const i = argv.indexOf(name);
  return i >= 0 ? argv[i + 1] : dflt;
};
const gameplay = argv.includes('--gameplay');
const shotDir = opt('--shot', null);
const minutes = Number(opt('--minutes', gameplay ? 45 : 30));
const measure = Number(opt('--measure', 300));
const noita = opt('--noita', join(process.env.HOME, 'src/noita-wasm/Noita'));
const keep = argv.includes('--keep');

async function main() {
  for (const f of ['gex-wgpu-replay.js', 'gex-wgpu-replay.wasm']) {
    if (!existsSync(join(WEB, 'public/gex', f))) throw new Error(`no web/public/gex/${f}: run web/build-mesa-wasm.sh`);
  }
  const tar = bundle('noita', 'x86', { noita }, [join(noita, 'noita.exe')]);
  const t0 = Date.now();
  // GEX_NT_PAGE_ENV: more of the page's environment, comma-separated
  // (GEX_NT_PAGE_FLAGS: more query flags, `&name=value`).
  const env = ['GEX_NT_STATS=1', ...(gameplay ? ['GEX_NT_PEEK=0x122374c*'] : []),
    ...(process.env.GEX_NT_PAGE_ENV ? [process.env.GEX_NT_PAGE_ENV] : [])].join(',');
  const { browser, page, evaluate, state, shot, consoleLines, profile } = await openPage(
    `tar=/gex/nt/${tar.split('/').pop()}&exe=/noita/noita.exe&cwd=/noita&gl=1&glbackend=wgpu`
      + `&size=1320x800&trapstack=1${process.env.GEX_NT_PAGE_FLAGS ?? ''}&env=${env}${gameplay ? '&args=-single_threaded_loading' : ''}`,
    { width: 1340, height: 900, shotDir },
  );
  // The newest stats line: ms since boot, frames, and the peeked word.
  const stats = () => {
    for (let i = consoleLines.length - 1; i >= 0; i--) {
      const m = /gex-nt: stats t=(\d+) presents=(\d+)(?: peek=(\d+))?/.exec(consoleLines[i]);
      if (m) return { t: Number(m[1]), n: Number(m[2]), peek: m[3] === undefined ? null : Number(m[3]) };
    }
    return { t: 0, n: 0, peek: null };
  };
  const window_ = async () => {
    const d = await evaluate('globalThis.__gexCompositor?.describe()', true);
    const m = (d?.list ?? []).map((l) => /role=1 (\d+)x(\d+)@(-?\d+),(-?\d+).*"(.*)"/.exec(l)).find((x) => x && /Noita/.test(x[5]));
    return m ? { w: +m[1], h: +m[2], x: +m[3], y: +m[4] } : null;
  };
  // As the native recipe does it: the pointer arrives a few frames before
  // the press, and the release a frame or two after -- Noita's menu buttons
  // act on a hover it has drawn, and a press and release inside one frame
  // is no click to it.
  const click = async (x, y) => {
    const send = (type) => browser.send('Input.dispatchMouseEvent', { type, x, y, button: 'left', clickCount: 1, buttons: type === 'mousePressed' ? 1 : 0 }, page);
    await send('mouseMoved');
    await sleep(1500);
    await send('mousePressed');
    await sleep(600);
    await send('mouseReleased');
  };
  // `after` frames after the previous step.
  const steps = [
    { after: 12, x: 85, y: 661, what: 'dismissing the release notes' },
    { after: 78, x: 716, y: 389, what: 'declining an autosave' },
    ...(gameplay
      ? [
          { after: 50, x: 639, y: 348, what: 'New Game' },
          { after: 80, x: 445, y: 250, what: 'the first game mode' },
        ]
      : []),
  ];
  const end = t0 + minutes * 60000;
  let last = 0;
  let lastClickAt = 0;
  let nextShot = Date.now() + 30000;
  let shots = 0;
  // Gameplay: when the world's counter first moved.
  let world = null;
  const profileSpec = process.env.GEX_NT_PROFILE
    ? (([match, at = '60', secs = '20']) => ({ match, at: +at, secs: +secs }))(process.env.GEX_NT_PROFILE.split(','))
    : null;
  let profiled = false;
  for (;;) {
    const s = await state();
    if (s?.phase === 'failed' || s?.phase === 'done') {
      log(`the program ended: phase ${s.phase}, status ${s.status}, ${s.error ?? ''}`);
      log(`output: ${s.out.slice(-2000)}`);
      throw new Error(`Noita stopped (${s.phase})`);
    }
    // A Worker that threw is a guest thread gone: the run cannot recover.
    const threw = consoleLines.find((l) => /\[gex\] pid \d+:\d+ threw:|execute fault|\[harness\] the page crashed/.test(l));
    if (threw) throw new Error(`a thread died: ${threw.slice(0, 600)}`);
    const p = stats();
    if (steps.length && p.n >= last + steps[0].after) {
      const w = await window_();
      if (w) {
        // The surface is the whole window, so add the frame: Noita's client
        // area is 1280x720, and its frame has equal sides and a caption.
        const side = Math.max(0, (w.w - 1280) / 2);
        const top = Math.max(0, w.h - 720 - side);
        const st = steps.shift();
        log(`frame ${p.n} at ${(p.t / 1000).toFixed(0)} s: ${st.what} (window ${w.w}x${w.h}@${w.x},${w.y}, frame ${side},${top})`);
        await click(w.x + side + st.x, w.y + top + st.y);
        last = p.n;
        lastClickAt = p.t;
      }
    }
    if (Date.now() >= nextShot) {
      nextShot += 30000;
      await shot(`t${String(++shots).padStart(3, '0')}`);
      log(`${((Date.now() - t0) / 1000).toFixed(0)} s: ${p.n} frames${p.peek !== null ? `, world frame ${p.peek}` : ''}`);
      // The compositor's side of the frames: GL submits and their time,
      // readbacks (what SwiftShader took for each frame).
      const gl = (await evaluate('globalThis.__gexCompositor?.describe()', true))?.gl;
      if (gl) log(`compositor gl: ${gl}`);
    }
    if (!steps.length && !gameplay && p.n >= last + 50) {
      await sleep(3000);
      await shot('menu');
      console.log(`ok   Noita reaches frame ${p.n} (the menu) after ${((Date.now() - t0) / 1000).toFixed(0)} s in the page`);
      break;
    }
    if (!steps.length && gameplay) {
      if (!world && p.peek !== null && p.peek > 0) {
        world = { t: p.t, n: p.n, peek: p.peek };
        console.log(`ok   the world is up at ${(p.t / 1000).toFixed(0)} s, ${((p.t - lastClickAt) / 1000).toFixed(0)} s after the game mode was chosen`);
      }
      // GEX_NT_PROFILE=MATCH[,AT[,SECS]]: a CPU profile of the Workers whose
      // script URL contains MATCH, AT seconds into the measurement (default
      // 60) for SECS (default 20), summarised and saved (--shot DIR).
      if (world && profileSpec && !profiled && p.t - world.t >= profileSpec.at * 1000) {
        profiled = true;
        const profs = await profile(profileSpec.match, profileSpec.secs * 1000);
        for (const [k, { url, profile: prof }] of profs.entries()) {
          const file = `${url.split('/').pop().split('?')[0]}.${k}`;
          log(`profile of ${file}:\n${summariseProfile(prof, 30)}`);
          if (shotDir) writeFileSync(join(shotDir, `${file}.cpuprofile`), JSON.stringify(prof));
        }
        // As GEX_NT_STATS' line natively (tests/nt/aot-share.mjs).
        console.log(`ok   ${aotLine(aotCounts(profs.map((x) => x.profile)))}`);
      }
      if (world && p.t - world.t >= measure * 1000) {
        const secs = (p.t - world.t) / 1000;
        const fps = (p.n - world.n) / secs;
        const wfps = (p.peek - world.peek) / secs;
        await shot('gameplay');
        console.log(`ok   gameplay: ${fps.toFixed(1)} presents/s (${(fps * 60).toFixed(0)}/min), world ${wfps.toFixed(1)} frames/s, over ${secs.toFixed(0)} s`);
        break;
      }
    }
    if (Date.now() > end) throw new Error(`only ${p.n} frames after ${minutes} min${p.peek !== null ? ` (world frame ${p.peek})` : ''}`);
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
