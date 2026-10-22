#!/usr/bin/env node
// The ejected W3 page leg: a site from web/eject-site.mjs of a W3 project
// (docs/eject.md, "W3 pages") reaches its window in headless Chromium, built
// with the features its project left out really gone.
//
//   node tests/eject/w3-page.mjs SITE [--title RE] [--timeout S] [--port N (default: any free one)] [--chrome PATH] [--placed] [--shell PAGE]
//
// Checks, in order:
//   1. the site's nt.json says kind w3, and for each feature it says is off,
//      the files that feature is are not in the site ("compositor":
//      assets/compositor-*, assets/compositor-worker-*, assets/gpu-device-*;
//      "gl": gex/gex-wgpu-replay.*);
//   2. served (cross-origin isolated) and opened at its launcher, a toplevel
//      titled RE (default "Untitled - Paint") appears;
//   2b. start()'s progress says which programs and images the start-up
//      reached (`boot`, `log`: the runtime's structured notes);
//   3. with "compositor" off, what draws it is the page's bare desktop
//      (`describe().compositor === 'none'`), and the window has pixels;
//   4. no service worker: the site ships no sw.js, none is registered, and
//      the first load is the only one: the site's page (the project's
//      user/index.html, which imports app.mjs and calls start()) once, and
//      gex's test page (nt.html) neither shipped, named nor loaded;
//   5. with --placed (Paint, "compositor" off): the bare desktop leaves a
//      window its client places (gex_window_placement_v1) to Wine. A caption
//      drag moves it by the drag; a smooth one (a move a frame, out and back)
//      has it follow the pointer every frame without moving against the drag
//      or getting ahead of the pointer (gex_window_placement_v1.origin: the
//      window shook when Wine read the pointer against where it had already
//      moved the window); a border drag past its edge grows it and
//      one inside shrinks it (Win32's own SC_MOVE/SC_SIZE loops, the pointer
//      held by the canvas), and a click on Paint leaves its owned dialog
//      (Ctrl+W, Resize and Skew) above it: the page neither cascades nor
//      raises a placed window. A right click on the caption opens Wine's
//      window menu (the guest has the right button) and the browser's
//      context menu is cancelled.
//   1b. fill.json's `demand` units (eject-site.mjs --boot-units: units no
//      image loads before the program is up) are whole in the fill and in no
//      boot archive; run, the fill ends (`[app] fill:`) with no unit of it
//      failing (`[app] a late unit failed`, `[app] the fill failed`; not the
//      summary `[app] late units: N files from the fill`);
//   1c. with pieces (eject-site.mjs --pieces), no bytes twice: files of one
//      class with the same bytes share one piece's copy;
//   6. with --shell PAGE (tests/eject/w3-shell-min.html): PAGE is served as
//      the site's page instead, a hand-written one with its own container,
//      size, arguments and onExit. The desktop is its box's size, inside the
//      box; then kill() ends the run, `exited` and onExit say 137.
// Prints PASS or FAIL lines; exits 0 only when all pass.

import { execFileSync, spawn } from 'node:child_process';
import { createReadStream, existsSync, mkdtempSync, readdirSync, readFileSync, rmSync, statSync } from 'node:fs';
import http from 'node:http';
import { tmpdir } from 'node:os';
import { extname, join, resolve } from 'node:path';
import { Cdp } from '../../web/test/cdp.mjs';

const av = process.argv.slice(2);
const opt = (n, d) => { const i = av.indexOf(n); return i >= 0 ? av[i + 1] : d; };
const site = resolve(av.find((a, i) => !a.startsWith('--') && !['--title', '--timeout', '--port', '--chrome', '--shell'].includes(av[i - 1])) ?? '');
const shell = opt('--shell', null) ? resolve(opt('--shell', null)) : null;
const placedChecks = av.includes('--placed');
const titleRe = new RegExp(opt('--title', 'Untitled - Paint'));
const timeout = Number(opt('--timeout', '400')) * 1000;
// Port 0 by default: the system's choice, so two runs on one box (check
// class allows that) never collide. Chromium's debugging port likewise (see
// DevToolsActivePort below).
const askPort = Number(opt('--port', '0'));
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
let failed = 0;
const check = (ok, what) => { console.log(`${ok ? 'PASS' : 'FAIL'} ${what}`); if (!ok) failed++; return ok; };

