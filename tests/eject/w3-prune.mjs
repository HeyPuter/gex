#!/usr/bin/env node
// A pruned-function hunt: drive an ejected W3 Paint site (`"prune"` in
// user/eject.json) through the program's real surface and fail on any
// function the proof left out that runs.
//
//   node tests/eject/w3-prune.mjs SITE [--title RE] [--timeout S] [--port N]
//        [--chrome PATH] [--console FILE] [--shots DIR] [--quick]
//
// A pruned slot's stub calls `gex.pruned(slot)`, which names the function's
// guest address on the console and traps (gex_split::prune,
// shims/browser/js/gex.mjs). So a wrong proof does not corrupt anything: it
// says which address, in which unit, and this leg collects every such line
// and prints it with the step that provoked it. A hit is a bug in the
// analysis (crates/gex-eject/src/fanout.rs), never something to add to a
// keep-list.
//
// The steps, each followed by Escape so a modal dialog cannot swallow the
// rest: the ribbon's tabs; a grid of clicks over the ribbon band of each tab
// (what reaches uiribbon.dll, which nothing imports and everything in Paint's
// chrome goes through); strokes drawn on the canvas; the File menu; and the
// dialogs behind Paint's accelerators, the Open and Save ones included.
// --shots DIR writes a PNG per step, to see what the clicks landed on.

import { spawn } from 'node:child_process';
import { createReadStream, existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, statSync, writeFileSync, appendFileSync } from 'node:fs';
import http from 'node:http';
import { tmpdir } from 'node:os';
import { extname, join, resolve } from 'node:path';
import { Cdp } from '../../web/test/cdp.mjs';

const av = process.argv.slice(2);
const opt = (n, d) => { const i = av.indexOf(n); return i >= 0 ? av[i + 1] : d; };
const site = resolve(av.find((a, i) => !a.startsWith('--') && !['--title', '--timeout', '--port', '--chrome', '--console', '--shots'].includes(av[i - 1])) ?? '');
const titleRe = new RegExp(opt('--title', 'Untitled - Paint'));
const timeout = Number(opt('--timeout', '400')) * 1000;
const askPort = Number(opt('--port', '0'));
const shots = opt('--shots', null);
const quick = av.includes('--quick');
const consoleFile = opt('--console', null);
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
let failed = 0;
const check = (ok, what) => { console.log(`${ok ? 'PASS' : 'FAIL'} ${what}`); if (!ok) failed++; return ok; };
if (shots) mkdirSync(shots, { recursive: true });
if (consoleFile) writeFileSync(consoleFile, '');

