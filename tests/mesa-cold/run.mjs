#!/usr/bin/env node
// Mesa's cold part (docs/mesa-wasm-size.md): does the split replay draw what
// the whole one does, and does a miss load the cold part and carry on?
//
//   node tests/mesa-cold/run.mjs [TARGET_DIR]
//
// TARGET_DIR is a Mesa build's gex-wasm target directory, after
// web/build-mesa-wasm.sh (default target/web-build/mesa-mt/build/src/gallium/
// targets/gex-wasm; the st one works the same). Replays
// tests/gl-traces/gles-gears-frames.gexgltr in Chromium (GEX_WEBGPU_SOFTWARE=1:
// on SwiftShader), reads the last frame back and hashes it, through:
//
//   whole     the module unsplit                        (the reference)
//   split     the build's own split, cold part loaded with the module
//             (as a page's main thread does)
//   miss      every function kept but glReadPixels' (web/mesa-split.py
//             --cold), left for a miss: the cold part is fetched and
//             compiled synchronously, and the read-back then waits on
//             WebGPU (JSPI) inside a function it brought
//   prefetch  the same after the background prefetch: instantiated only
//
// Prints `ok   mesa-cold <leg>: ...` per leg and exits 0 when every digest
// matches the whole module's and each miss was taken as described.
import { spawn, spawnSync } from 'node:child_process';
import { createServer } from 'node:http';
import { copyFileSync, createReadStream, mkdtempSync, readFileSync, rmSync, statSync, writeFileSync, existsSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, extname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { Cdp, browserWsUrl } from '../../web/test/cdp.mjs';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '../..');
const TGT = resolve(process.argv[2] || join(ROOT, 'target/web-build/mesa-mt/build/src/gallium/targets/gex-wasm'));
const TRACE = join(ROOT, 'tests/gl-traces/gles-gears-frames.gexgltr');
const CHROME = process.env.CHROME || '/usr/bin/chromium';
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const fail = (msg) => { console.log(`FAIL mesa-cold: ${msg}`); process.exitCode = 1; };