// 1. What the build left out.
const ntPath = join(site, 'project/build/web/nt.json');
if (!check(existsSync(ntPath), `${ntPath} exists`)) process.exit(1);
const nt = JSON.parse(readFileSync(ntPath, 'utf8'));
check(nt.kind === 'w3', `the site is a W3 page (kind ${nt.kind})`);
const off = nt.features_off ?? [];
const assets = existsSync(join(site, 'assets')) ? readdirSync(join(site, 'assets')) : [];
const gexDir = existsSync(join(site, 'gex')) ? readdirSync(join(site, 'gex')) : [];
if (off.includes('compositor')) {
  const left = assets.filter((n) => /^(compositor|compositor-worker|gpu-device)-/.test(n));
  check(left.length === 0, `"compositor" off: no scene compositor in the site${left.length ? ` (found ${left.join(', ')})` : ''}`);
}
// Every unit a Worker can see published is one every Worker of the process
// can load: in the boot archive all Workers get at start (preload.json
// `tar:N`) or in the background fill (`unit:KEY/c.wasm`, eject-site.mjs
// --late), which every Worker also holds from its start; never in a list
// only Workers started later are given (PF's E7b: units-late.tar, and a
// Worker of winlogon trapped "published but cannot be loaded here").
{
  const preP = join(site, 'project/build/web/units/preload.json');
  const pre = existsSync(preP) ? JSON.parse(readFileSync(preP, 'utf8')) : [];
  const fillP = join(site, 'project/build/web/fill.json');
  const fill = existsSync(fillP) ? JSON.parse(readFileSync(fillP, 'utf8')) : { files: [] };
  const lateOnly = pre.filter(([k]) => /^late-/.test(k)).map(([k]) => k);
  // A unit's region entries are modules of their own in the archive, not units.
  const boot = pre.filter(([k]) => /^tar:/.test(k)).reduce((a, [k, , o]) => a + Number(k.slice(4)) - (o?.regions ?? 0), 0);
  const inFill = new Set(fill.files.filter(([n]) => /^unit:.*\/c\.wasm$/.test(n)).map(([n]) => n.slice(5, -7)));
  const units = nt.units ?? [];
  if (fill.demand) {
    const names = new Set(fill.files.map(([n]) => n));
    const bootKeys = new Set();
    for (const [k, url] of pre) {
      if (!/^tar:/.test(k)) continue;
      const tp = join(site, String(url).replace(/^\/+/, ''));
      // A unit archive is packed (pack.json: TAR.zst); its names are the units' keys.
      const z = existsSync(`${tp}.zst`) ? `${tp}.zst` : null;
      const list = z ? execFileSync('sh', ['-c', 'zstd -dc "$1" | tar -t', 'sh', z], { encoding: 'utf8' }) : existsSync(tp) ? execFileSync('tar', ['-tf', tp], { encoding: 'utf8' }) : '';
      for (const l of list.split('\n')) if (l.endsWith('/c.wasm')) bootKeys.add(l.slice(0, -7));
    }
    const whole = fill.demand.filter((k) => names.has(`unit:${k}/c.wasm`) && names.has(`unit:${k}/c.entries`));
    const twice = fill.demand.filter((k) => bootKeys.has(k));
    check(fill.demand.length > 0 && whole.length === fill.demand.length && twice.length === 0,
      `the fill's demand units: ${fill.demand.length}, ${whole.length} whole in the fill, ${twice.length} also in a boot archive`);
  }
  if (pre.some(([k]) => /^tar:/.test(k))) {
    check(lateOnly.length === 0 && boot + inFill.size === units.length,
      `every unit loadable by every Worker: ${boot} in the boot archive + ${inFill.size} in the fill = ${units.length} units${lateOnly.length ? `; given only to later Workers: ${lateOnly.join(', ')}` : ''}`);
  }
}
{
  const pj = join(site, 'project/build/web/pieces.json');
  if (existsSync(pj)) {
    const { createHash } = await import('node:crypto');
    const pi = JSON.parse(readFileSync(pj, 'utf8'));
    const body = pi.pieces.map(([url]) => {
      const f = join(site, String(url).replace(/^\/+/, ''));
      return existsSync(`${f}.zst`) ? execFileSync('zstd', ['-dc', `${f}.zst`], { maxBuffer: 1 << 30 }) : readFileSync(f);
    });
    const at = new Map(); // "class:hash" -> "piece:offset"
    let twice = 0, bytes = 0;
    for (const [path, i, off, n] of pi.files) {
      const k = `${pi.pieces[i][2]}:${createHash('sha256').update(body[i].subarray(off, off + n)).digest('hex')}`;
      const here = `${i}:${off}`;
      if (!at.has(k)) at.set(k, here);
      else if (at.get(k) !== here) { twice++; bytes += n; }
    }
    check(twice === 0, `pieces: no file's bytes stored twice (${pi.files.length} files; ${twice} copies stored again, ${bytes} bytes)`);
  }
}
check(!existsSync(join(site, 'sw.js')), 'the site ships no service worker (no sw.js)');
// The app library, and no test page: nt.html is gex's, never an ejected build's.
check(existsSync(join(site, 'app.mjs')), 'the site has the app library (app.mjs)');
{
  const named = [];
  const scan = (d, rel = '') => {
    for (const e of readdirSync(d, { withFileTypes: true })) {
      const r = rel ? `${rel}/${e.name}` : e.name;
      if (r.startsWith('project/build/web/lazy')) continue;
      if (e.isDirectory()) scan(join(d, e.name), r);
      else if (e.name === 'nt.html' || (/\.(m?js|html|json)$/.test(e.name) && readFileSync(join(d, e.name)).includes('nt.html'))) named.push(r);
    }
  };
  scan(site);
  check(named.length === 0, `no nt.html in the site, and nothing names it${named.length ? ` (${named.slice(0, 4).join(', ')})` : ''}`);
}
// "bundle": "split": the files the recorded run never opened are lazy files,
// each in the store under its hash.
if (nt.bundle?.split) {
  const lj = join(site, 'project/build/web/lazy.json');
  const m = existsSync(lj) ? JSON.parse(readFileSync(lj, 'utf8')) : null;
  const missing = m ? m.files.filter((f) => !existsSync(join(site, m.base, f[2]))).length : -1;
  check(!!m && missing === 0 && nt.params?.lazy === '/project/build/web/lazy.json',
    `bundle split: ${nt.bundle.read} read files in the bundle, ${m?.files.length ?? 'no'} lazy ones (${missing} missing from the store)`);
}
if (off.includes('gl')) {
  const left = gexDir.filter((n) => n.startsWith('gex-wgpu-replay'));
  check(left.length === 0, `"gl" off: no Mesa in the site${left.length ? ` (found ${left.join(', ')})` : ''}`);
}

