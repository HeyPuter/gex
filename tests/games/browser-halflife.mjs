// Half-Life (GoldSrc, an i386 Linux program) in headless Chromium: the game,
// its libraries and gex's guest GL and Steam libraries bundled into
// web/nt.html, SDL's window on the page's Wayland compositor, its OpenGL
// through gex-gl's generated encoder onto Mesa's gex_wgpu driver over
// WebGPU, and the X display its CEF (chromehtml.so, GTK2) insists on from
// the page's own X server (Xwayland as wasm, `hostx=1`).
//
//   node tests/games/browser-halflife.mjs [--map c1a0] [--shot DIR]
//        [--minutes N] [--measure S] [--keep] [--hl-args ARGS]
//        [--overlay DIR --exe PATH [--exe-args ARGS] --want RE]
//
// --overlay: a directory bundled at / over the game's bundle and --exe run
// from it instead of hl_linux, with the game's libraries and environment
// (tests/games/src/sdl-fbo-blit.c: Half-Life's present path alone); --want
// is what its output must show.
//
// Needs: the Steam install (HL_DIR, default ~/.steam/steam/steamapps/common/
// Half-Life), guest/gl built for i386 (GEX_GL_I386, guest/gl/build.sh with
// GEX_CC="gcc -m32"), gex-steam for i386 (GEX_STEAM_I386, guest/steam/
// build.sh), the Steam runtime's GTK2 libraries CEF loads (GEX_HL_FALLBACK),
// the host's i386 glibc and friends (tests/games/halflife-system-files.txt),
// web/public/gex/gex-wgpu-replay.* (web/build-mesa-wasm.sh) and
// web/public/gex/xwayland/ (web/build-xwayland-wasm.sh).
//
// Reports when the map's first frame is up and the frame rate over
// --measure seconds (default 60), with screenshots (map.png at the end).

import { existsSync, mkdirSync, readFileSync, rmSync, statSync, symlinkSync, writeFileSync } from 'node:fs';
import { execFileSync } from 'node:child_process';
import { dirname, join } from 'node:path';
import { openPage, sleep, log, WEB, ROOT, STATE } from '../nt/browser-harness.mjs';

const argv = process.argv.slice(2);
const opt = (name, dflt) => {
  const i = argv.indexOf(name);
  return i >= 0 ? argv[i + 1] : dflt;
};
const map = opt('--map', 'c1a0');
const shotDir = opt('--shot', null);
const minutes = Number(opt('--minutes', 40));
const measure = Number(opt('--measure', 60));
const keep = argv.includes('--keep');
const overlayDir = opt('--overlay', null);
const exe = opt('--exe', '/hl/hl_linux');
const want = opt('--want', null);
const env = (k, d) => process.env[k] || d;
const HL = env('HL_DIR', join(process.env.HOME, '.steam/steam/steamapps/common/Half-Life'));
const GL = env('GEX_GL_I386', join(ROOT, 'guest/gl/out-i386'));
const STEAM = env('GEX_STEAM_I386', join(ROOT, 'guest/steam/out-i386'));
const FALLBACK = env('GEX_HL_FALLBACK', '');

/** The bundle: the game at /hl, gex's libraries under /gex, the host files
 * at their own paths. Rebuilt when missing (delete it to refresh). */
function bundle() {
  const out = join(WEB, 'public/gex/nt/halflife.tar');
  if (existsSync(out)) return out;
  const stage = join(STATE, 'stage-halflife');
  rmSync(stage, { recursive: true, force: true });
  mkdirSync(join(stage, 'gex'), { recursive: true });
  mkdirSync(join(stage, 'home/hl'), { recursive: true });
  mkdirSync(join(stage, 'tmp'), { recursive: true });
  symlinkSync(HL, join(stage, 'hl'));
  symlinkSync(GL, join(stage, 'gex/gl'));
  symlinkSync(STEAM, join(stage, 'gex/steam'));
  if (FALLBACK) symlinkSync(FALLBACK, join(stage, 'gex/fallback'));
  const files = readFileSync(join(ROOT, 'tests/games/halflife-system-files.txt'), 'utf8')
    .split('\n').filter((l) => l && !l.startsWith('#') && existsSync(l));
  for (const f of files) {
    const at = join(stage, f);
    mkdirSync(dirname(at), { recursive: true });
    if (!existsSync(at)) symlinkSync(f, at);
  }
  const top = ['hl', 'gex', 'home', 'tmp', ...new Set(files.map((f) => f.split('/')[1]))];
  mkdirSync(dirname(out), { recursive: true });
  // chromehtml's Chromium (libcef.so) is most of the game directory's size
  // and must stay: the engine calls its factory unconditionally.
  execFileSync('tar', ['-chf', out, '-C', stage, ...new Set(top)]);
  log(`bundle ${out}: ${(statSync(out).size / 1048576).toFixed(0)} MiB`);
  return out;
}

