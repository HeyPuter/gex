#!/usr/bin/env node
// A site's second visit fetches none of its packed files: every file pack.json
// names (the bundle or its pieces, the runtime's wasm, the lazy files' list,
// the compositor's Mesa) comes from the page's Cache Storage the second time.
// The server sends every response `no-store`, so the browser's HTTP cache keeps
// nothing (as with a self-signed certificate, which Chromium never caches
// for): what the second visit does not fetch, the page kept itself. Mesa
// for the scene compositor and the lazy files' list are among what it keeps.
//
//   node tests/eject/cached-visit.mjs SITE [--secs S (default 90)] [--chrome PATH]
//
// SITE: a directory with index.html and pack.json (web/eject-site.mjs).
// Each visit runs S seconds. Prints
// "ok   cached visit: ..." or FAIL lines naming what visit 2 fetched again.
import http from 'node:http';
import { spawn } from 'node:child_process';
import { readFileSync, statSync, createReadStream, mkdtempSync, rmSync } from 'node:fs';
import { join, resolve, extname } from 'node:path';
import { tmpdir } from 'node:os';
import { Cdp, browserWsUrl } from '../../web/test/cdp.mjs';

const av = process.argv.slice(2);
const opt = (k, d) => { const i = av.indexOf(k); return i >= 0 ? av[i + 1] : d; };
const site = resolve(av.find((a, i) => !a.startsWith('--') && !(i > 0 && av[i - 1].startsWith('--'))) ?? '');
const secs = Number(opt('--secs', '90'));
let pack;
try { pack = JSON.parse(readFileSync(join(site, 'pack.json'), 'utf8')); } catch { console.log(`skip cached visit: no ${site}/pack.json`); process.exit(0); }
const packed = new Set(Object.keys(pack.files ?? {}));

const types = { '.html': 'text/html', '.js': 'text/javascript', '.mjs': 'text/javascript', '.wasm': 'application/wasm', '.json': 'application/json' };
let visit = 0;
const got = [new Map(), new Map(), new Map()]; // visit -> path -> bytes
const srv = http.createServer((req, res) => {
  const path = decodeURIComponent(new URL(req.url, 'http://x').pathname);
  let p = join(site, path);
  if (!p.startsWith(site)) { res.writeHead(403).end(); return; }
  let st = null; try { st = statSync(p); if (st.isDirectory()) { p = join(p, 'index.html'); st = statSync(p); } } catch { st = null; }
  const h = { 'Cross-Origin-Opener-Policy': 'same-origin', 'Cross-Origin-Embedder-Policy': 'require-corp',
    'Cross-Origin-Resource-Policy': 'same-origin', 'Cache-Control': 'no-store', 'Accept-Ranges': 'bytes' };
  if (!st?.isFile()) { res.writeHead(404, h).end(); return; }
  let a = 0, b = st.size - 1, code = 200;
  const m = /^bytes=(\d*)-(\d*)$/.exec(req.headers.range || '');
  if (m) { if (m[1] === '') a = Math.max(0, st.size - Number(m[2])); else { a = Number(m[1]); if (m[2] !== '') b = Math.min(b, Number(m[2])); } code = 206; h['Content-Range'] = `bytes ${a}-${b}/${st.size}`; }
  h['Content-Type'] = types[extname(p)] || 'application/octet-stream';
  h['Content-Length'] = b - a + 1;
  const g = got[visit];
  g.set(path, (g.get(path) ?? 0) + (b - a + 1));
  res.writeHead(code, h);
  if (req.method === 'HEAD' || st.size === 0) { res.end(); return; }
  createReadStream(p, { start: a, end: b }).pipe(res);
});
await new Promise((r) => srv.listen(0, '127.0.0.1', r));
const url = `http://127.0.0.1:${srv.address().port}/`;

const prof = mkdtempSync(join(tmpdir(), 'cached-visit-'));
const port = 9800 + Math.floor(Math.random() * 400);
const chrome = spawn(opt('--chrome', process.env.CHROME || '/usr/bin/chromium'), [
  '--headless', '--no-sandbox', '--enable-unsafe-webgpu', '--use-angle=vulkan', '--enable-features=Vulkan',
  '--ozone-platform=headless', '--window-size=1280,800', '--no-first-run', '--disable-background-timer-throttling',
  '--disable-renderer-backgrounding', '--remote-allow-origins=*', `--remote-debugging-port=${port}`, `--user-data-dir=${prof}`, 'about:blank',
], { stdio: 'ignore' });
let ws = null;
for (let i = 0; i < 50 && !ws; i++) { await new Promise((r) => setTimeout(r, 200)); ws = await browserWsUrl(port); }
const done = (code) => { chrome.kill(); srv.close(); try { rmSync(prof, { recursive: true, force: true }); } catch {} process.exit(code); };
if (!ws) { console.log('FAIL cached visit: chromium did not start'); done(1); }
const cdp = await Cdp.connect(ws);
for (visit = 1; visit <= 2; visit++) {
  const { targetId } = await cdp.send('Target.createTarget', { url: 'about:blank' });
  const { sessionId } = await cdp.send('Target.attachToTarget', { targetId, flatten: true });
  await cdp.send('Page.enable', {}, sessionId);
  await cdp.send('Page.navigate', { url }, sessionId);
  await new Promise((r) => setTimeout(r, secs * 1000));
  await cdp.send('Target.closeTarget', { targetId });
  await new Promise((r) => setTimeout(r, 2000));
}
visit = 0;
// A packed file is fetched as FILE.zst (a piece also plain, by its name).
const isPacked = (p) => packed.has(p) || packed.has(p.replace(/\.zst$/, ''));
const first = [...got[1].keys()].filter(isPacked);
const again = [...got[2].entries()].filter(([p]) => isPacked(p));
const mb = (n) => (n / 1048576).toFixed(1);
const sum = (m) => [...m.values()].reduce((a, b) => a + b, 0);
if (!first.length) { console.log(`FAIL cached visit: visit 1 fetched none of pack.json's ${packed.size} files`); done(1); }
if (again.length) {
  for (const [p, n] of again) console.log(`FAIL cached visit: visit 2 fetched ${p} again (${mb(n)} MiB)`);
  done(1);
}
console.log(`ok   cached visit: ${first.length} packed files on visit 1 (${mb(sum(got[1]))} MiB in all), none again on visit 2 (${mb(sum(got[2]))} MiB in all)`);
done(0);