// 2. Served and run to the title.
const types = { '.html': 'text/html', '.js': 'text/javascript', '.mjs': 'text/javascript', '.wasm': 'application/wasm', '.json': 'application/json', '.css': 'text/css' };
let lazyReads = 0;
const server = http.createServer((req, res) => {
  if (req.url.includes('/build/web/lazy/')) lazyReads++;
  const path = decodeURIComponent(new URL(req.url, 'http://x').pathname);
  let p = shell && (path === '/' || path === '/index.html') ? shell : join(site, path);
  if (!p.startsWith(site) && p !== shell) { res.writeHead(403).end(); return; }
  let st = null;
  try { st = statSync(p); if (st.isDirectory()) { p = join(p, 'index.html'); st = statSync(p); } } catch { st = null; }
  const h = { 'Cross-Origin-Opener-Policy': 'same-origin', 'Cross-Origin-Embedder-Policy': 'require-corp', 'Cross-Origin-Resource-Policy': 'same-origin' };
  if (!st?.isFile()) { res.writeHead(404, h).end(); return; }
  // Ranges: a bundle split's lazy files are read 1 MiB at a time.
  const r = /^bytes=(\d+)-(\d*)$/.exec(req.headers.range ?? '');
  if (r) {
    const a = Number(r[1]), b = Math.min(r[2] ? Number(r[2]) : st.size - 1, st.size - 1);
    res.writeHead(206, { ...h, 'Content-Type': 'application/octet-stream', 'Content-Length': b - a + 1, 'Content-Range': `bytes ${a}-${b}/${st.size}`, 'Cache-Control': 'no-cache' });
    createReadStream(p, { start: a, end: b }).pipe(res);
    return;
  }
  res.writeHead(200, { ...h, 'Content-Type': types[extname(p)] || 'application/octet-stream', 'Content-Length': st.size, 'Cache-Control': 'no-cache' });
  createReadStream(p).pipe(res);
});
await new Promise((r) => server.listen(askPort, '127.0.0.1', r));
const port = server.address().port;
const profile = mkdtempSync(join(tmpdir(), 'gex-w3-leg-'));
const chrome = spawn(opt('--chrome', process.env.CHROME || '/usr/bin/chromium'), [
  '--headless', '--no-sandbox', '--disable-gpu-sandbox', '--enable-unsafe-webgpu',
  '--use-angle=vulkan', '--enable-features=Vulkan', '--ozone-platform=headless',
  '--ozone-override-screen-size=1280,900', '--window-size=1280,900', '--disable-hang-monitor',
  '--no-first-run', '--no-default-browser-check', '--remote-allow-origins=*',
  '--remote-debugging-port=0', `--user-data-dir=${profile}`, 'about:blank',
], { detached: true, stdio: 'ignore' });
const done = () => {
  try { process.kill(-chrome.pid, 'SIGKILL'); } catch {}
  server.close();
  rmSync(profile, { recursive: true, force: true });
  console.log(failed ? `FAIL ${failed} check(s)` : 'PASS all');
  process.exit(failed ? 1 : 0);
};
let ws = null;
// Chromium writes the port it got, and the browser target's path, here.
const activePort = () => {
  try {
    const [p, path] = readFileSync(join(profile, 'DevToolsActivePort'), 'utf8').split('\n');
    return Number(p) > 0 && path ? `ws://127.0.0.1:${Number(p)}${path}` : null;
  } catch { return null; }
};
for (let i = 0; i < 80 && !ws; i++) { ws = activePort(); if (!ws) await sleep(250); }
if (!check(!!ws, 'Chromium started')) done();
const browser = await Cdp.connect(ws);
const { targetId } = await browser.send('Target.createTarget', { url: 'about:blank' });
const { sessionId: page } = await browser.send('Target.attachToTarget', { targetId, flatten: true });
await browser.send('Runtime.enable', {}, page);
// Every target's console (the page's and its Workers', with the guests'
// stderr): the function-tier units each image hit or missed
// ("[gex-record] unit KEY hit|miss PATH"), and with W3PAGE_CONSOLE=FILE
// every line, timestamped, for a diagnosis.
const units = { hit: new Set(), miss: new Set() };
// The background fill: its end, and a unit of it that failed.
const fillSeen = { done: null, failed: [] };
// A line saying a unit of the fill failed (app.js), not its success summary.
const fillFailure = (line) => /\[app\] (a late unit failed|the fill failed)/.test(line);
for (const [line, want] of [
  ['[app] late units: 60 files from the fill in 196 ms', false],
  ['[app] fill: 132 files, 325.8 MiB in 2064 ms', false],
  ['[app] a late unit failed: units/KEY/c.wasm: CompileError', true],
  ['[app] the fill failed: TypeError: x', true],
]) {
  if (fillFailure(line) !== want) throw new Error(`w3-page: fillFailure(${JSON.stringify(line)}) is not ${want}`);
}
{
  const { appendFileSync } = await import('node:fs');
  const t0 = Date.now();
  browser.on('Runtime.consoleAPICalled', (p) => {
    const line = p.args.map((a) => a.value ?? a.description ?? '').join(' ');
    const u = /\[gex-record\] unit [0-9a-f-]+ (hit|miss) (\S+)/.exec(line);
    if (u) units[u[1]].add(u[2]);
    if (/\[app\] fill: \d+ files/.test(line)) fillSeen.done = line.replace(/^.*\[app\] /, '');
    if (fillFailure(line)) fillSeen.failed.push(line.replace(/^.*\[app\] /, ''));
    if (process.env.W3PAGE_CONSOLE) appendFileSync(process.env.W3PAGE_CONSOLE, `${Date.now() - t0} ${line}\n`);
  });
  browser.on('Target.attachedToTarget', async (p) => {
    const s = p.sessionId;
    await browser.send('Runtime.enable', {}, s).catch(() => {});
    await browser.send('Target.setAutoAttach', { autoAttach: true, waitForDebuggerOnStart: true, flatten: true }, s).catch(() => {});
    // The `[gex-record]` lines are debug output (shims/browser/js/log.mjs): asked for in
    // every target before it runs (a Worker sees neither the page's globals nor its query).
    await browser.send('Runtime.evaluate', { expression: "globalThis.GEX_LOG ??= 'record'" }, s).catch(() => {});
    await browser.send('Runtime.runIfWaitingForDebugger', {}, s).catch(() => {});
  });
  await browser.send('Target.setAutoAttach', { autoAttach: true, waitForDebuggerOnStart: false, flatten: true }, page);
}
const ev = async (e) => (await browser.send('Runtime.evaluate', { expression: e, returnByValue: true, awaitPromise: true }, page).catch(() => ({})))?.result?.value;
// Every load of the top frame: the site's page, and nothing again.
const loads = [];
await browser.send('Page.enable', {}, page);
browser.on('Page.frameNavigated', (p) => { if (!p.frame.parentId) loads.push(new URL(p.frame.url).pathname); });
const t0 = Date.now();
await browser.send('Page.navigate', { url: `http://127.0.0.1:${port}/` }, page);
let d = null;
while (Date.now() - t0 < timeout) {
  const s = await ev('(async () => { const c = globalThis.__gexCompositor; return c ? JSON.stringify(await c.describe()) : null; })()');
  d = s ? JSON.parse(s) : null;
  if (d?.titles?.some((t) => titleRe.test(t))) break;
  const st = await ev('JSON.stringify({ phase: globalThis.__nt?.phase, error: globalThis.__nt?.error, start: globalThis.startError, shown: document.getElementById("error")?.textContent })');
  const sv = st ? JSON.parse(st) : {};
  if (sv.phase === 'failed' || sv.start || sv.shown) { check(false, `the page failed: ${sv.error ?? sv.start ?? sv.shown}`); done(); }
  await sleep(500);
}
const secs = ((Date.now() - t0) / 1000).toFixed(1);
if (!check(!!d?.titles?.some((t) => titleRe.test(t)), `a window titled /${titleRe.source}/ (${secs} s; titles ${JSON.stringify(d?.titles ?? [])})`)) {
  // The programs' own output (Wine's stderr), for a diagnosis.
  if (process.env.W3PAGE_CONSOLE) {
    const { appendFileSync } = await import('node:fs');
    appendFileSync(process.env.W3PAGE_CONSOLE, `== the programs' output\n${(await ev('String(globalThis.__nt?.out ?? "").slice(-200000)')) ?? ''}\n`);
  }
  done();
}
// The site's units are the ones the runtime looks for: a runtime whose unit
// keys moved (gex-cgen's sources changed under it) misses every one and runs
// the program on tier 1 -- it still reaches the window, slowly. An image
// missed is one the recording had no unit for, or one the site does not
// ship; most of what ran must hit.
{
  const hit = units.hit.size;
  const miss = [...units.miss].filter((p) => !units.hit.has(p)).length;
  check(hit > 0 && hit >= 2 * miss, `the units are the runtime's: ${hit} image(s) hit a unit, ${miss} missed`);
}

