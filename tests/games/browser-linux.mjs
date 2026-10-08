// A Linux game in headless Chromium (web/nt.html): the game's directory, its
// extra libraries, gex's guest GL and the host files it opens bundled into
// the page, its X11 window on the page's own X server (Xwayland as wasm,
// `hostx=1`), its OpenGL through gex-gl onto Mesa's gex_wgpu driver over
// WebGPU, and scripted keys from a GEX_WL_INPUT script (the same one the
// native run uses, tests/games/*.wl), timed from the page's Nth GL present.
//
//   node tests/games/browser-linux.mjs --name NAME --dir GAME_DIR --exe REL
//        [--cwd REL] [--args ARGS] [--libs DIR:DIR] [--files LIST] [--home DIR]
//        [--input SCRIPT.wl] [--size WxH] [--minutes N] [--shot DIR]
//        [--profile AT,SECS[,MATCH]] [--gameplay N] [--mem64] [--fjit] [--wasmdlls SO=PATH] [--query K=V&...] [--env K=V,K=V] [--end-on-exit]
//
// --files lists host files the game opens, one per line (collected from a
// native `gexc run --strace`; tests/games/retrocycles-linux-files.txt).
// --gameplay N: report when the page has presented N GL frames.
// --mem64: a 64-bit guest memory (the wasm64 shim, `?mem64=1`; docs/unity-dotnet.md P5).
// --env: more environment for the guest. --end-on-exit: a program that exits is a result
// (its output and status are printed), not a failure.
// Needs web/public/gex built (shims/browser/build.sh), Mesa's wasm
// (gex-wgpu-replay.*) and web/public/gex/xwayland/ (web/build-xwayland-wasm.sh),
// and GEX_GL_X64 (guest/gl/build.sh with GEX_CC=gcc).
//
// GEX_NT_VITE_PORT / GEX_NT_CHROME_PORT pick the ports (default 5291 and
// 9431, not the harness's own defaults).
//
// Prints `ok` lines (first frame, the input script's start, gameplay) and the
// frame rate over the last minute, with a screenshot every 30 s.

import { existsSync, mkdirSync, readFileSync, readdirSync, rmSync, statSync, symlinkSync, writeFileSync } from 'node:fs';
import { execFileSync } from 'node:child_process';
import { deflateSync, crc32 } from 'node:zlib';

/** RGBA rows (top first) as a PNG. */
function encodePng(w, h, px) {
  const chunk = (type, data) => {
    const len = Buffer.alloc(4); len.writeUInt32BE(data.length);
    const td = Buffer.concat([Buffer.from(type, 'latin1'), data]);
    const crc = Buffer.alloc(4); crc.writeUInt32BE(crc32(td) >>> 0);
    return Buffer.concat([len, td, crc]);
  };
  const ihdr = Buffer.alloc(13);
  ihdr.writeUInt32BE(w, 0); ihdr.writeUInt32BE(h, 4); ihdr[8] = 8; ihdr[9] = 6;
  const raw = Buffer.alloc((w * 4 + 1) * h);
  for (let y = 0; y < h; y++) px.copy(raw, y * (w * 4 + 1) + 1, y * w * 4, (y + 1) * w * 4);
  return Buffer.concat([Buffer.from([137, 80, 78, 71, 13, 10, 26, 10]), chunk('IHDR', ihdr), chunk('IDAT', deflateSync(raw)), chunk('IEND', Buffer.alloc(0))]);
}
import { dirname, join, basename } from 'node:path';
// Ports of its own (the harness reads these when it loads), so a games run
// and a gate's browser legs on the defaults (5199, 9339) do not collide.
process.env.GEX_NT_VITE_PORT ??= '5291';
process.env.GEX_NT_CHROME_PORT ??= '9431';
const { openPage, sleep, log, WEB, STATE, summariseProfile } = await import('../nt/browser-harness.mjs');

/** A CPU profile's span, its time not idle, and its time in Atomics.wait (ms). */
function profileBusy(profile) {
  const byId = new Map(profile.nodes.map((x) => [x.id, x]));
  const { samples = [], timeDeltas = [] } = profile;
  let span = 0, idle = 0, wait = 0;
  for (let i = 0; i < samples.length; i++) {
    const dt = (timeDeltas[i + 1] ?? 0) / 1000;
    span += dt;
    const f = byId.get(samples[i])?.callFrame?.functionName ?? '';
    if (f === '(idle)' || f === '(program)') idle += dt;
    else if (f === 'wait' || f === 'Atomics.wait' || f === 'waitAsync') wait += dt;
  }
  return { span, busy: span - idle - wait, wait };
}

