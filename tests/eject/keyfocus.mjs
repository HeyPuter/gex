#!/usr/bin/env node
// tests/eject/keyfocus.sh's run: the W3 page it built (SITE, keyfocus.exe on
// Wine), served cross-origin isolated, in headless Chromium with a GPU. The
// program shows A and an off-output window OFF on top of it, neither
// activated, so no window has the keyboard.
//
//   node tests/eject/keyfocus.mjs SITE [--chrome PATH]
//
// Checks: (1) with no window focused, a key goes to A, the topmost toplevel
// that is mapped and on the output, not OFF; (2) once the program activates
// B and raises A over it without activation, the next key goes to B (the
// focused window keeps the keyboard; the fallback never takes it). Prints
// PASS/FAIL lines and "ok eject-keyfocus" or "FAIL eject-keyfocus".
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

const profile = mkdtempSync(join(tmpdir(), 'gex-keyfocus-'));
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
  if (failed && out) console.log('guest output (kf, err, keyboard lines):\n' + out.split('\n').filter((l) => /^kf:|err:|keyboard|xkb/i.test(l)).slice(-30).join('\n'));
  console.log(failed ? `FAIL eject-keyfocus (${failed} check(s))` : 'ok eject-keyfocus');
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
await s('Emulation.setDeviceMetricsOverride', { width: 1280, height: 760, deviceScaleFactor: 1, mobile: false });
const ev = async (x) => {
  const r = await s('Runtime.evaluate', { expression: x, returnByValue: true, awaitPromise: true });
  if (r.exceptionDetails) throw new Error(`${x.slice(0, 60)}: ${r.exceptionDetails.exception?.description ?? r.exceptionDetails.text}`);
  return r.result?.value;
};
await s('Page.navigate', { url });


const describe = () => ev('globalThis.__gexCompositor?.describe({ pixels: false }) ?? null').catch(() => null);
const keyOf = (d, title) => d?.list?.map((l) => /^(\S+) role=1 .*"([^"]*)"$/.exec(l)).find((m) => m && m[2] === title)?.[1] ?? null;
const wait = async (what, secs, f) => {
  const t = Date.now();
  while (Date.now() - t < secs * 1000) {
    out = (await ev('globalThis.__test?.out() ?? ""').catch(() => '')) ?? '';
    const status = await ev('globalThis.__test?.status() ?? ""').catch(() => '');
    if (/^(failed|exited)/.test(status ?? '')) { check(false, `page: ${status} (waiting for ${what})`); done(); }
    const r = await f();
    if (r) return r;
    await sleep(1000);
  }
  return null;
};
// Up: both windows listed, the program ready.
const up = await wait('the windows', 400, async () => {
  if (!/^kf: ready/m.test(out)) return null;
  const d = await describe();
  const a = keyOf(d, 'kf-a'), off = keyOf(d, 'kf-off');
  return a && off ? { a, off, d } : null;
});
if (!check(!!up, 'keyfocus.exe is up with its windows kf-a and kf-off')) done();
console.log(up.d.list.filter((l) => / role=1 /.test(l)).join('\n'));
// The case: nothing activated, so no window has the keyboard.
check(up.d.focus === null, `no window has the keyboard before the first key (focus ${up.d.focus})`);
const key = async (k, code, vk) => {
  await ev(`document.querySelector('#stage canvas').focus({ preventScroll: true })`);
  for (const type of ['keyDown', 'keyUp']) {
    await s('Input.dispatchKeyEvent', { type, key: k, code, text: type === 'keyDown' ? k : undefined, windowsVirtualKeyCode: vk, nativeVirtualKeyCode: vk });
    await sleep(100);
  }
};
await key('a', 'KeyA', 65);
const first = await wait('the first key', 30, async () => (/^kf: (A|OFF|B|\?) key 0x41$/m.exec(out) ?? [])[1]);
check(first === 'A', `with no window focused the key reached A, the topmost window on the output (it reached ${first ?? 'no window'})`);
// The compositor's own note of where the keyboard went.
const d1 = await describe();
const gave = d1?.log?.map((l) => /no window has the keyboard; it goes to the top one, (\S+)/.exec(l)?.[1]).filter(Boolean) ?? [];
check(gave.length === 1 && gave[0] === up.a, `the keyboard went to A (${up.a}) once, not OFF (${up.off}), which is above it but off the output (given to: ${gave.join(', ') || 'none'})`);
// B activated by the program, A raised over it without activation.
const b = await wait('B', 60, async () => {
  if (!/^kf: B up/m.test(out)) return null;
  const d = await describe();
  const k = keyOf(d, 'kf-b');
  return k && d.focus === k ? k : null;
});
if (!check(!!b, 'B is up and has the keyboard (its client activated it)')) done();
await sleep(1000);
await key('b', 'KeyB', 66);
const second = await wait('the second key', 30, async () => (/^kf: (A|OFF|B|\?) key 0x42$/m.exec(out) ?? [])[1]);
check(second === 'B', `the next key reached B, which has the keyboard, not A above it (it reached ${second ?? 'no window'})`);
const d2 = await describe();
check(d2?.focus === b, `B keeps the keyboard (${d2?.focus})`);
done();