// 2b. The start-up's status, structured (app.js progress `boot` and `log`,
// from the runtime's own notes, not its console): at the window, processes
// have installed the build's units, an .exe among them, and the log says so.
{
  const pr = JSON.parse((await ev('JSON.stringify(globalThis.__nt?.progress ?? null)')) ?? 'null');
  const b = pr?.boot;
  const started = (pr?.log ?? []).filter((l) => l.kind === 'program').map((l) => l.text);
  check(b && b.units.total > 0 && b.units.done > 0 && b.programs.done > 0 && started.length > 0,
    `the start-up's status: ${b?.units.done ?? 0} of ${b?.units.total ?? 0} images and ${b?.programs.done ?? 0} of ${b?.programs.total ?? 0} programs reached, the latest ${b?.program ?? 'none'} (log: ${started.slice(-4).join('; ') || 'nothing started'})`);
}
// 3. Drawn by the page itself when the compositor is out.
if (off.includes('compositor')) {
  check(d.compositor === 'none', `drawn without the scene compositor (describe().compositor ${d.compositor})`);
  // dom-compositor.js is the bare desktop's own layer: only the scene
  // compositor's files count.
  const comp = await ev("performance.getEntriesByType('resource').some((r) => /\\/(compositor|compositor-worker|gpu-device)-[^/]*\\.js$/.test(r.name))");
  check(!comp, 'the page loaded no compositor script');
}
// 4. No service worker, and no second load.
const regs = await ev('navigator.serviceWorker.getRegistrations().then((r) => r.length)');
check(regs === 0, `no service worker registered (${regs})`);
check(loads.length === 1 && (loads[0] === '/' || loads[0].endsWith('/index.html')), `one load of the site's page, no reload (top-frame loads: ${loads.join(' ')})`);
await sleep(2000);
d = JSON.parse((await ev('(async () => JSON.stringify(await globalThis.__gexCompositor.describe()))()')) ?? '{}');
check((d.lit ?? 0) > 1000, `the window has pixels (lit ${d.lit})`);
if (nt.bundle?.split) console.log(`info: ${lazyReads} lazy read(s) to the window`);
// 1b, run: the fill (with --boot-units, the units deferred to it) ends, and
// none of its units failed.
{
  const fillP = join(site, 'project/build/web/fill.json');
  const fill = existsSync(fillP) ? JSON.parse(readFileSync(fillP, 'utf8')) : null;
  if (fill?.demand) {
    for (let i = 0; i < 240 && !fillSeen.done && !fillSeen.failed.length; i++) await sleep(500);
    check(!!fillSeen.done && !fillSeen.failed.length, `the fill ends with every unit in it whole (${fillSeen.done ?? 'no end in 120 s'}${fillSeen.failed.length ? `; ${fillSeen.failed.slice(0, 3).join('; ')}` : ''})`);
  }
}