const argv = process.argv.slice(2);
const opt = (name, dflt) => {
  const i = argv.indexOf(name);
  return i >= 0 ? argv[i + 1] : dflt;
};
const name = opt('--name', 'game');
const gameDir = opt('--dir', null);
const exe = opt('--exe', null);
const cwd = opt('--cwd', dirname(exe ?? '.'));
const minutes = Number(opt('--minutes', 30));
const shotDir = opt('--shot', null);
const gameplay = Number(opt('--gameplay', 0));
const [width, height] = opt('--size', '1280x720').split('x').map(Number);
const GL = process.env.GEX_GL_X64;
if (!gameDir || !exe || !GL) throw new Error('need --dir, --exe and GEX_GL_X64');

/** The bundle: the game at /game, libraries under /gex, host files at
 * their own paths (tar follows the links). Delete it to rebuild. */
function bundle() {
  const out = join(WEB, `public/gex/nt/${name}.tar`);
  if (existsSync(out)) return out;
  const stage = join(STATE, `stage-${name}`);
  rmSync(stage, { recursive: true, force: true });
  mkdirSync(join(stage, 'gex/libs'), { recursive: true });
  mkdirSync(join(stage, 'home/g'), { recursive: true });
  // --home DIR: its entries in the guest's $HOME (saved settings, so a game skips first-run screens).
  const home = opt('--home', null);
  if (home) for (const e of readdirSync(home)) symlinkSync(join(home, e), join(stage, 'home/g', e));
  mkdirSync(join(stage, 'tmp'), { recursive: true });
  symlinkSync(gameDir, join(stage, 'game'));
  symlinkSync(GL, join(stage, 'gex/gl'));
  const libs = (opt('--libs', '') || '').split(':').filter(Boolean);
  libs.forEach((d, i) => symlinkSync(d, join(stage, `gex/libs/${i}`)));
  const listed = opt('--files', null)
    ? readFileSync(opt('--files'), 'utf8').split('\n').filter((l) => l && !l.startsWith('#'))
    : [];
  const files = ['/lib64/ld-linux-x86-64.so.2', ...listed].filter((f) => existsSync(f));
  for (const f of files) {
    const at = join(stage, f);
    mkdirSync(dirname(at), { recursive: true });
    if (!existsSync(at)) symlinkSync(f, at);
  }
  const top = ['game', 'gex', 'home', 'tmp', ...new Set(files.map((f) => f.split('/')[1]))];
  mkdirSync(dirname(out), { recursive: true });
  execFileSync('tar', ['-chf', out, '-C', stage, ...new Set(top)]);
  log(`bundle ${out}: ${(statSync(out).size / 1048576).toFixed(0)} MiB`);
  return out;
}

