#!/usr/bin/env node
// Record a W3 page session for gex-eject (docs/eject.md, "W3 pages").
//
// A W3 program (Wine as native wasm32, every Wine process a guest process)
// runs in nt.html like any page. This opens one in headless Chromium, waits
// for a window title, and writes OUT/w3-session.json: how the page was
// started (nt-page.js's `__nt.session`), and from every Worker's
// `[gex-record]` lines (gex.mjs) which processes ran, which function-tier
// units each image used or missed, and which native wasm32 modules loaded;
// and, at the end, the files the run opened (nt-page.js `__ntOpened`, the
// kernel's read set), which the site's bundle split keeps in the bundle.
// `gexc eject OUT --out PROJECT` then makes a project of it.
//
//   node tools/eject/record-w3.mjs URL OUT [--serve DIR] [--port N] [--title RE] [--timeout S]
//            [--bundle PATH] [--settle S] [--chrome PATH]
//
// URL: the page, e.g. http://127.0.0.1:8000/ for a site whose index opens
// nt.html, or an nt.html?... URL itself. --serve DIR: serve DIR there first,
// cross-origin isolated. --bundle PATH: the bundle the page fetched, on this
// machine (its URL is all the page knows); the eject reads it from there.
// --title: a regular expression a toplevel's title must match (default
// "Untitled - Paint"); --settle: seconds to keep recording after it (5).

import { spawn } from 'node:child_process';
import { createReadStream, mkdirSync, mkdtempSync, rmSync, statSync, writeFileSync } from 'node:fs';
import http from 'node:http';
import { tmpdir } from 'node:os';
import { extname, join, resolve } from 'node:path';
import { Cdp, browserWsUrl } from '../../web/test/cdp.mjs';

const av = process.argv.slice(2);
const opt = (n, d) => { const i = av.indexOf(n); return i >= 0 ? av[i + 1] : d; };
const named = ['--serve', '--port', '--title', '--timeout', '--bundle', '--settle', '--chrome'];
const [url, out] = av.filter((a, i) => !a.startsWith('--') && !named.includes(av[i - 1]));
if (!url || !out) {
  console.error('usage: node tools/eject/record-w3.mjs URL OUT [--serve DIR] [--port N] [--title RE] [--timeout S] [--bundle PATH] [--settle S] [--chrome PATH]');
  process.exit(2);
}
const titleRe = new RegExp(opt('--title', 'Untitled - Paint'));
const timeout = Number(opt('--timeout', '400')) * 1000;
const settle = Number(opt('--settle', '5')) * 1000;
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
mkdirSync(out, { recursive: true });

// A static server for --serve, with the isolation headers SharedArrayBuffer needs.
let server = null;
if (opt('--serve', null)) {
  const root = resolve(opt('--serve', null));
  const types = { '.html': 'text/html', '.js': 'text/javascript', '.mjs': 'text/javascript', '.wasm': 'application/wasm', '.json': 'application/json', '.css': 'text/css' };
  server = http.createServer((req, res) => {
    let p = join(root, decodeURIComponent(new URL(req.url, 'http://x').pathname));
    if (!p.startsWith(root)) { res.writeHead(403).end(); return; }
    let st = null;
    try { st = statSync(p); if (st.isDirectory()) { p = join(p, 'index.html'); st = statSync(p); } } catch { st = null; }
    const h = { 'Cross-Origin-Opener-Policy': 'same-origin', 'Cross-Origin-Embedder-Policy': 'require-corp', 'Cross-Origin-Resource-Policy': 'same-origin' };
    if (!st?.isFile()) { res.writeHead(404, h).end(); return; }
    res.writeHead(200, { ...h, 'Content-Type': types[extname(p)] || 'application/octet-stream', 'Content-Length': st.size, 'Cache-Control': 'no-cache' });
    createReadStream(p).pipe(res);
  });
  const u = new URL(url);
  await new Promise((r) => server.listen(Number(u.port || 80), u.hostname, r));
}

const port = Number(opt('--port', '9711'));
const profile = mkdtempSync(join(tmpdir(), 'gex-record-w3-'));
const chrome = spawn(opt('--chrome', process.env.CHROME || '/usr/bin/chromium'), [
  '--headless', '--no-sandbox', '--disable-gpu-sandbox', '--enable-unsafe-webgpu',
  '--use-angle=vulkan', '--enable-features=Vulkan', '--ozone-platform=headless',
  '--ozone-override-screen-size=1280,900', '--window-size=1280,900', '--disable-hang-monitor',
  '--no-first-run', '--no-default-browser-check', '--remote-allow-origins=*',
  `--remote-debugging-port=${port}`, `--user-data-dir=${profile}`, 'about:blank',
], { detached: true, stdio: 'ignore' });
const done = (code) => {
  try { process.kill(-chrome.pid, 'SIGKILL'); } catch {}
  server?.close();
  rmSync(profile, { recursive: true, force: true });
  process.exit(code);
};

let ws = null;
for (let i = 0; i < 80 && !ws; i++) { ws = await browserWsUrl(port); if (!ws) await sleep(250); }
if (!ws) { console.error('record-w3: Chromium did not start'); done(1); }
const browser = await Cdp.connect(ws);
const { targetId } = await browser.send('Target.createTarget', { url: 'about:blank' });
const { sessionId: page } = await browser.send('Target.attachToTarget', { targetId, flatten: true });

