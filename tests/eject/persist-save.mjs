// tests/eject/persist-save.sh's driver: the save probe's three visits, each
// reload DELAY ms (default 3000) after the visit's save, for each way it
// saves. Nothing asks for a snapshot: the page's own snapshot after a save is
// what has to keep it.
//
//   node tests/eject/persist-save.mjs SITE [--chrome PATH]
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

const profile = mkdtempSync(join(tmpdir(), 'gex-persist-save-'));
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
  console.log(failed ? `FAIL persist-save (${failed} check(s))` : 'ok persist-save');
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

const delay = Number(process.env.DELAY ?? 3000);
const want = { 1: 'probe: saved 1', 2: 'probe: saved 2', 3: 'probe: checked 3' };
for (const how of ['stdio', 'rename']) {
  for (const visit of [1, 2, 3]) {
    await s('Page.navigate', { url: `${url}?visit=${visit}&how=${how}` });
    const t0 = Date.now();
    while (Date.now() - t0 < 120e3) {
      await sleep(300);
      out = (await ev('globalThis.__test?.out() ?? ""').catch(() => '')) ?? '';
      if (out.includes(want[visit]) || out.includes('probe: FAIL')) break;
    }
    console.log(`  ${how} visit ${visit}: ${out.split('\n').filter((l) => l.startsWith('probe:')).join(' | ')}`);
    if (!check(out.includes(want[visit]), `${how} visit ${visit}: ${want[visit]}`)) done();
    if (visit < 3) await sleep(delay);
  }
}
done();