async function main() {
  for (const f of ['xwayland/Xwayland.mjs', 'gex-wgpu-replay.wasm']) {
    if (!existsSync(join(WEB, 'public/gex', f))) throw new Error(`no web/public/gex/${f}`);
  }
  const tar = bundle();
  let overlay = '';
  if (overlayDir) {
    const out = join(WEB, 'public/gex/nt/overlay.tar');
    execFileSync('tar', ['-chf', out, '-C', overlayDir, '.']);
    overlay = ',/gex/nt/overlay.tar';
  }
  const t0 = Date.now();
  const genv = [
    'LD_LIBRARY_PATH=/gex/steam:/gex/gl:/hl:/gex/fallback:/usr/lib32',
    'SDL_VIDEODRIVER=wayland', 'SDL_AUDIODRIVER=dummy', 'XDG_RUNTIME_DIR=/tmp',
    'DISPLAY=:0', 'HOME=/home/hl',
    ...(process.env.GEX_HL_PAGE_ENV ? [process.env.GEX_HL_PAGE_ENV] : []),
  ].join(',');
  // No joystick: SDL's udev hotplug path is for devices a page has none of.
  const args = overlayDir ? opt('--exe-args', '')
    : `-novid -nojoy ${opt('--hl-args', '')} +map ${map}`;
  const { browser, page, evaluate, state, shot, consoleLines } = await openPage(
    `tar=/gex/nt/${tar.split('/').pop()}${overlay}&exe=${exe}&cwd=/hl&gl=1&glbackend=wgpu&hostx=1`
      + (process.env.GEX_GLCAPTURE ? `&glcapture=${process.env.GEX_GLCAPTURE}` : '')
      + (process.env.GEX_MESA_ENV ? `&mesaenv=${encodeURIComponent(process.env.GEX_MESA_ENV)}` : '')
      + (process.env.GEX_NT_PAGE_FLAGS ?? "")
      + `&size=800x600&trapstack=1&env=${encodeURIComponent(genv)}&args=${encodeURIComponent(args)}`,
    { width: 820, height: 700, shotDir },
  );
  void page;
  // Frames: the compositor's GL presents (describe().gl counts them).
  const presents = async () => {
    const gl = (await evaluate('globalThis.__gexCompositor?.describe()', true))?.gl ?? '';
    const m = /presented=(\d+)/.exec(gl);
    return m ? Number(m[1]) : null;
  };
  const end = t0 + minutes * 60000;
  let up = null;
  let shots = 0;
  let nextShot = Date.now() + 30000;
  for (;;) {
    const s = await state();
    // A program that prints its answer and exits (sdl-fbo-blit info).
    if (s?.phase === 'done' && want && new RegExp(want).test(s.out)) {
      if (opt('--out', null)) writeFileSync(opt('--out', null), s.out);
      console.log(`ok   ${exe}: ${want} (exited ${s.status})`);
      break;
    }
    if (s?.phase === 'failed' || s?.phase === 'done') {
      log(`output: ${s.out.slice(-3000)}`);
      throw new Error(`Half-Life stopped (${s.phase}, status ${s.status}) ${s.error ?? ''}`);
    }
    const threw = consoleLines.find((l) => /\[gex\] pid \d+:\d+ threw:|execute fault|\[harness\] the page crashed/.test(l));
    if (threw) throw new Error(`a thread died: ${threw.slice(0, 600)}`);
    const out = s?.out ?? '';
    const n = await presents();
    if (want && !up && new RegExp(want).test(out)) {
      up = { t: Date.now(), n };
      console.log(`ok   ${exe}: ${want} (presents ${n})`);
    }
    if (!up && /Unknown command: VModEnable|loaded map|Loading map/.test(out)) {
      up = { t: Date.now(), n };
      console.log(`ok   ${map} is loaded ${((up.t - t0) / 1000).toFixed(0)} s after the page opened (presents ${n})`);
    }
    if (Date.now() >= nextShot) {
      nextShot += 30000;
      await shot(`t${String(++shots).padStart(3, '0')}`);
      log(`${((Date.now() - t0) / 1000).toFixed(0)} s: presents ${n}; ${out.slice(-160).replace(/\n/g, ' | ')}`);
      // Each guest thread is a Worker with its own dispatch tables, all in
      // the renderer's one V8 heap cage: the count is what an OOM is about.
      const { targetInfos } = await browser.send('Target.getTargets', {}).catch(() => ({ targetInfos: [] }));
      log(`workers ${targetInfos.filter((t) => /worker/.test(t.type)).length}`);
      const d = await evaluate('globalThis.__gexCompositor?.describe()', true);
      if (d) log(`compositor: frames ${d.frames}, windows ${d.windows}, lit ${d.lit}, ${JSON.stringify(d.list)}, gl ${d.gl}`);
    }
    if (up && Date.now() - up.t >= measure * 1000) {
      const n2 = await presents();
      await shot('map');
      // --out FILE: the program's whole output, for a run that prints its
      // answer (sdl-fbo-blit info) rather than draws it.
      if (opt('--out', null)) writeFileSync(opt('--out', null), (await state())?.out ?? '');
      const secs = (Date.now() - up.t) / 1000;
      console.log(`ok   ${map}: ${n2 !== null && up.n !== null ? ((n2 - up.n) / secs).toFixed(1) : '?'} frames/s over ${secs.toFixed(0)} s`);
      break;
    }
    if (Date.now() > end) throw new Error(`no map after ${minutes} min`);
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