// ---------------------------------------------------------------------------
// Served, cross-origin isolated, as the site's own server does.
const types = { '.html': 'text/html', '.js': 'text/javascript', '.mjs': 'text/javascript', '.wasm': 'application/wasm', '.json': 'application/json', '.css': 'text/css' };
const server = http.createServer((req, res) => {
  const path = decodeURIComponent(new URL(req.url, 'http://x').pathname);
  let p = join(site, path);
  if (!p.startsWith(site)) { res.writeHead(403).end(); return; }
  let st = null;
  try { st = statSync(p); if (st.isDirectory()) { p = join(p, 'index.html'); st = statSync(p); } } catch { st = null; }
  const h = { 'Cross-Origin-Opener-Policy': 'same-origin', 'Cross-Origin-Embedder-Policy': 'require-corp', 'Cross-Origin-Resource-Policy': 'same-origin' };
  if (!st?.isFile()) { res.writeHead(404, h).end(); return; }
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

const profile = mkdtempSync(join(tmpdir(), 'gex-w3-prune-'));
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
const activePort = () => {
  try {
    const [p, path] = readFileSync(join(profile, 'DevToolsActivePort'), 'utf8').split('\n');
    return Number(p) > 0 && path ? `ws://127.0.0.1:${Number(p)}${path}` : null;
  } catch { return null; }
};
let ws = null;
for (let i = 0; i < 80 && !ws; i++) { ws = activePort(); if (!ws) await sleep(250); }
if (!check(!!ws, 'Chromium started')) done();
const browser = await Cdp.connect(ws);
const { targetId } = await browser.send('Target.createTarget', { url: 'about:blank' });
const { sessionId: page } = await browser.send('Target.attachToTarget', { targetId, flatten: true });
await browser.send('Runtime.enable', {}, page);

// Every target's console: gex runs each guest process in its own Worker, so
// the trap that matters happens there. FLAT auto-attach brings it here.
const pruned = [];
const traps = [];
// Declared before the handlers: one can fire while auto-attach is still
// being set up, and a step name read in its temporal dead zone would throw
// inside the handler, losing the line.
let step = 'load';
{
  const t0 = Date.now();
  browser.on('Runtime.consoleAPICalled', (p) => {
    const line = p.args.map((a) => a.value ?? a.description ?? '').join(' ');
    if (line.includes('[pruned]')) pruned.push({ at: step, line });
    if (consoleFile) appendFileSync(consoleFile, `${Date.now() - t0} ${line}\n`);
  });
  browser.on('Runtime.exceptionThrown', (p) => {
    const t = p.exceptionDetails?.exception?.description ?? p.exceptionDetails?.text ?? '';
    if (/pruned function/.test(t)) traps.push({ at: step, line: t.split('\n')[0] });
    if (consoleFile) appendFileSync(consoleFile, `${Date.now() - t0} EXCEPTION ${t.split('\n')[0]}\n`);
  });
  browser.on('Target.attachedToTarget', async (p) => {
    const s = p.sessionId;
    await browser.send('Runtime.enable', {}, s).catch(() => {});
    await browser.send('Target.setAutoAttach', { autoAttach: true, waitForDebuggerOnStart: true, flatten: true }, s).catch(() => {});
    await browser.send('Runtime.runIfWaitingForDebugger', {}, s).catch(() => {});
  });
  await browser.send('Target.setAutoAttach', { autoAttach: true, waitForDebuggerOnStart: false, flatten: true }, page);
}
const ev = async (e) => (await browser.send('Runtime.evaluate', { expression: e, returnByValue: true, awaitPromise: true }, page).catch(() => ({})))?.result?.value;

// ---------------------------------------------------------------------------
// 1. The window.
await browser.send('Page.enable', {}, page);
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
if (!check(!!d?.titles?.some((t) => titleRe.test(t)), `a window titled /${titleRe.source}/ (${secs} s; titles ${JSON.stringify(d?.titles ?? [])})`)) done();

// ---------------------------------------------------------------------------
// The input the bare desktop takes, as a real mouse sends it: a held button
// names itself on every move (Chrome drops pointer capture on a buttonless
// move).
const mouse = (type, x, y, held) => browser.send('Input.dispatchMouseEvent',
  { type, x, y, button: type === 'mouseMoved' && !held ? 'none' : 'left', buttons: held ? 1 : 0, clickCount: 1 }, page);
const click = async (x, y) => {
  await mouse('mouseMoved', x, y, false); await sleep(40);
  await mouse('mousePressed', x, y, true); await sleep(60);
  await mouse('mouseReleased', x, y, false); await sleep(120);
};
const drag = async (x, y, x2, y2, n = 8) => {
  await mouse('mouseMoved', x, y, false); await sleep(40);
  await mouse('mousePressed', x, y, true); await sleep(60);
  for (let k = 1; k <= n; k++) { await mouse('mouseMoved', Math.round(x + (x2 - x) * k / n), Math.round(y + (y2 - y) * k / n), true); await sleep(30); }
  await mouse('mouseReleased', x2, y2, false); await sleep(120);
};
const key = async (k, code, vk, modifiers = 0) => {
  if (modifiers & 2) await browser.send('Input.dispatchKeyEvent', { type: 'rawKeyDown', key: 'Control', code: 'ControlLeft', windowsVirtualKeyCode: 17, modifiers: 2 }, page);
  await browser.send('Input.dispatchKeyEvent', { type: 'rawKeyDown', key: k, code, windowsVirtualKeyCode: vk, modifiers }, page);
  await browser.send('Input.dispatchKeyEvent', { type: 'keyUp', key: k, code, windowsVirtualKeyCode: vk, modifiers }, page);
  if (modifiers & 2) await browser.send('Input.dispatchKeyEvent', { type: 'keyUp', key: 'Control', code: 'ControlLeft', windowsVirtualKeyCode: 17, modifiers: 0 }, page);
  await sleep(200);
};
const esc = () => key('Escape', 'Escape', 27);
const shot = async (name) => {
  if (!shots) return;
  const r = await browser.send('Page.captureScreenshot', { format: 'png' }, page).catch(() => null);
  if (r?.data) writeFileSync(join(shots, `${name}.png`), Buffer.from(r.data, 'base64'));
};
// The placed windows' canvases (bare-desktop.js stacks them from z 2000000).
const wins = async () => JSON.parse(await ev(`JSON.stringify([...document.querySelectorAll('canvas')]
  .filter((c) => Number(c.style.zIndex) >= 2000000 && c.isConnected)
  .map((c) => { const r = c.getBoundingClientRect(); return { x: Math.round(r.left), y: Math.round(r.top), w: c.width, h: c.height, z: Number(c.style.zIndex) }; })
  .sort((a, b) => a.z - b.z))`) ?? '[]');
const main = async () => (await wins()).reduce((a, w) => (!a || w.w * w.h > a.w * a.h ? w : a), null);

const w = await main();
if (!check(!!w, `Paint's window is on the desktop (${JSON.stringify(w)})`)) done();
console.log(`info: window ${w.w}x${w.h} at ${w.x},${w.y}`);
await shot('00-loaded');

// A step: run it, let the program settle, note any pruned line it provoked.
const seen = () => pruned.length + traps.length;
const steps = [];
const run = async (name, fn) => {
  step = name;
  const before = seen();
  try { await fn(); } catch (e) { console.log(`info: step ${name} threw ${e.message}`); }
  await sleep(quick ? 150 : 400);
  const hit = seen() - before;
  steps.push({ name, hit });
  if (hit) console.log(`FAIL step ${name}: ${hit} pruned function(s) ran`);
  await shot(name);
};

// ---------------------------------------------------------------------------
// 2. The ribbon. Paint's chrome is uiribbon.dll's, reached only through COM,
// so the tabs and a grid over the band are what exercise it.
const X = (f) => Math.round(w.x + f * w.w);
const Y = (f) => Math.round(w.y + f * w.h);
// The tab row sits just under the caption; the band under it.
const tabY = Y(0.045);
for (const [i, f] of [0.03, 0.08, 0.13, 0.18].entries()) {
  await run(`tab-${i}`, () => click(X(f), tabY));
}
const bandYs = [Y(0.085), Y(0.12), Y(0.155)];
for (const [ti, tf] of [0.08, 0.13].entries()) {
  await run(`tab-select-${ti}`, async () => { await click(X(tf), tabY); await sleep(300); });
  for (const [yi, by] of bandYs.entries()) {
    await run(`band-${ti}-${yi}`, async () => {
      for (let f = 0.02; f < 0.98; f += quick ? 0.12 : 0.045) {
        await click(X(f), by);
        await esc();
      }
    });
  }
}

// 3. Drawing: strokes with whatever tool the band left selected, then the
// pencil and the fill, and a shape dragged out.
await run('draw-strokes', async () => {
  const cy = Y(0.55), cx = X(0.3);
  await drag(cx, cy, cx + 160, cy + 90);
  await drag(cx + 20, cy + 100, cx + 200, cy - 40, 12);
  await drag(X(0.6), Y(0.4), X(0.8), Y(0.75), 16);
});
await run('draw-clicks', async () => {
  for (let k = 0; k < 8; k++) await click(X(0.25 + 0.05 * k), Y(0.6));
});

// 4. The File menu: Paint's application button, and its items.
await run('file-menu', async () => {
  await click(X(0.015), tabY);
  await sleep(600);
  for (const f of [0.1, 0.16, 0.22, 0.28, 0.34, 0.4]) { await click(X(0.05), Y(f)); await sleep(250); await esc(); }
  await esc();
});

// 5. The dialogs behind the accelerators: Open and Save among them. Each is
// dismissed, so the next one still gets its clicks.
const keys = [
  ['select-all', 'a', 'KeyA', 65, 2],
  ['copy', 'c', 'KeyC', 67, 2],
  ['paste', 'v', 'KeyV', 86, 2],
  ['undo', 'z', 'KeyZ', 90, 2],
  ['redo', 'y', 'KeyY', 89, 2],
  ['resize-skew', 'w', 'KeyW', 87, 2],
  ['properties', 'e', 'KeyE', 69, 2],
  ['open', 'o', 'KeyO', 79, 2],
  ['save', 's', 'KeyS', 83, 2],
  ['print', 'p', 'KeyP', 80, 2],
  ['new', 'n', 'KeyN', 78, 2],
  ['zoom-in', '+', 'NumpadAdd', 107, 2],
  ['zoom-out', '-', 'NumpadSubtract', 109, 2],
  ['rotate', 'r', 'KeyR', 82, 2],
  ['grid', 'g', 'KeyG', 71, 2],
  ['full-screen', 'F11', 'F11', 122, 0],
];
for (const [name, k, code, vk, mod] of keys) {
  await run(`key-${name}`, async () => { await key(k, code, vk, mod); await sleep(600); await esc(); await esc(); });
}

// 6. Still alive, and still Paint.
d = JSON.parse((await ev('(async () => JSON.stringify(await globalThis.__gexCompositor.describe()))()')) ?? '{}');
check((d.lit ?? 0) > 1000, `the window still has pixels after the pass (lit ${d.lit})`);
check(!!d?.titles?.some((t) => /Paint/.test(t)), `a Paint window is still there (titles ${JSON.stringify(d?.titles ?? [])})`);

// ---------------------------------------------------------------------------
// The verdict: any pruned function that ran is a soundness bug.
const all = [...pruned, ...traps];
check(all.length === 0, `no pruned function ran over ${steps.length} step(s)`);
if (all.length) {
  console.log('pruned functions that ran:');
  const by = new Map();
  for (const p of all) by.set(p.line, (by.get(p.line) ?? 0) + 1);
  for (const [line, n] of by) console.log(`  [${n}x] ${line}`);
  console.log('steps that provoked one:');
  for (const s of steps.filter((s) => s.hit)) console.log(`  ${s.name}: ${s.hit}`);
}
done();