/** A GEX_WL_INPUT script: `frames N`, then `SECONDS tap|key NAME [1|0]`. */
function script() {
  const file = opt('--input', null);
  if (!file) return { after: Infinity, events: [] };
  let after = 1;
  const events = [];
  for (const raw of readFileSync(file, 'utf8').split('\n')) {
    const w = raw.replace(/#.*/, '').trim().split(/\s+/).filter(Boolean);
    if (!w.length) continue;
    if (w[0] === 'frames') { after = Number(w[1]); continue; }
    const t = Number(w[0]);
    if (w[1] === 'tap') { events.push([t, w[2], 'keydown'], [t + 0.1, w[2], 'keyup']); }
    else if (w[1] === 'key') events.push([t, w[2], w[3] === '1' ? 'keydown' : 'keyup']);
  }
  events.sort((a, b) => a[0] - b[0]);
  return { after, events };
}

/** A script's key name as KeyboardEvent.code (what the compositor maps). */
function code(k) {
  const named = { enter: 'Enter', return: 'Enter', space: 'Space', esc: 'Escape', escape: 'Escape',
    up: 'ArrowUp', down: 'ArrowDown', left: 'ArrowLeft', right: 'ArrowRight', tab: 'Tab',
    shift: 'ShiftLeft', ctrl: 'ControlLeft', alt: 'AltLeft', backspace: 'Backspace' };
  const l = k.toLowerCase();
  if (named[l]) return named[l];
  if (/^[a-z]$/.test(l)) return `Key${l.toUpperCase()}`;
  if (/^[0-9]$/.test(l)) return `Digit${l}`;
  if (/^f[0-9]+$/.test(l)) return l.toUpperCase();
  throw new Error(`no key ${k}`);
}

async function main() {
  for (const f of ['xwayland/Xwayland.mjs', 'gex-wgpu-replay.wasm']) {
    if (!existsSync(join(WEB, 'public/gex', f))) throw new Error(`no web/public/gex/${f}`);
  }
  const tar = bundle();
  const libs = (opt('--libs', '') || '').split(':').filter(Boolean).map((_, i) => `/gex/libs/${i}`);
  const genv = [
    `LD_LIBRARY_PATH=${['/gex/gl', ...libs, `/game/${dirname(exe)}`].join(':')}`,
    'SDL_VIDEODRIVER=x11', 'SDL_AUDIODRIVER=dummy', 'XDG_RUNTIME_DIR=/tmp', 'DISPLAY=:0', 'HOME=/home/g',
    'DBUS_SESSION_BUS_ADDRESS=unix:path=/nonexistent/gex-no-bus',
    ...(opt('--env', '') || '').split(/,(?=[A-Za-z_][A-Za-z0-9_]*=)/).filter(Boolean),
  ].join(',');
  const t0 = Date.now();
  const { evaluate, state, shot, consoleLines, profile } = await openPage(
    `tar=/gex/nt/${basename(tar)}&exe=/game/${exe}&cwd=/game/${cwd}&gl=1&glbackend=wgpu&hostx=1`
      + (argv.includes('--mem64') ? '&mem64=1' : '') + (argv.includes('--fjit') ? '&fjit=1' : '')
      + (opt('--wasmdlls', null) ? `&wasmdlls=${encodeURIComponent(opt('--wasmdlls'))}` : '')
      + (opt('--query', null) ? `&${opt('--query')}` : '')
      + `&size=${width}x${height}&trapstack=1&env=${encodeURIComponent(genv)}&args=${encodeURIComponent(opt('--args', ''))}`,
    { width: width + 20, height: height + 100, shotDir },
  );
  const presents = async () => {
    const gl = (await evaluate('globalThis.__gexCompositor?.describe()', true))?.gl ?? '';
    const m = /presented=(\d+)/.exec(gl);
    // A describe() that timed out (a busy compositor) answers nothing: keep
    // the last count rather than going back to zero.
    if (m) last = Math.max(last, Number(m[1]));
    return last;
  };
  let last = 0;
  const key = (c, type) => evaluate(
    `(() => { for (const t of document.querySelectorAll('canvas')) t.dispatchEvent(new KeyboardEvent(${JSON.stringify(type)}, { code: ${JSON.stringify(c)}, key: ${JSON.stringify(c)}, bubbles: true })); return 1; })()`, true);
  const { after, events } = script();
  let first = null, start = null, next = 0, playAt = null, shots = 0, nextShot = Date.now() + 30000;
  const profSpec = opt('--profile', null) ? (([at = '300', secs = '20', match = '']) => ({ at: +at, secs: +secs, match }))(opt('--profile').split(',')) : null;
  let profiled = false;
  let hundredth = null;
  const window = [];
  const end = t0 + minutes * 60000;
  while (Date.now() < end) {
    const s = await state();
    if (s?.phase === 'done' && argv.includes('--end-on-exit')) {
      console.log(`exit ${s.status}\n${s.out ?? ''}`);
      return;
    }
    if (s?.phase === 'failed' || s?.phase === 'done') {
      log(`output: ${(s.out ?? '').slice(-3000)}`);
      throw new Error(`${name} stopped (${s.phase}, status ${s.status}) ${s.error ?? ''}`);
    }
    const threw = consoleLines.find((l) => /\[gex\] pid \d+:\d+ threw:|execute fault|\[harness\] the page crashed/.test(l));
    if (threw) throw new Error(`a thread died: ${threw.slice(0, 600)}`);
    const n = await presents();
    window.push([Date.now(), n]);
    while (window.length && window[0][0] < Date.now() - 60000) window.shift();
    if (!first && n > 0) {
      first = Date.now();
      console.log(`ok   first frame ${((first - t0) / 1000).toFixed(0)} s after the page opened`);
    }
    // A game may present a frame or two long before its first real one (Muck
    // does in some runs only): the 100th present is the steadier start.
    if (!hundredth && n >= 100) {
      hundredth = Date.now();
      console.log(`ok   100 presents ${((hundredth - t0) / 1000).toFixed(0)} s after the page opened`);
    }
    if (!start && n >= after) {
      start = Date.now();
      console.log(`ok   input script starts at present ${n}`);
    }
    while (start && next < events.length && Date.now() - start >= events[next][0] * 1000) {
      const [t, k, type] = events[next++];
      await key(code(k), type);
      log(`input at ${t} s: ${type} ${k} (present ${n})`);
    }
    if (gameplay && !playAt && n >= gameplay) {
      playAt = Date.now();
      console.log(`ok   gameplay (present ${n}) ${((playAt - t0) / 1000).toFixed(0)} s after the page opened`);
    }
    // --profile AT,SECS[,MATCH]: CPU profiles of the page's main thread and
    // every Worker whose script URL contains MATCH, AT s after the page
    // opened, for SECS; each one's busy time (not idle, not in Atomics.wait)
    // and its top functions, saved as .cpuprofile with --shot.
    if (profSpec && !profiled && Date.now() - t0 >= profSpec.at * 1000) {
      profiled = true;
      const n0 = n;
      const profs = await profile(profSpec.match, profSpec.secs * 1000, true);
      const rows = [];
      for (const [k, { url, profile: prof }] of profs.entries()) {
        const file = `${url.split('/').pop().split('?')[0]}.${k}`;
        const b = profileBusy(prof);
        rows.push({ file, ...b });
        if (shotDir) writeFileSync(join(shotDir, `${file}.cpuprofile`), JSON.stringify(prof));
        if (b.busy >= 0.05 * b.span) log(`profile of ${file}:\n${summariseProfile(prof, 15)}`);
      }
      rows.sort((a, b) => b.busy - a.busy);
      console.log(`ok   profile over ${profSpec.secs} s (${n - n0} presents): ` + rows.slice(0, 12).map((r) => `${r.file} busy ${(100 * r.busy / Math.max(r.span, 1)).toFixed(0)}% wait ${(100 * r.wait / Math.max(r.span, 1)).toFixed(0)}%`).join('; '));
    }
    if (Date.now() >= nextShot) {
      nextShot += 30000;
      await shot(`t${String(++shots).padStart(3, '0')}`);
      // The compositor's own output, read back (the page screenshot can miss the canvas).
      if (shotDir) {
        const r = await evaluate('globalThis.__gexCompositor?.readoutBase64()', true);
        if (r?.b64) {
          writeFileSync(join(shotDir, `c${String(shots).padStart(3, '0')}.png`), encodePng(r.w, r.h, Buffer.from(r.b64, 'base64')));
        }
      }
      const rate = window.length > 1 ? (n - window[0][1]) / ((Date.now() - window[0][0]) / 1000) : 0;
      log(`${((Date.now() - t0) / 1000).toFixed(0)} s: presents ${n}, ${rate.toFixed(1)}/s over the last minute; ${(s?.out ?? '').slice(-160).replace(/\n/g, ' | ')}`);
      // The compositor's own account: its windows, its notes (GL attachment
      // per window, which says why a GL window stays black), lit pixels.
      const d = await evaluate('globalThis.__gexCompositor?.describe()', true);
      if (d) log(`compositor: frame ${d.frames}, ${d.windows} window(s), lit ${d.lit}; ${(d.list ?? []).join('; ')}; gl: ${(d.gl ?? '').slice(0, 160)}; notes: ${(d.log ?? []).slice(-3).join(' / ').slice(0, 600)}`);
    }
    await sleep(1000);
  }
  if (shotDir) await shot('end');
  if (gameplay && !playAt) throw new Error(`no gameplay after ${minutes} minutes`);
}

main().then(() => process.exit(0), (e) => {
  console.log(`FAIL ${e.message}`);
  process.exit(1);
});