// 6. A page of its own: the desktop in its box, at the box's size; kill().
if (shell) {
  const box = JSON.parse(await ev(`(() => { const s = document.getElementById('stage'); const r = s.getBoundingClientRect();
    const d = s.querySelector('.gex-desktop'); const dr = d?.getBoundingClientRect();
    return JSON.stringify({ box: [Math.round(r.left), Math.round(r.top), Math.round(r.width), Math.round(r.height)], desk: dr ? [Math.round(dr.left), Math.round(dr.top), Math.round(dr.width), Math.round(dr.height)] : null }); })()`) ?? '{}');
  check(box.desk && box.desk.join() === box.box.join(), `the desktop fills the page's own box (box ${box.box}, desktop ${box.desk})`);
  check(d.output === `${box.box[2]}x${box.box[3]}`, `the guest's output is the box's size (${d.output})`);
  const st = await ev('(async () => { globalThis.app.kill(); return JSON.stringify({ status: await globalThis.app.exited, onExit: globalThis.exitedWith, title: document.title }); })()');
  const k = st ? JSON.parse(st) : {};
  check(k.status === 137 && k.onExit === 137, `kill() ends the run: exited ${k.status}, onExit ${k.onExit} (title "${k.title}")`);
}

// 5. Placed windows: Wine moves, sizes and stacks them; the page follows.
if (placedChecks) {
  // The placed windows' canvases (bare-desktop.js stacks them from z 2000000),
  // bottom to top, in page coordinates.
  const wins = async () => JSON.parse(await ev(`JSON.stringify([...document.querySelectorAll('canvas')]
    .filter((c) => Number(c.style.zIndex) >= 2000000 && c.isConnected)
    .map((c) => { const r = c.getBoundingClientRect(); return { x: Math.round(r.left), y: Math.round(r.top), w: c.width, h: c.height, z: Number(c.style.zIndex) }; })
    .sort((a, b) => a.z - b.z))`) ?? '[]');
  const main = async () => (await wins()).reduce((a, w) => (!a || w.w * w.h > a.w * a.h ? w : a), null);
  // A held button names itself on every move, as a real mouse's does
  // (Chrome drops an element's pointer capture on a move with no button).
  const mouse = (type, x, y, held) => browser.send('Input.dispatchMouseEvent',
    { type, x, y, button: type === 'mouseMoved' && !held ? 'none' : 'left', buttons: held ? 1 : 0, clickCount: 1 }, page);
  const drag = async (x, y, x2, y2) => {
    await mouse('mouseMoved', x, y, false); await sleep(150);
    await mouse('mousePressed', x, y, true); await sleep(150);
    for (let k = 1; k <= 10; k++) { await mouse('mouseMoved', Math.round(x + (x2 - x) * k / 10), Math.round(y + (y2 - y) * k / 10), true); await sleep(60); }
    await mouse('mouseReleased', x2, y2, false);
  };
  // A caption drag of `steps` moves `ms` apart, recording every animation
  // frame the biggest placed window's place and the pointer's; then a second
  // held still and the release. Per frame, the error is the window's travel
  // less the pointer's: behind (Wine and the page are a frame or two late)
  // is expected, against the drag or ahead of the pointer is the shake.
  const smoothDrag = async (x, y, dx, dy, steps = 80, ms = 4) => {
    await ev(`(() => {
      const D = globalThis.__dragProbe = { frames: [], ptr: null, on: true };
      globalThis.__gexEvLog = [];
      if (!globalThis.__dragProbeListening) {
        globalThis.__dragProbeListening = true;
        // Only the pointer: reading layout here (getBoundingClientRect) would
        // flush it before the page's own listener reads offsetX, and hide a
        // page that reads a stale one (the shake this drag looks for).
        const note = (e) => { if (globalThis.__dragProbe) globalThis.__dragProbe.ptr = [e.clientX, e.clientY]; };
        addEventListener('pointermove', note, { capture: true });
        addEventListener('pointerdown', note, { capture: true });
      }
      const f = (t) => {
        const cs = [...document.querySelectorAll('canvas')].filter((c) => Number(c.style.zIndex) >= 2000000 && c.isConnected);
        const c = cs.reduce((a, c) => (!a || c.width * c.height > a.width * a.height ? c : a), null);
        if (c && D.ptr) { const r = c.getBoundingClientRect(); D.frames.push([r.left, r.top, D.ptr[0], D.ptr[1], t, c.style.zIndex, c.width, c.height, cs.indexOf(c)]); }
        if (D.on) requestAnimationFrame(f);
      };
      requestAnimationFrame(f);
      return 1;
    })()`);
    await mouse('mouseMoved', x, y, false); await sleep(150);
    await mouse('mousePressed', x, y, true); await sleep(150);
    for (let k = 1; k <= steps; k++) {
      await mouse('mouseMoved', Math.round(x + dx * k / steps), Math.round(y + dy * k / steps), true);
      await sleep(ms);
    }
    await sleep(1000);
    await mouse('mouseReleased', x + dx, y + dy, false);
    const fr = (await ev('(() => { const D = globalThis.__dragProbe; D.on = false; return D.frames; })()')) ?? [];
    const evlog = (await ev('(() => { const l = globalThis.__gexEvLog; globalThis.__gexEvLog = null; return l; })()')) ?? [];
    // From the press on: the first frame that saw the pointer at (x, y).
    const i0 = fr.findIndex((f) => f[2] === x && f[3] === y);
    const rows = i0 < 0 ? [] : fr.slice(i0);
    const sx = Math.sign(dx), sy = Math.sign(dy);
    let back = 0, ahead = 0, maxAhead = 0, sumErr = 0, maxErr = 0;
    rows.forEach((f, i) => {
      const ex = (f[0] - rows[0][0]) - (f[2] - x), ey = (f[1] - rows[0][1]) - (f[3] - y);
      const lead = Math.max(ex * sx, ey * sy);
      if (lead > 1) { ahead++; maxAhead = Math.max(maxAhead, lead); }
      const e = Math.hypot(ex, ey); sumErr += e; maxErr = Math.max(maxErr, e);
      if (i && ((f[0] - rows[i - 1][0]) * sx < 0 || (f[1] - rows[i - 1][1]) * sy < 0)) {
        back++;
        // Which frames, to tell a window that went back from a probe that
        // looked at something else: frame time, canvas (z, size), place, pointer.
        for (const g of evlog.filter((g) => g[3] > rows[Math.max(0, i - 3)][4] && g[3] < f[4] + 20)) console.log(`info: drag event: ${g[0]} ${g[1]},${g[2]} t ${g[3].toFixed(1)}`);
        for (const g of rows.slice(Math.max(0, i - 3), i + 3)) console.log(`info: drag back at frame ${i}: t ${g[4].toFixed(1)} z ${g[5]} ${g[6]}x${g[7]} #${g[8]} at ${g[0]},${g[1]} pointer ${g[2]},${g[3]}`);
      }
    });
    return { frames: rows.length, back, ahead, maxAhead: Math.round(maxAhead), meanErr: rows.length ? Math.round(sumErr / rows.length) : 0, maxErr: Math.round(maxErr) };
  };
  const key = async (k, code, vk, modifiers = 0) => {
    if (modifiers & 2) await browser.send('Input.dispatchKeyEvent', { type: 'rawKeyDown', key: 'Control', code: 'ControlLeft', windowsVirtualKeyCode: 17, modifiers: 2 }, page);
    await browser.send('Input.dispatchKeyEvent', { type: 'rawKeyDown', key: k, code, windowsVirtualKeyCode: vk, modifiers }, page);
    await browser.send('Input.dispatchKeyEvent', { type: 'keyUp', key: k, code, windowsVirtualKeyCode: vk, modifiers }, page);
    if (modifiers & 2) await browser.send('Input.dispatchKeyEvent', { type: 'keyUp', key: 'Control', code: 'ControlLeft', windowsVirtualKeyCode: 17, modifiers: 0 }, page);
  };
  // Wait up to 15 s for `want(state)`, then for the state to hold still for
  // a second (a size loop resizes as the drag's moves arrive).
  const until = async (get, want) => {
    let v, same = 0, last = '';
    for (let i = 0; i < 60; i++) {
      v = await get();
      const k = JSON.stringify(v);
      same = k === last ? same + 1 : 0; last = k;
      if (v && want(v) && same >= 4) return v;
      await sleep(250);
    }
    return v;
  };
  const fmt = (w) => w ? `${w.w}x${w.h} at ${w.x},${w.y}` : 'none';
  let m = await main();
  if (check(!!m, `a placed window (${fmt(m)}; the bare desktop and Wine's gex_window_placement_v1)`)) {
    // Not cascaded: Paint's own placement (stage.sh: 0,0) is where it is.
    check(m.x === 0 && m.y === 0, `the first window is where Wine put it, not cascaded (${fmt(m)})`);
    // Caption drag: by (+40, +30).
    const before = m;
    await drag(before.x + Math.round(before.w / 2), before.y + 10, before.x + Math.round(before.w / 2) + 40, before.y + 40);
    m = await until(main, (w) => w.x === before.x + 40 && w.y === before.y + 30);
    check(m.x === before.x + 40 && m.y === before.y + 30 && m.w === before.w, `a caption drag moves it by the drag (${fmt(before)} -> ${fmt(m)})`);
    // Smooth caption drags, out and back, a move every 4 ms (a fast hand,
    // which is when it shook: Wine moved the window between two moves): every
    // animation frame, where the window is against where the pointer is.
    for (const [ddx, ddy] of [[160, 60], [-160, -60]]) {
      const from = m;
      const r = await smoothDrag(from.x + Math.round(from.w / 2), from.y + 10, ddx, ddy);
      m = await until(main, (w) => w.x === from.x + ddx && w.y === from.y + ddy);
      check(r.frames >= 10 && r.back === 0 && r.ahead <= 2 && r.maxErr <= 16,
        `a smooth caption drag by ${ddx},${ddy}: the window follows the pointer (${r.frames} frames: moved against the drag in ${r.back}, ahead of the pointer in ${r.ahead} by up to ${r.maxAhead} px; behind it by ${r.meanErr} px on average, ${r.maxErr} px at most)`);
      check(m.x === from.x + ddx && m.y === from.y + ddy, `... and ends where the drag did (${fmt(from)} -> ${fmt(m)})`);
    }
    // Right border: out by 30 (past the canvas: the pointer stays captured), then in by 60.
    const at = m;
    await drag(at.x + at.w - 3, at.y + Math.round(at.h / 2), at.x + at.w - 3 + 30, at.y + Math.round(at.h / 2));
    m = await until(main, (w) => w.w > at.w);
    check(m.w >= at.w + 25 && m.w <= at.w + 35 && m.x === at.x, `a border drag past its edge grows it (${fmt(at)} -> ${fmt(m)})`);
    const grown = m;
    await drag(grown.x + grown.w - 3, grown.y + Math.round(grown.h / 2), grown.x + grown.w - 3 - 60, grown.y + Math.round(grown.h / 2));
    m = await until(main, (w) => w.w < grown.w);
    check(m.w >= grown.w - 65 && m.w <= grown.w - 55 && m.x === grown.x, `a border drag inside it shrinks it (${fmt(grown)} -> ${fmt(m)})`);
    // An owned dialog stays above its owner when the owner is clicked.
    await key('w', 'KeyW', 87, 2);
    let ws = await until(wins, (l) => l.length >= 2);
    const dlg = ws.length >= 2 ? ws[ws.length - 1] : null;
    if (check(!!dlg && dlg.w * dlg.h < m.w * m.h, `Ctrl+W opens a dialog above Paint (${fmt(dlg)})`)) {
      // Click Paint's client area away from the dialog.
      const cx = m.x + m.w - 40 > dlg.x + dlg.w ? m.x + m.w - 40 : m.x + 40;
      const cy = m.y + m.h - 60;
      await mouse('mouseMoved', cx, cy, false); await sleep(100);
      await mouse('mousePressed', cx, cy, true); await mouse('mouseReleased', cx, cy, false);
      await sleep(2000);
      ws = await wins();
      const top = ws[ws.length - 1];
      check(top && top.w === dlg.w && top.h === dlg.h, `a click on Paint leaves its dialog on top (top ${fmt(top)})`);
      await key('Escape', 'Escape', 27);
      ws = await until(wins, (l) => l.length === 1);
      check(ws.length === 1, `Escape closes the dialog (${ws.length} placed window(s))`);
    }
    // A split bundle's lazy files work: printing's DLLs (winspool.drv,
    // compstui.dll) are not what a run to the window opens, so Ctrl+P reads
    // them from the store before Paint says there is no printer.
    if (nt.bundle?.split) {
      const before = lazyReads;
      await key('p', 'KeyP', 80, 2);
      ws = await until(wins, (l) => l.length >= 2);
      check(ws.length >= 2 && lazyReads > before, `Ctrl+P reads lazy files and answers (${lazyReads - before} lazy read(s), ${ws.length} placed window(s))`);
      await key('Enter', 'Enter', 13);
      await until(wins, (l) => l.length === 1);
    }
    // The right button is the guest's: a right click on the caption opens
    // Wine's window menu (WM_CONTEXTMENU on the caption, so the client saw
    // BTN_RIGHT go down and up), and the browser's own context menu is
    // cancelled (it opens on the press on Linux and took the release).
    await ev(`(() => { globalThis.__ctxMenus = []; addEventListener('contextmenu', (e) => globalThis.__ctxMenus.push(e.defaultPrevented)); return 1; })()`);
    m = await main();
    const rx = m.x + Math.round(m.w / 2), ry = m.y + 10;
    const right = (type, held) => browser.send('Input.dispatchMouseEvent',
      { type, x: rx, y: ry, button: 'right', buttons: held ? 2 : 0, clickCount: 1 }, page);
    await mouse('mouseMoved', rx, ry, false); await sleep(150);
    await right('mousePressed', true); await sleep(100);
    await right('mouseReleased', false);
    ws = await until(wins, (l) => l.length >= 2);
    const menus = (await ev('JSON.stringify(globalThis.__ctxMenus)')) ?? '[]';
    check(JSON.parse(menus).length >= 1 && JSON.parse(menus).every(Boolean), `the browser's context menu is cancelled over the desktop (contextmenu events, defaultPrevented: ${menus})`);
    check(ws.length >= 2, `a right click on the caption opens the window menu: the guest has the right button (${ws.length} placed window(s))`);
    await key('Escape', 'Escape', 27);
    await until(wins, (l) => l.length === 1);
  }
}
done();
