#!/usr/bin/env node
// The ejected W3 page leg: a site from web/eject-site.mjs of a W3 project
// (docs/eject.md, "W3 pages") reaches its window in headless Chromium, built
// with the features its project left out really gone.
//
//   node tests/eject/w3-page.mjs SITE [--title RE] [--timeout S] [--port N] [--chrome PATH] [--placed]
//
// Checks, in order:
//   1. the site's nt.json says kind w3, and for each feature it says is off,
//      the files that feature is are not in the site ("compositor":
//      assets/compositor-*, assets/compositor-worker-*, assets/gpu-device-*;
//      "gl": gex/gex-wgpu-replay.*);
//   2. served (cross-origin isolated) and opened at its launcher, a toplevel
//      titled RE (default "Untitled - Paint") appears;
//   3. with "compositor" off, what draws it is the page's bare desktop
//      (`describe().compositor === 'none'`), and the window has pixels;
//   4. no service worker: the site ships no sw.js, none is registered, and
//      the first load is the only one (the launcher once, nt.html once);
//   5. with --placed (Paint, "compositor" off): the bare desktop leaves a
//      window its client places (gex_window_placement_v1) to Wine. A caption
//      drag moves it by the drag, a border drag past its edge grows it and
//      one inside shrinks it (Win32's own SC_MOVE/SC_SIZE loops, the pointer
//      held by the canvas), and a click on Paint leaves its owned dialog
//      (Ctrl+W, Resize and Skew) above it: the page neither cascades nor
//      raises a placed window.
// Prints PASS or FAIL lines; exits 0 only when all pass.

import { spawn } from 'node:child_process';
import { createReadStream, existsSync, mkdtempSync, readdirSync, readFileSync, rmSync, statSync } from 'node:fs';
import http from 'node:http';
import { tmpdir } from 'node:os';
import { extname, join, resolve } from 'node:path';
import { Cdp, browserWsUrl } from '../../web/test/cdp.mjs';

const av = process.argv.slice(2);
const opt = (n, d) => { const i = av.indexOf(n); return i >= 0 ? av[i + 1] : d; };
const site = resolve(av.find((a, i) => !a.startsWith('--') && !['--title', '--timeout', '--port', '--chrome'].includes(av[i - 1])) ?? '');
const placedChecks = av.includes('--placed');
const titleRe = new RegExp(opt('--title', 'Untitled - Paint'));
const timeout = Number(opt('--timeout', '400')) * 1000;
const port = Number(opt('--port', '8731'));
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
check(!existsSync(join(site, 'sw.js')), 'the site ships no service worker (no sw.js)');
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
  let p = join(site, decodeURIComponent(new URL(req.url, 'http://x').pathname));
  if (!p.startsWith(site)) { res.writeHead(403).end(); return; }
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
await new Promise((r) => server.listen(port, '127.0.0.1', r));
const profile = mkdtempSync(join(tmpdir(), 'gex-w3-leg-'));
const cport = port + 1;
const chrome = spawn(opt('--chrome', process.env.CHROME || '/usr/bin/chromium'), [
  '--headless', '--no-sandbox', '--disable-gpu-sandbox', '--enable-unsafe-webgpu',
  '--use-angle=vulkan', '--enable-features=Vulkan', '--ozone-platform=headless',
  '--ozone-override-screen-size=1280,900', '--window-size=1280,900', '--disable-hang-monitor',
  '--no-first-run', '--no-default-browser-check', '--remote-allow-origins=*',
  `--remote-debugging-port=${cport}`, `--user-data-dir=${profile}`, 'about:blank',
], { detached: true, stdio: 'ignore' });
const done = () => {
  try { process.kill(-chrome.pid, 'SIGKILL'); } catch {}
  server.close();
  rmSync(profile, { recursive: true, force: true });
  console.log(failed ? `FAIL ${failed} check(s)` : 'PASS all');
  process.exit(failed ? 1 : 0);
};
let ws = null;
for (let i = 0; i < 80 && !ws; i++) { ws = await browserWsUrl(cport); if (!ws) await sleep(250); }
if (!check(!!ws, 'Chromium started')) done();
const browser = await Cdp.connect(ws);
const { targetId } = await browser.send('Target.createTarget', { url: 'about:blank' });
const { sessionId: page } = await browser.send('Target.attachToTarget', { targetId, flatten: true });
await browser.send('Runtime.enable', {}, page);
const ev = async (e) => (await browser.send('Runtime.evaluate', { expression: e, returnByValue: true, awaitPromise: true }, page).catch(() => ({})))?.result?.value;
// Every load of the top frame: the launcher, then nt.html, and nothing again.
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
  const st = await ev('JSON.stringify({ phase: globalThis.__nt?.phase, error: globalThis.__nt?.error })');
  if (st && JSON.parse(st).phase === 'failed') { check(false, `the page failed: ${JSON.parse(st).error}`); done(); }
  await sleep(500);
}
const secs = ((Date.now() - t0) / 1000).toFixed(1);
if (!check(!!d?.titles?.some((t) => titleRe.test(t)), `a window titled /${titleRe.source}/ (${secs} s; titles ${JSON.stringify(d?.titles ?? [])})`)) done();

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
const launcher = loads.filter((u) => u === '/' || u.endsWith('/index.html')).length;
const ntLoads = loads.filter((u) => u.endsWith('/nt.html')).length;
check(launcher === 1 && ntLoads === 1, `one load, no reload (top-frame loads: ${loads.join(' ')})`);
await sleep(2000);
d = JSON.parse((await ev('(async () => JSON.stringify(await globalThis.__gexCompositor.describe()))()')) ?? '{}');
check((d.lit ?? 0) > 1000, `the window has pixels (lit ${d.lit})`);
if (nt.bundle?.split) console.log(`info: ${lazyReads} lazy read(s) to the window`);

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
  }
}
done();
