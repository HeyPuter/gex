#!/usr/bin/env node
// tests/eject/repaint.sh's run: the page it built (SITE), served
// cross-origin isolated, in headless Chromium at a 1280x720 viewport (with
// the host's GPU, or SwiftShader with --swiftshader). The Wayland client
// tests/eject/repaint.c commits one 8x400 damage rectangle ROUNDS times with
// its pixels unchanged, then ROUNDS times with them changed; the page takes
// the compositor's counters (describe().frames, .uploads) at each phase line.
//
//   node tests/eject/repaint.mjs SITE [--chrome PATH] [--swiftshader]
//
// Checks: unchanged damage draws no frame and uploads nothing; changed
// damage draws at most one frame per commit and at least one per two, and
// an upload carries the rectangle's pixels (8x400x4 bytes), not the
// buffer's rows across it. Prints PASS/FAIL lines and "ok eject-repaint" or
// "FAIL eject-repaint".
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

const profile = mkdtempSync(join(tmpdir(), 'gex-repaint-'));
const cport = 9800 + Math.floor(Math.random() * 600);
const chrome = spawn(opt('--chrome', process.env.CHROME || '/usr/bin/chromium'), [
  '--headless', '--no-sandbox', '--disable-gpu-sandbox', '--enable-unsafe-webgpu',
  ...(av.includes('--swiftshader') ? ['--enable-unsafe-swiftshader', '--use-angle=swiftshader-webgl', '--use-webgpu-adapter=swiftshader']
    : ['--use-angle=vulkan', '--enable-features=Vulkan']),
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
  console.log(failed ? `FAIL eject-repaint (${failed} check(s))` : 'ok eject-repaint');
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
await s('Emulation.setDeviceMetricsOverride', { width: 1280, height: 720, deviceScaleFactor: 1, mobile: false });
const ev = async (x) => {
  const r = await s('Runtime.evaluate', { expression: x, returnByValue: true, awaitPromise: true });
  if (r.exceptionDetails) throw new Error(`${x.slice(0, 60)}: ${r.exceptionDetails.exception?.description ?? r.exceptionDetails.text}`);
  return r.result?.value;
};
await s('Page.navigate', { url });

const ROUNDS = 60;
let marks = {};
const t0 = Date.now();
while (Date.now() - t0 < 240e3) {
  await sleep(500);
  out = (await ev('globalThis.__test?.out() ?? ""').catch(() => '')) ?? '';
  const status = await ev('globalThis.__test?.status() ?? ""').catch(() => '');
  if (/^(failed|exited)/.test(status ?? '')) { check(false, `page: ${status}`); done(); }
  marks = (await ev('globalThis.__test?.marks() ?? {}').catch(() => ({}))) ?? {};
  if (out.includes('repaint done') && marks['change end']) break;
}
if (!check(!!marks['change end'], 'the client ran both phases')) done();
for (const [k, v] of Object.entries(marks)) console.log(`${k}: frames ${v.frames}, uploads ${JSON.stringify(v.uploads)}`);
const delta = (a, b) => ({
  frames: marks[b].frames - marks[a].frames,
  n: marks[b].uploads.n - marks[a].uploads.n,
  bytes: marks[b].uploads.bytes - marks[a].uploads.bytes,
  same: marks[b].uploads.same - marks[a].uploads.same,
  // A commit the compositor did not see on its own (two between frames)
  // is a whole-buffer upload: counted apart.
  full: marks[b].uploads.full - marks[a].uploads.full,
  fullBytes: marks[b].uploads.fullBytes - marks[a].uploads.fullBytes,
});
const same = delta('same start', 'same end');
const change = delta('change start', 'change end');
const RECT = 8 * 400 * 4;
check(same.frames === 0, `unchanged damage draws no frame: ${same.frames} in ${ROUNDS} commits`);
check(same.n === 0 && same.same >= ROUNDS / 2, `unchanged damage uploads nothing: ${same.n} uploads, ${same.same} left out`);
check(change.frames >= ROUNDS / 2 && change.frames <= ROUNDS, `changed damage draws one frame a commit at most, one per two at least: ${change.frames} in ${ROUNDS}`);
check(change.n > change.full && change.bytes - change.fullBytes <= (change.n - change.full) * RECT,
  `an upload carries the rectangle, not the rows: ${change.bytes - change.fullBytes} bytes in ${change.n - change.full} uploads (the rectangle is ${RECT}; ${change.full} whole-buffer uploads apart)`);
done();
