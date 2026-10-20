#!/usr/bin/env node
// tests/eject/click-scroll.sh's run: the page it built (SITE), served
// cross-origin isolated, in headless Chromium with a GPU at a 1280x720
// viewport. The X client xinput-echo prints what it gets. The pointer moves
// to a spot near the top of its window ("motion X Y" in the guest), then
// presses there ("button press 1 at X Y").
//
//   node tests/eject/click-scroll.mjs SITE [--chrome PATH]
//
// Checks: the press reaches the guest at the window-local spot pressed
// (within 1 px, the window's border) and where its last motion line said the
// pointer was; window.scrollY is 0 after it. Prints PASS/FAIL lines and
// "ok eject-click-scroll" or "FAIL eject-click-scroll".
import { spawn } from 'node:child_process';
import { createServer } from 'node:http';
import { mkdtempSync, readFileSync, rmSync, statSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { extname, join, normalize, resolve } from 'node:path';
import { Cdp, browserWsUrl } from '../../web/test/cdp.mjs';

const av = process.argv.slice(2);
const opt = (n, d) => { const i = av.indexOf(n); return i >= 0 ? av[i + 1] : d; };
const site = resolve(av[0] ?? '');
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
let failed = 0;
const check = (ok, what) => { console.log(`${ok ? 'PASS' : 'FAIL'} ${what}`); if (!ok) failed++; return ok; };

const TYPES = { '.html': 'text/html', '.mjs': 'text/javascript', '.js': 'text/javascript', '.wasm': 'application/wasm', '.json': 'application/json' };
const server = createServer((req, res) => {
  const p = normalize(decodeURIComponent(new URL(req.url, 'http://x').pathname)).replace(/^(\.\.[/\\])+/, '');
  const f = join(site, p.endsWith('/') ? p + 'index.html' : p);
  let st = null;
  try { st = statSync(f); } catch {}
  if (!st?.isFile()) { res.writeHead(404); res.end(); return; }
  res.writeHead(200, {
    'Content-Type': TYPES[extname(f)] ?? 'application/octet-stream', 'Content-Length': st.size,
    'Cross-Origin-Opener-Policy': 'same-origin', 'Cross-Origin-Embedder-Policy': 'require-corp',
    'Cross-Origin-Resource-Policy': 'same-origin', 'Cache-Control': 'no-store',
  });
  res.end(readFileSync(f));
});
await new Promise((r) => server.listen(0, '127.0.0.1', r));
const url = `http://127.0.0.1:${server.address().port}/`;

const profile = mkdtempSync(join(tmpdir(), 'gex-click-scroll-'));
const cport = 9800 + Math.floor(Math.random() * 600);
const chrome = spawn(opt('--chrome', process.env.CHROME || '/usr/bin/chromium'), [
  '--headless', '--no-sandbox', '--disable-gpu-sandbox', '--enable-unsafe-webgpu', '--use-angle=vulkan', '--enable-features=Vulkan',
  '--ozone-platform=headless', '--ozone-override-screen-size=1280,720', '--window-size=1280,720',
  '--disable-hang-monitor', '--no-first-run', '--no-default-browser-check', '--remote-allow-origins=*',
  `--remote-debugging-port=${cport}`, `--user-data-dir=${profile}`, 'about:blank',
], { detached: true, stdio: 'ignore' });
let out = '';
const done = () => {
  try { process.kill(-chrome.pid, 'SIGKILL'); } catch {}
  server.close();
  rmSync(profile, { recursive: true, force: true });
  if (failed && out) console.log('guest output (tail):\n' + out.slice(-600));
  console.log(failed ? `FAIL eject-click-scroll (${failed} check(s))` : 'ok eject-click-scroll');
  process.exit(failed ? 1 : 0);
};
for (const e of ['uncaughtException', 'unhandledRejection']) process.on(e, (x) => { check(false, `driver: ${x?.message ?? x}`); done(); });

let ws = null;
for (let i = 0; i < 60 && !ws; i++) { await sleep(250); ws = await browserWsUrl(cport); }
if (!check(!!ws, 'Chromium started')) done();
const cdp = await Cdp.connect(ws);
const { targetId } = await cdp.send('Target.createTarget', { url: 'about:blank' });
const { sessionId } = await cdp.send('Target.attachToTarget', { targetId, flatten: true });
const s = (m, p = {}) => cdp.send(m, p, sessionId);
await s('Runtime.enable');
await s('Page.enable');
// The viewport the leg is about: 34 px bar + 720 px output does not fit.
await s('Emulation.setDeviceMetricsOverride', { width: 1280, height: 720, deviceScaleFactor: 1, mobile: false });
const ev = async (x) => {
  const r = await s('Runtime.evaluate', { expression: x, returnByValue: true, awaitPromise: true });
  if (r.exceptionDetails) throw new Error(`${x.slice(0, 60)}: ${r.exceptionDetails.exception?.description ?? r.exceptionDetails.text}`);
  return r.result?.value;
};
await s('Page.navigate', { url });

// Up: the window mapped, and the compositor holding a toplevel the size the
// client asked for (800x600).
let win = null;
const t0 = Date.now();
while (Date.now() - t0 < 200e3) {
  await sleep(1000);
  out = (await ev('globalThis.__test?.out() ?? ""').catch(() => '')) ?? '';
  const status = await ev('globalThis.__test?.status() ?? ""').catch(() => '');
  if (/^(failed|exited)/.test(status ?? '')) { check(false, `page: ${status}`); done(); }
  if (!out.includes('window mapped')) continue;
  const d = await ev('globalThis.__gexCompositor?.describe({ pixels: false })').catch(() => null);
  const line = d?.list?.find((l) => / role=1 800x600@/.test(l));
  const m = line && / 800x600@(-?\d+),(-?\d+)/.exec(line);
  if (m) { win = { x: Number(m[1]), y: Number(m[2]), line }; break; }
}
if (!check(!!win, `the X window is up${win ? ` (${win.line})` : ''}`)) done();
await sleep(1000);

// The output's place on the page, before anything is pressed.
const rect = await ev(`(() => { const c = document.querySelector('#stage canvas'); const r = c.getBoundingClientRect();
  return { left: r.left, top: r.top, w: r.width, h: r.height, cw: c.width, ch: c.height, sh: document.scrollingElement.scrollHeight, y: scrollY }; })()`);
console.log(`canvas at ${rect.left},${rect.top} ${rect.w}x${rect.h} (backing ${rect.cw}x${rect.ch}), page height ${rect.sh}, scrollY ${rect.y}`);
check(rect.sh > 720 && rect.y === 0, 'the page is taller than the viewport and not scrolled');
// 12 px into the window's top band.
const want = { x: 40, y: 12 };
const sx = rect.w / rect.cw, sy = rect.h / rect.ch;
const px = { x: rect.left + (win.x + want.x) * sx, y: rect.top + (win.y + want.y) * sy };
console.log(`pointer to ${px.x},${px.y} (window-local ${want.x},${want.y})`);
const mouse = (type, extra = {}) => s('Input.dispatchMouseEvent', { type, x: px.x, y: px.y, ...extra });
const before = (await ev('globalThis.__test.out()')).length;
await mouse('mouseMoved');
// The guest prints every 20th motion; the first one after the move, if any,
// is where it saw the pointer.
let moved = null;
for (let i = 0; i < 6 && !moved; i++) {
  await sleep(500);
  out = await ev('globalThis.__test.out()');
  const m = /^motion (-?\d+) (-?\d+)$/m.exec(out.slice(before));
  if (m) moved = { x: Number(m[1]), y: Number(m[2]) };
}
console.log(`the guest saw the pointer at ${moved ? `${moved.x},${moved.y}` : '(no motion line)'}`);
await mouse('mousePressed', { button: 'left', buttons: 1, clickCount: 1 });
await sleep(150);
await mouse('mouseReleased', { button: 'left', buttons: 0, clickCount: 1 });
let press = null;
for (let i = 0; i < 20 && !press; i++) {
  await sleep(500);
  out = await ev('globalThis.__test.out()');
  const m = /^button press 1 at (-?\d+) (-?\d+)$/m.exec(out.slice(before));
  if (m) press = { x: Number(m[1]), y: Number(m[2]) };
}
const scrollY = await ev('scrollY');
const d = await ev('globalThis.__gexCompositor.describe({ pixels: false })');
// Within 1 px of the spot aimed at (the window's 1 px border), and where the
// guest last saw the pointer.
const near = (a, b) => !!a && !!b && Math.abs(a.x - b.x) <= 1 && Math.abs(a.y - b.y) <= 1;
check(near(press, want), `the press reached the guest at the spot pressed: ${press ? `${press.x},${press.y}` : 'no press'}, aimed at ${want.x},${want.y}`);
if (moved) check(press?.x === moved.x && press?.y === moved.y, `the press is where the guest saw the pointer (${moved.x},${moved.y})`);
check(scrollY === 0, `window.scrollY stays 0 (is ${scrollY})`);
console.log(`describe().pointer: ${JSON.stringify(d?.pointer ?? null)}`);
done();
