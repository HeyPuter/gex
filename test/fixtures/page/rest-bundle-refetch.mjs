#!/usr/bin/env node
// A rest bundle's frames are fetched once each, however a program's reads
// move between them (lazy-cache-worker.js). The owner keeps only a few
// frames decoded; one that was let go was fetched again when read again,
// and a launch that moved between frames (Notepad++ in the Wine desktop
// page) fetched 17 frames 33 times. Now the frames are kept as fetched,
// compressed, and decoded again from there.
//
// The real lazy-cache-worker.js runs in a worker thread over a Cache
// Storage on disk; the bundle (8 frames of 1 MiB) comes from a ranged
// server in another process that counts its requests. One page Worker reads
// a span in each frame, in order, then again in reverse, then the first
// again: 17 reads over 8 frames, more than the owner keeps decoded.
//
//   node test/fixtures/page/rest-bundle-refetch.mjs     prints "ok   rest bundle refetch: ..." or FAIL
import { pathToFileURL as gexPathToFileURL } from 'node:url';
import { join as gexJoin } from 'node:path';
// gex: the checkout these fixtures are in, or GEX_ROOT.
const GEX_ROOT = process.env.GEX_ROOT || gexJoin(import.meta.dirname, '..', '..', '..');
const gexUrl = (p) => gexPathToFileURL(gexJoin(GEX_ROOT, p)).href;
import { Worker } from 'node:worker_threads';
import { spawn, execFileSync } from 'node:child_process';
import { mkdtempSync, mkdirSync, writeFileSync, readFileSync, rmSync } from 'node:fs';
import { join } from 'node:path';
import { tmpdir } from 'node:os';
const { makeControl } = await import(gexUrl('hosts/browser/js/lazy-cache.mjs'));

const dir = mkdtempSync(join(tmpdir(), 'rest-bundle-refetch-'));
const site = join(dir, 'site'), store = join(dir, 'cache');
mkdirSync(site, { recursive: true });
mkdirSync(store, { recursive: true });
let server = null;
const cleanup = () => { server?.kill(); rmSync(dir, { recursive: true, force: true }); };
const fail = (m) => { console.log(`FAIL rest bundle refetch: ${m}`); cleanup(); process.exit(1); };

// The bundle: one file of 8 MiB, cut into 1 MiB frames, each compressed on its own.
const FRAME = 1 << 20, NFRAMES = 8;
const fill = (len, seed) => { const b = Buffer.alloc(len); for (let i = 0; i < len; i++) b[i] = ((i * 2654435761 + seed) >>> 13) & 255; return b; };
const files = [fill(FRAME * NFRAMES, 7)];
const offsets = [];
let at = 0;
for (const f of files) { offsets.push(at); at += f.length; }
const plain = Buffer.concat(files);
const frames = [], parts = [];
for (let o = 0, k = 0; o < plain.length; o += FRAME, k++) {
  const p = join(dir, `frame${k}`);
  writeFileSync(p, plain.subarray(o, o + FRAME));
  execFileSync('zstd', ['-q', '-f', '-3', p, '-o', `${p}.zst`]);
  const z = readFileSync(`${p}.zst`);
  frames.push([parts.reduce((a, b) => a + b.length, 0), z.length]);
  parts.push(z);
}
writeFileSync(join(site, 'rest.bin'), Buffer.concat(parts));
writeFileSync(join(site, 'rest.bin.json'), JSON.stringify({ frame: FRAME, size: plain.length, frames }));

server = spawn(process.execPath, ['-e', `
  const http = require('node:http'), fs = require('node:fs'), path = require('node:path');
  const site = ${JSON.stringify(site)};
  let ranged = 0;
  http.createServer((req, res) => {
    if (req.url === '/count') { res.writeHead(200).end(String(ranged)); return; }
    if (req.url.startsWith('/rest.bin') && req.headers.range) ranged++;
    const p = path.join(site, decodeURIComponent(new URL(req.url, 'http://x').pathname));
    let st; try { st = fs.statSync(p); } catch { res.writeHead(404).end(); return; }
    let a = 0, b = st.size - 1, code = 200; const h = { 'Cache-Control': 'no-store' };
    const m = /^bytes=(\\d+)-(\\d*)$/.exec(req.headers.range || '');
    if (m) { a = +m[1]; if (m[2] !== '') b = Math.min(b, +m[2]); code = 206; h['Content-Range'] = 'bytes ' + a + '-' + b + '/' + st.size; }
    res.writeHead(code, h); fs.createReadStream(p, { start: a, end: b }).pipe(res);
  }).listen(0, '127.0.0.1', function () { console.log(this.address().port); });
`], { stdio: ['ignore', 'pipe', 'inherit'] });
const port = await new Promise((r) => server.stdout.once('data', (d) => r(Number(String(d).trim()))));
const bundle = `http://127.0.0.1:${port}/rest.bin`;

