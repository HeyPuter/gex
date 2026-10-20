#!/usr/bin/env node
// tests/eject/persist-written.sh's run: the page it built (SITE), served
// cross-origin isolated, in headless Chromium, visited three times with one
// profile (one IndexedDB). The page keeps /save; its bundle has a file there
// the program never touches (seed.txt) and a lazy file there (big.bin, 4 MiB,
// fetched from lazy/ only if something reads it). persist-probe.c says what
// each visit writes and expects.
//
//   node tests/eject/persist-written.mjs SITE [--chrome PATH]
//
// Checks, per visit: the probe's "probe: ok N"; the snapshot after it stored
// only what the program wrote (1, 1, 0 files) and holds 1, 2, 2 files
// (a.txt, kept on visit 2 though not rewritten, and b.txt; never seed.txt);
// and no request for the lazy file, on any visit. Prints PASS/FAIL lines and
// "ok eject-persist-written" or "FAIL eject-persist-written".
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
const lazyHits = [];
const server = createServer((req, res) => {
  const p = normalize(decodeURIComponent(new URL(req.url, 'http://x').pathname)).replace(/^(\.\.[/\\])+/, '');
  if (p.startsWith('/lazy/')) lazyHits.push(`${p} ${req.headers.range ?? ''}`);
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

const profile = mkdtempSync(join(tmpdir(), 'gex-persist-written-'));
const cport = 9800 + Math.floor(Math.random() * 600);
const chrome = spawn(opt('--chrome', process.env.CHROME || '/usr/bin/chromium'), [
  '--headless', '--no-sandbox', '--disable-gpu-sandbox', '--enable-unsafe-webgpu',
  '--disable-hang-monitor', '--no-first-run', '--no-default-browser-check', '--remote-allow-origins=*',
  `--remote-debugging-port=${cport}`, `--user-data-dir=${profile}`, 'about:blank',
], { detached: true, stdio: 'ignore' });
let out = '';
const done = () => {
  try { process.kill(-chrome.pid, 'SIGKILL'); } catch {}
  server.close();
  rmSync(profile, { recursive: true, force: true });
  if (failed && out) console.log('guest output (tail):\n' + out.slice(-600));
  console.log(failed ? `FAIL eject-persist-written (${failed} check(s))` : 'ok eject-persist-written');
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
const ev = async (x) => {
  const r = await s('Runtime.evaluate', { expression: x, returnByValue: true, awaitPromise: true });
  if (r.exceptionDetails) throw new Error(`${x.slice(0, 60)}: ${r.exceptionDetails.exception?.description ?? r.exceptionDetails.text}`);
  return r.result?.value;
};

const want = { 1: { wrote: 1, files: 1, restored: 0 }, 2: { wrote: 1, files: 2, restored: 1 }, 3: { wrote: 0, files: 2, restored: 2 } };
for (const visit of [1, 2, 3]) {
  await s('Page.navigate', { url: `${url}?visit=${visit}` });
  let status = '';
  const t0 = Date.now();
  while (Date.now() - t0 < 120e3) {
    await sleep(500);
    status = (await ev('globalThis.__test?.status() ?? ""').catch(() => '')) ?? '';
    if (/^(failed|exited \(\d+\) synced)/.test(status)) break;
  }
  out = (await ev('globalThis.__test?.out() ?? ""').catch(() => '')) ?? '';
  const st = (await ev('globalThis.__test?.state() ? { restored: globalThis.__test.state().restored ?? 0 } : null').catch(() => null)) ?? {};
  console.log(`visit ${visit}: ${status}`);
  if (!check(out.includes(`probe: ok ${visit}`), `visit ${visit}: the probe passed (${(/probe: .*/.exec(out) ?? ['no probe line'])[0]})`)) done();
  const r = JSON.parse((/synced (.*)$/.exec(status) ?? [, 'null'])[1]);
  const w = want[visit];
  check(r && !r.error, `visit ${visit}: a snapshot was taken${r?.error ? ` (${r.error})` : ''}`);
  check(r?.wrote === w.wrote && r?.files === w.files,
    `visit ${visit}: stored ${r?.wrote} file(s), holds ${r?.files} (want ${w.wrote}, ${w.files}: only what the program wrote, the restored ones kept)`);
  check(st.restored === w.restored, `visit ${visit}: ${st.restored} file(s) restored (want ${w.restored})`);
}
check(lazyHits.length === 0, `the lazy file under /save was never fetched${lazyHits.length ? ` (${lazyHits.slice(0, 3).join(', ')})` : ''}`);
done();