for (const f of ['gex-wgpu-replay.wasm.orig', 'gex-wgpu-replay.js', 'split/gex-wgpu-replay.wasm']) {
  if (!existsSync(join(TGT, f))) { console.log(`skip mesa-cold: no ${f} in ${TGT} (web/build-mesa-wasm.sh)`); process.exit(0); }
}
const WORK = mkdtempSync(join(process.env.TMPDIR || tmpdir(), 'mesa-cold-'));
const glue = readFileSync(join(TGT, 'gex-wgpu-replay.js'), 'utf8');
const marked = (split) => glue.replace(/(gexMesaColdSplit *= *["'])GEX_MESA_COLD_[A-Z]*/,
  `$1GEX_MESA_COLD_${split ? 'YES' : 'NO'}`);
const variant = (name, files, split) => {
  spawnSync('mkdir', ['-p', join(WORK, name)]);
  for (const [to, from] of Object.entries(files)) copyFileSync(from, join(WORK, name, to));
  writeFileSync(join(WORK, name, 'gex-wgpu-replay.js'), marked(split));
};
variant('whole', { 'gex-wgpu-replay.wasm': join(TGT, 'gex-wgpu-replay.wasm.orig') }, false);
const cold = join(TGT, 'split/gex-wgpu-replay.deferred.wasm');
variant('split', { 'gex-wgpu-replay.wasm': join(TGT, 'split/gex-wgpu-replay.wasm'),
  ...(existsSync(cold) ? { 'gex-wgpu-replay.deferred.wasm': cold } : {}) }, existsSync(cold));
const r = spawnSync('python3', [join(ROOT, 'web/mesa-split.py'), join(TGT, 'gex-wgpu-replay.wasm.orig'), '-',
  join(WORK, 'miss'), '--cold', '[Rr]ead[Pp]ixels'], { encoding: 'utf8' });
if (r.status !== 0) { fail(`mesa-split.py: ${r.stderr || r.stdout}`); process.exit(1); }
writeFileSync(join(WORK, 'miss', 'gex-wgpu-replay.js'), marked(true));
console.log(`     ${r.stdout.trim()}`);

const types = { '.html': 'text/html', '.js': 'text/javascript', '.mjs': 'text/javascript', '.wasm': 'application/wasm' };
const server = createServer((req, res) => {
  const p = decodeURIComponent(new URL(req.url, 'http://x').pathname);
  if (p.includes('..')) { res.writeHead(403).end(); return; }
  const file = p === '/' ? join(ROOT, 'tests/mesa-cold/page.html') : p === '/trace' ? TRACE : join(WORK, p);
  let st;
  try { st = statSync(file); } catch { res.writeHead(404).end(); return; }
  res.writeHead(200, { 'Content-Type': types[extname(file)] ?? 'application/octet-stream', 'Content-Length': st.size,
    'Cross-Origin-Opener-Policy': 'same-origin', 'Cross-Origin-Embedder-Policy': 'require-corp' });
  createReadStream(file).pipe(res);
});
await new Promise((ok) => server.listen(0, '127.0.0.1', ok));
const port = server.address().port;

async function leg(name, dir, query) {
  const user = mkdtempSync(join(WORK, 'chrome-'));
  const cdpPort = Number(process.env.GEX_MESA_COLD_CDP_PORT || 9667);
  const chrome = spawn(CHROME, ['--headless', '--no-sandbox', '--disable-gpu-sandbox', '--enable-unsafe-webgpu',
    '--enable-unsafe-swiftshader',
    ...(process.env.GEX_WEBGPU_SOFTWARE === '1' ? ['--use-angle=swiftshader-webgl', '--use-webgpu-adapter=swiftshader'] : []),
    '--no-first-run', '--remote-allow-origins=*', `--remote-debugging-port=${cdpPort}`, `--user-data-dir=${user}`,
    'about:blank'], { stdio: 'ignore' });
  try {
    let ws = null;
    for (let i = 0; i < 80 && !ws; i++) { ws = await browserWsUrl(cdpPort); if (!ws) await sleep(250); }
    if (!ws) throw new Error('no browser');
    const browser = await Cdp.connect(ws);
    const { targetId } = await browser.send('Target.createTarget', { url: 'about:blank' });
    const { sessionId: s } = await browser.send('Target.attachToTarget', { targetId, flatten: true });
    await browser.send('Runtime.enable', {}, s);
    const lines = [];
    browser.on('Runtime.consoleAPICalled', (p, sid) => {
      if (sid === s) lines.push(p.args.map((a) => a.value ?? a.description ?? '').join(' '));
    });
    await browser.send('Page.navigate', { url: `http://127.0.0.1:${port}/?mesa=/${dir}/gex-wgpu-replay.js&trace=/trace${query}` }, s);
    let res = null;
    for (let t0 = Date.now(); !res && Date.now() - t0 < 300000;) {
      await sleep(250);
      res = (await browser.send('Runtime.evaluate', { expression: 'globalThis.__result ?? null', returnByValue: true }, s)
        .catch(() => null))?.result?.value ?? null;
    }
    browser.close();
    return { res: res ?? { ok: false, error: 'timeout' }, cold: lines.filter((l) => l.includes('cold part')) };
  } finally {
    chrome.kill('SIGKILL');
    await sleep(500);
  }
}

const want = {
  whole: [],
  split: existsSync(cold) ? ['loaded with the module'] : [],
  miss: ['loaded on a miss (fetched and compiled now'],
  prefetch: ['loaded on a miss (prefetched)'],
};
const legs = [['whole', 'whole', ''], ['split', 'split', ''], ['miss', 'miss', '&lazy=1&noprefetch=1'],
  ['prefetch', 'miss', '&lazy=1&settle=3000']];
let ref = null, passed = 0;
for (const [name, dir, query] of legs) {
  const { res, cold: seen } = await leg(name, dir, query);
  const how = seen.join(' | ') || 'no cold part loaded';
  if (!res.ok) { fail(`${name}: ${res.error ?? JSON.stringify(res)}`); continue; }
  ref ??= res.digest;
  if (res.digest !== ref) { fail(`${name}: digest ${res.digest}, the whole module's ${ref}`); continue; }
  const need = want[name];
  if (need.some((w) => !seen.some((l) => l.includes(w))) || (need.length === 0 && seen.length)) {
    fail(`${name}: ${how}; wanted ${need.join(', ') || 'none'}`);
    continue;
  }
  passed++;
  console.log(`ok   mesa-cold ${name}: digest ${res.digest}, ${res.frames} frames; ${how}`);
}
server.close();
rmSync(WORK, { recursive: true, force: true });
if (passed === legs.length) console.log(`ok   mesa-cold: ${passed}/${legs.length}`);
process.exit(process.exitCode ?? 0);