const readerSrc = `
  const { parentPort, workerData: d } = require('node:worker_threads');
  (async () => {
    const lc = await import(d.lazyCache), lf = await import(d.lazyFetch);
    lc.setLazyCache({ ctl: d.ctl });
    lf.setRestBundle ? lf.setRestBundle(true) : (globalThis.GEX_REST_BUNDLE = 1);
    await lf.lazyReady();
    const read = lf.lazyFetch(d.memory);
    const mem = new Uint8Array(d.memory.buffer);
    const got = [];
    for (const [href, from, len, dst] of d.reads) {
      const url = new TextEncoder().encode(href);
      mem.set(url, 0);
      got.push(read(0, url.length, BigInt(from), dst, len));
    }
    parentPort.postMessage({ got });
  })().catch((e) => parentPort.postMessage({ error: String(e?.stack ?? e) }));
`;
const ownerSrc = `
  const { parentPort, workerData } = require('node:worker_threads');
  const fs = require('node:fs'), path = require('node:path'), crypto = require('node:crypto');
  const file = (name, key) => path.join(workerData.store, crypto.createHash('sha1').update(name + '\\n' + key).digest('hex'));
  globalThis.caches = { open: async (name) => ({
    match: async (key) => { try { return new Response(fs.readFileSync(file(name, String(key)))); } catch { return undefined; } },
    put: async (key, res) => { fs.writeFileSync(file(name, String(key)), Buffer.from(await res.arrayBuffer())); },
    delete: async (key) => { fs.rmSync(file(name, String(key)), { force: true }); return true; },
  }) };
  globalThis.self = globalThis;
  self.postMessage = (m) => parentPort.postMessage(m);
  import(workerData.worker).then(() => parentPort.on('message', (data) => self.onmessage({ data })));
`;
const { makeControl: mk } = { makeControl };
const memory = new WebAssembly.Memory({ initial: 256, maximum: 256, shared: true }); // 16 MiB
const ctl = mk();
const owner = new Worker(ownerSrc, { eval: true, workerData: { store, worker: gexUrl('page/src/lazy-cache-worker.js') } });
const ready = await new Promise((r) => {
  owner.on('message', (m) => { if (m.kind === 'ready') r(m); });
  owner.once('error', (e) => r({ ok: false, why: String(e) }));
  owner.postMessage({ ctl, memory, cache: 'gex-pieces', versions: {}, zstd: gexUrl('page/vendor/zstddec/zstddec-stream.mjs') });
});
if (!ready.ok) fail(`owner not ready: ${ready.why}`);
// A 64 KiB span in the middle of each frame: forward, back, then the first again.
const order = [...Array(NFRAMES).keys()];
const seq = [...order, ...order.slice().reverse(), 0];
const SPAN = 65536;
const reads = seq.map((k, i) => [`${bundle}#0`, k * FRAME + FRAME / 2, SPAN, (1 << 20) + i * SPAN, k]);
const res = await new Promise((resolve) => {
  const w = new Worker(readerSrc, { eval: true, workerData: { lazyCache: gexUrl('hosts/browser/js/lazy-cache.mjs'), lazyFetch: gexUrl('hosts/browser/js/lazy-fetch.mjs'), memory, ctl, reads: reads.map(([h, f, l, d]) => [h, f, l, d]) } });
  w.once('message', (m) => { w.terminate(); resolve(m); });
  w.once('error', (e) => resolve({ error: String(e) }));
});
await owner.terminate();
if (res.error) fail(res.error);
reads.forEach(([, from, len, dst, k], i) => {
  if (res.got[i] !== len) fail(`read ${i} (frame ${k}): ${res.got[i]} bytes, not ${len}`);
  if (!Buffer.from(new Uint8Array(memory.buffer, dst, len)).equals(files[0].subarray(from, from + len))) fail(`read ${i} (frame ${k}): wrong bytes`);
});
const fetched = Number(await (await fetch(`http://127.0.0.1:${port}/count`)).text());
if (fetched !== NFRAMES) fail(`${reads.length} reads over ${NFRAMES} frames fetched ${fetched} times, not ${NFRAMES} (once a frame)`);
cleanup();
console.log(`ok   rest bundle refetch: ${reads.length} reads over ${NFRAMES} frames, ${fetched} fetches, the bytes right`);
process.exit(0);