// Every Worker's console, from its first line: auto-attach, paused at start.
const units = new Map();     // key -> { path, hit }
const processes = new Map(); // main image -> cmdline
const modules = new Map();   // path -> content key
const bases = new Map();     // unit key -> load base, for the images without one
const errors = [];
browser.on('Runtime.consoleAPICalled', (p) => {
  const t = p.args.map((a) => a.value ?? a.description ?? '').join(' ');
  let m;
  if ((m = /^\[gex-record\] unit ([0-9a-f]{64}) (hit|miss) (.*)$/.exec(t))) {
    const was = units.get(m[1]);
    units.set(m[1], { path: m[3], hit: m[2] === 'hit' || !!was?.hit });
  } else if ((m = /^\[gex-record\] process (.*?) :: (.*)$/.exec(t))) {
    processes.set(m[1], m[2]);
  } else if ((m = /^\[gex-record\] module (.*) (\S+)$/.exec(t))) {
    modules.set(m[1], m[2]);
  } else {
    // The page's own word for an image without a unit, with where it was
    // loaded: a unit is built for an image at a base (gexc cgen-image --pe-base).
    if ((m = /gex-nt: image (.*) at base (0x[0-9a-f]+): no function-tier unit \(key ([0-9a-f]{64})\)/.exec(t))) bases.set(m[3], m[2]);
    if (p.type === 'error') errors.push(t.slice(0, 300));
  }
});
browser.on('Target.attachedToTarget', async (p) => {
  const s = p.sessionId;
  // A Worker is held at its start until its console is on, so no line is
  // missed. Anything else (the service worker) goes at once: its
  // Runtime.enable does not answer while it is held.
  if (p.targetInfo?.type !== 'worker') await browser.send('Runtime.runIfWaitingForDebugger', {}, s).catch(() => {});
  await browser.send('Runtime.enable', {}, s).catch(() => {});
  await browser.send('Target.setAutoAttach', { autoAttach: true, waitForDebuggerOnStart: true, flatten: true }, s).catch(() => {});
  await browser.send('Runtime.runIfWaitingForDebugger', {}, s).catch(() => {});
});
await browser.send('Target.setAutoAttach', { autoAttach: true, waitForDebuggerOnStart: true, flatten: true }, page);
await browser.send('Runtime.enable', {}, page);
await browser.send('Page.enable', {}, page);
const ev = async (e) => (await browser.send('Runtime.evaluate', { expression: e, returnByValue: true, awaitPromise: true }, page).catch(() => ({})))?.result?.value;
await browser.send('Page.navigate', { url }, page);

const t0 = Date.now();
let title = null;
while (Date.now() - t0 < timeout) {
  const d = await ev('(async () => { const c = globalThis.__gexCompositor; return c ? JSON.stringify(await c.describe()) : null; })()');
  const titles = d ? JSON.parse(d)?.titles ?? [] : [];
  title = titles.find((x) => titleRe.test(x)) ?? null;
  if (title) break;
  const st = await ev('JSON.stringify({ phase: globalThis.__nt?.phase, error: globalThis.__nt?.error })');
  if (st && JSON.parse(st).phase === 'failed') { console.error(`record-w3: the page failed: ${JSON.parse(st).error}`); done(1); }
  await sleep(500);
}
if (!title) {
  console.error(`record-w3: no window titled /${titleRe.source}/ in ${timeout / 1000} s`);
  console.error(`  at ${await ev('location.href')}, phase ${await ev('globalThis.__nt?.phase')}`);
  console.error(`  page says: ${String(await ev("globalThis.__nt?.out ?? document.body?.innerText ?? ''")).slice(-1500)}`);
  for (const e of errors.slice(0, 8)) console.error(`  console error: ${e}`);
  done(1);
}
const atTitle = (Date.now() - t0) / 1000;
await sleep(settle);
const sess = JSON.parse((await ev('JSON.stringify(globalThis.__nt?.session ?? null)')) ?? 'null');
if (!sess) { console.error('record-w3: the page has no __nt.session (an nt-page.js from before the recorder)'); done(1); }
const pageUrl = await ev('location.href');
// The run's read set. A page from before `__ntOpened` gives none, and the
// site then keeps its whole bundle.
const opened = JSON.parse((await ev('globalThis.__ntOpened ? __ntOpened().then((p) => JSON.stringify(p), (e) => JSON.stringify({ error: String(e) })) : "null"')) ?? 'null');
if (opened?.error) console.error(`record-w3: no read set: ${opened.error}`);

const session = {
  format: 'gex-w3-session',
  version: 1,
  url: pageUrl,
  title,
  title_seconds: atTitle,
  page: sess,
  bundle: opt('--bundle', null) ? resolve(opt('--bundle', null)) : null,
  // Each Wine process's main image: a unit's .exe (a Windows program) or the
  // first wasm32 module it loaded as an .exe (Wine's own programs).
  processes: [
    ...[...processes].map(([image, cmdline]) => ({ image, cmdline })),
    ...[...modules.keys()].filter((m) => /^wasmdll:.*\.exe$/i.test(m) && !processes.has(m.slice(8)))
      .map((m) => ({ image: m.slice(8), cmdline: null })),
  ],
  units: [...units].filter(([, u]) => u.hit).map(([key, u]) => ({ key, path: u.path })),
  unit_misses: [...units].filter(([, u]) => !u.hit).map(([key, u]) => ({ key, path: u.path, ...(bases.has(key) ? { base: bases.get(key) } : {}) })),
  modules: [...modules].map(([path, content]) => ({ path, content })),
  files_read: Array.isArray(opened) ? opened : null,
};
writeFileSync(join(out, 'w3-session.json'), `${JSON.stringify(session, null, 2)}\n`);
console.log(`${out}/w3-session.json: "${title}" at ${atTitle.toFixed(1)} s; ${session.processes.length} processes, `
  + `${session.units.length} units (${session.unit_misses.length} images without one), ${session.modules.length} wasm modules`
  + `${session.files_read ? `, ${session.files_read.length} files read` : ''}`);
if (errors.length) console.log(`record-w3: ${errors.length} console errors, first: ${errors[0]}`);
done(0);
