// Noita on W3 (Wine's own i386 Unix side) in the page: web/nt.html, x86 shim,
// gl=1 on Mesa's gex_wgpu driver. The bench-box half of
// tests/games/noita-page-leg.sh.
//
//   node tests/games/noita-page.mjs TREE NOITA_EXE [measure-seconds=300]
//
// Bundles (TREE/web/public/gex/nt): w3d.tar (the Wine dist at /w3) and
// NOITA_TAR (default noita.tar: the prefix at /pfx with the seedpin mod
// enabled in its save, the game at /g with mods/seedpin, xkb data).
// NOITA_EXE is the same noita.exe on this machine's disk: its PE header is
// how the Noita process is told from Wine's own.
//
// The native recipe's steps (MERGE-STATUS.md, "Canonical Noita recipe"),
// as CDP clicks, each at a count of frames the compositor's GL device has
// produced: the release notes' Ok at (85,661) at frame 12; New Game at
// (639,348) at frame 100; the first game mode at (445,250) at frame 180.
// Noita runs with -single_threaded_loading, as natively.
//
// The metric is the world's own frame counter (noita.exe 0x122374c, as
// docs/noita-perf.md and GEX_NT_PEEK use it), read from the Noita process's
// memory by the page (host.mjs's `__gexMemories`): world frames per second
// over MEASURE seconds from when the counter first moves. The world must
// keep running: a 30 s slice in which the counter does not move fails.
//
// Prints progress lines, then one result line:
//   "== noita: world up at T s ..., world W frames/s over S s (...)"
// or "== noita: FAIL <why>". Exit 0 on the former.
import { mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';

const [tree, exePath, measureArg = '300'] = process.argv.slice(2);
if (!tree || !exePath) {
  console.log('usage: noita-page.mjs TREE NOITA_EXE [measure-seconds]');
  process.exit(2);
}
const measure = Number(measureArg);
const { openPage, sleep, stopAll, summariseProfile } = await import(join(tree, 'tests/nt/browser-harness.mjs'));
const WORLD = 0x122374c;
const IMAGE = 0x400000;
// Minutes from the page's start to the world, at most (tier-1 or a hang).
const worldBy = Number(process.env.NOITA_WORLD_BY_MIN || 12) * 60000;
// Whether TREE's compositor hit-tests by input region (545564cb): the
// click offset above depends on it.
const inputRegions = readFileSync(`${tree}/web/src/compositor-worker.js`, 'utf8').includes('#takesInput');

// noita.exe's PE signature and file header (24 bytes at e_lfanew: machine,
// sections, link time stamp...), as mapped at its base.
const exe = readFileSync(exePath);
const lfanew = exe.readUInt32LE(0x3c);
const sig = [...exe.subarray(lfanew, lfanew + 24)];

const tars = `/gex/nt/w3d.tar,/gex/nt/${process.env.NOITA_TAR || 'noita.tar'}`;
const argv = encodeURIComponent(JSON.stringify(['noita.exe', '-single_threaded_loading']));
const env = encodeURIComponent(`WINEPREFIX=/pfx,WINEDEBUG=${process.env.WDBG || 'err+all,fixme-all'},XDG_RUNTIME_DIR=/tmp,WAYLAND_DISPLAY=wayland-0`);
const { browser, page, state, evaluate, consoleLines, profile } = await openPage(
  `tar=${tars}&exe=/w3/lib/wine/i386-unix/wine&mtimes=1&cwd=/g&gl=1&glbackend=wgpu&size=1320x800`
    + `&argv=${argv}&env=${env}${process.env.NOITA_Q ?? '&cgen=1'}`,
  { width: 1340, height: 900 },
);

const produced = async () => {
  const gl = (await evaluate('globalThis.__gexCompositor?.describe()', true))?.gl ?? '';
  const m = /produced=(\d+)/.exec(gl);
  return m ? Number(m[1]) : 0;
};
const noitaWindow = async () => {
  const d = await evaluate('globalThis.__gexCompositor?.describe()', true);
  const m = (d?.list ?? []).map((l) => /role=1 (\d+)x(\d+)@(-?\d+),(-?\d+).*"(.*)"/.exec(l)).find((x) => x && /Noita/.test(x[5]));
  return m ? { w: +m[1], h: +m[2], x: +m[3], y: +m[4] } : null;
};
// NOITA_SHOT=DIR: the compositor's output, read back (a page screenshot
// does not have the WebGPU canvas), as DIR/NAME.ppm.
const grab = async (name) => {
  const dir = process.env.NOITA_SHOT;
  if (!dir) return;
  try {
    const r = await evaluate('globalThis.__gexCompositor?.readoutBase64()', true);
    if (!r) return;
    const rgba = Buffer.from(r.b64, 'base64');
    const rgb = Buffer.alloc(r.w * r.h * 3);
    for (let i = 0, j = 0; i < rgba.length; i += 4, j += 3) { rgb[j] = rgba[i]; rgb[j + 1] = rgba[i + 1]; rgb[j + 2] = rgba[i + 2]; }
    mkdirSync(dir, { recursive: true });
    writeFileSync(join(dir, `${name}.ppm`), Buffer.concat([Buffer.from(`P6\n${r.w} ${r.h}\n255\n`), rgb]));
  } catch { /* a missed picture is no failure */ }
};
// The world counter (through the singleton's pointer, as GEX_NT_PEEK's
// `0x122374c*`), from whichever process has noita.exe at its base.
const peek = async () => evaluate(`(() => {
  const all = globalThis.__gexMemories;
  if (!all) return null;
  const sig = ${JSON.stringify(sig)};
  for (const [pid, ref] of all) {
    const mem = ref.deref();
    if (!mem) continue;
    const b = new Uint8Array(mem.buffer);
    const at = ${IMAGE + lfanew};
    if (b.length < ${WORLD + 4} || sig.some((v, i) => b[at + i] !== v)) continue;
    // The game-state singleton's pointer, then its first field.
    const dv = new DataView(mem.buffer);
    const at2 = dv.getUint32(${WORLD}, true);
    if (at2 === 0 || at2 + 4 > b.length) return { pid, v: 0 };
    return { pid, v: dv.getUint32(at2, true) };
  }
  return null;
})()`);
// As browser-noita.mjs: the pointer arrives first, the release a little
// after the press -- Noita's buttons act on a hover they have drawn.
const click = async (x, y) => {
  const send = (type) => browser.send('Input.dispatchMouseEvent', { type, x, y, button: 'left', clickCount: 1, buttons: type === 'mousePressed' ? 1 : 0 }, page);
  await send('mouseMoved');
  await sleep(1500);
  await send('mousePressed');
  await sleep(600);
  await send('mouseReleased');
};

const prof = process.env.NOITA_PROF
  ? (([at = '60', ms = '20000']) => ({ at: +at, ms: +ms, done: false }))(process.env.NOITA_PROF.split(','))
  : null;
const steps = [
  { at: 12, x: 85, y: 661, what: 'dismissing the release notes' },
  { at: 100, x: 639, y: 348, what: 'New Game' },
  { at: 180, x: 445, y: 250, what: 'the first game mode' },
];
const t0 = Date.now();
const secs = () => ((Date.now() - t0) / 1000).toFixed(0);
let tRun = null, world = null, lastMove = null, nextLog = 0, why = null, result = null, lastLog = null;
let s = null;
for (;;) {
  s = await state();
  const now = Date.now();
  if (s?.phase === 'done' || s?.phase === 'failed') { why = `the program ended: phase ${s.phase}, status ${s.status} ${s.error ?? ''}`; break; }
  const threw = consoleLines.find((l) => /\[gex\] pid \d+(:\d+)? threw:|execute fault|\[harness\] the page crashed/.test(l));
  if (threw) { why = `a thread died: ${threw.slice(0, 400)}`; break; }
  // A unit cache without the provider modules (gex.mjs `#cgenProviders`
  // names them, as cgen.rs `gex_cgen_missing` does the NT shim's trampoline
  // unit) runs, but slower, or at a crawl that looks like a hang.
  const lacking = consoleLines.find((l) => /missing unit: /.test(l));
  if (lacking) { why = `the unit cache is incomplete: ${lacking.replace(/^.*missing unit: /, '').slice(0, 400)}`; break; }
  if (tRun === null && s && s.phase !== 'loading' && s.phase !== 'fetching') tRun = now;
  const n = tRun === null ? 0 : await produced();
  if (steps.length && n >= steps[0].at) {
    const w = await noitaWindow();
    if (w) {
      // The surface holds Wine's own window frame around the 1280x720
      // client area the recipe's points are in: a 3-pixel border and a
      // caption, the client at (3,29) in the surface (read off the
      // compositor's output with the frozen prefix's theme). The client
      // area is a GL subsurface there, which the page compositor hands the
      // pointer to with subsurface-local coordinates (it ignores the empty
      // input region Wine gives it), and Wine reads those as relative to the
      // whole window: so the point is offset by the frame once more. A
      // compositor that honours input regions (545564cb, `#takesInput`)
      // gives the window the pointer in its own coordinates: one (3,29).
      const st = steps.shift();
      const [fx, fy] = inputRegions ? [3, 29] : [6, 58];
      const x = w.x + fx + st.x, y = w.y + fy + st.y;
      console.log(`${secs()} s, frame ${n}: ${st.what} at ${x},${y} (window ${w.w}x${w.h}@${w.x},${w.y})`);
      await click(x, y);
      await grab(`step-${3 - steps.length}`);
    }
  }
  if (!steps.length) {
    const p = await peek();
    if (p && p.v > 0) {
      if (!world) {
        world = { t: now, v: p.v, pid: p.pid };
        lastMove = { t: now, v: p.v };
        console.log(`${secs()} s: the world is up (pid ${p.pid}, world frame ${p.v}, ${n} frames)`);
      } else if (p.v !== lastMove.v) {
        lastMove = { t: now, v: p.v };
      } else if (now - lastMove.t > 30000) {
        why = `the world stopped at world frame ${p.v} (${((now - world.t) / 1000).toFixed(0)} s in)`;
        break;
      }
      // NOITA_PROF=AT,MS: a CPU profile of every Worker and the page, AT s
      // into the world for MS ms, summarised (and saved beside NOITA_SHOT's
      // pictures). The measurement goes on through it: a profiling run's
      // world rate is not a baseline.
      if (prof && !prof.done && now - world.t >= prof.at * 1000) {
        prof.done = true;
        const v0 = p.v, tp = Date.now();
        const profs = await profile('', prof.ms, true);
        const v1 = (await peek())?.v ?? v0;
        console.log(`== profile ${((tp - world.t) / 1000).toFixed(0)} s into the world for ${((Date.now() - tp) / 1000).toFixed(1)} s: world ${((v1 - v0) * 1000 / (Date.now() - tp)).toFixed(1)} frames/s while sampling`);
        profs.forEach(({ url, profile: pr }, k) => {
          if (process.env.NOITA_SHOT) writeFileSync(join(process.env.NOITA_SHOT, `prof.${k}.cpuprofile`), JSON.stringify(pr));
          const sum = summariseProfile(pr, 25);
          console.log(`-- profile ${k} ${url}`);
          console.log(typeof sum === 'string' ? sum : JSON.stringify(sum, null, 1).slice(0, 5000));
        });
      }
      if (now - world.t >= measure * 1000) {
        const span = (now - world.t) / 1000;
        result = { span, wfps: (p.v - world.v) / span, n };
        break;
      }
    }
  }
  if (!world && now - t0 > worldBy) {
    const p = steps.length ? null : await peek();
    why = `no world after ${secs()} s (${n} frames, ${steps.length} clicks left, world frame ${p ? p.v : 'unread'})`;
    break;
  }
  if (now >= nextLog) {
    nextLog = now + 30000;
    const p = await peek().catch(() => null);
    const rate = p && lastLog ? ` (${((p.v - lastLog.v) * 1000 / (now - lastLog.t)).toFixed(1)} world frames/s since the last line)` : '';
    if (p && world) lastLog = { t: now, v: p.v };
    console.log(`${secs()} s: ${n} frames${p ? `, world frame ${p.v}` : ''}${rate}`);
    await grab(`t${secs().padStart(4, '0')}`);
  }
  await sleep(1000);
}
await grab('end');
// With NOITA_SHOT, the program's whole output beside the pictures.
if (process.env.NOITA_SHOT && s?.out) writeFileSync(join(process.env.NOITA_SHOT, 'out.txt'), s.out);
for (const l of consoleLines.filter((x) => /no function-tier unit/.test(x))) {
  const m = /image (\S+) at base (0x[0-9a-f]+): no function-tier unit/.exec(l);
  if (m) console.log(`unitless: ${m[1]} ${m[2]}`);
}
if (result) {
  const presents = result.n;
  console.log(`== noita: world up at ${((world.t - t0) / 1000).toFixed(0)} s, world ${result.wfps.toFixed(1)} frames/s over ${result.span.toFixed(0)} s (${presents} GL frames in all)`);
} else {
  console.log(`== noita: FAIL ${why}`);
  if (s?.out) console.log(s.out.slice(-3000));
}
stopAll();
await sleep(1000);
process.exit(result ? 0 : 1);
