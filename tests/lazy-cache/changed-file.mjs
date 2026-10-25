#!/usr/bin/env node
// The page's cache of lazy files (web/src/lazy-cache-worker.js) keeps a
// file's chunks under its URL and version. A file that changes under the
// same URL -- a list naming files by path, restaged -- must read as its new
// bytes on the next visit, not as the chunks the last visit kept.
//
// The real owner Worker runs in a worker thread, over a Cache Storage kept
// on disk between its runs (the visits) and a server in another
// process (the reader blocks this thread while it waits). Visit 1 reads the
// file; it is rewritten, 69 bytes shorter; visit 2 reads it again with the
// new list's version, and again with the file gone from the server (the
// cache alone). A last visit with no versions, as before, shows the
// kept chunks would have come back.
//
//   node tests/lazy-cache/changed-file.mjs     prints "ok   lazy cache: ..." or FAIL
import { Worker } from 'node:worker_threads';
import { spawn } from 'node:child_process';
import { mkdtempSync, mkdirSync, writeFileSync, rmSync, statSync, utimesSync } from 'node:fs';
import { join, dirname } from 'node:path';
import { tmpdir } from 'node:os';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { makeControl, setLazyCache, cachedRead, lazyVersion, CHUNK } from '../../shims/browser/js/lazy-cache.mjs';

const ROOT = join(dirname(fileURLToPath(import.meta.url)), '..', '..');
const dir = mkdtempSync(join(tmpdir(), 'lazy-cache-changed-'));
const site = join(dir, 'site'), store = join(dir, 'cache');
const fail = (m) => { console.log(`FAIL lazy cache: ${m}`); cleanup(); process.exit(1); };
let server = null;
const cleanup = () => { server?.kill(); rmSync(dir, { recursive: true, force: true }); };

// Two contents that differ from the first byte, 2.5 chunks long.
const fill = (len, seed) => { const b = Buffer.alloc(len); for (let i = 0; i < len; i++) b[i] = (i * 31 + seed) & 255; return b; };
const A = fill(Math.floor(CHUNK * 2.5), 7), B = fill(A.length - 69, 101);
const file = join(site, 'lazy/w64/winepe.wasm');
const write = (bytes, t) => {
  rmSync(file, { force: true });
  writeFileSync(file, bytes, { flag: 'w' });
  utimesSync(file, t, t);
};
mkdirSync(dirname(file), { recursive: true });
mkdirSync(store, { recursive: true });

// The server: ranged GETs of the site, in its own process.
server = spawn(process.execPath, ['-e', `
  const http = require('node:http'), fs = require('node:fs'), path = require('node:path');
  const site = ${JSON.stringify(site)};
  http.createServer((req, res) => {
    const p = path.join(site, decodeURIComponent(new URL(req.url, 'http://x').pathname));
    let st; try { st = fs.statSync(p); } catch { res.writeHead(404).end(); return; }
    let a = 0, b = st.size - 1, code = 200; const h = { 'Cache-Control': 'no-store' };
    const m = /^bytes=(\\d+)-(\\d*)$/.exec(req.headers.range || '');
    if (m) { a = +m[1]; if (m[2] !== '') b = Math.min(b, +m[2]); code = 206; h['Content-Range'] = 'bytes ' + a + '-' + b + '/' + st.size; }
    res.writeHead(code, h); fs.createReadStream(p, { start: a, end: b }).pipe(res);
  }).listen(0, '127.0.0.1', function () { console.log(this.address().port); });
`], { stdio: ['ignore', 'pipe', 'inherit'] });
const port = await new Promise((r) => server.stdout.once('data', (d) => r(Number(String(d).trim()))));
const path = '/lazy/w64/winepe.wasm';
const url = `http://127.0.0.1:${port}${path}`;

// The owner Worker, with `self` and a Cache Storage on disk.
const prelude = `
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
const worker = pathToFileURL(join(ROOT, 'web/src/lazy-cache-worker.js')).href;
const memory = new WebAssembly.Memory({ initial: 64, maximum: 64, shared: true });

async function visit(versions, len = statSync(file).size) {
  const ctl = makeControl();
  setLazyCache({ ctl });
  const w = new Worker(prelude, { eval: true, workerData: { store, worker } });
  const ready = await new Promise((r) => {
    w.on('message', (m) => { if (m.kind === 'ready') r(m); });
    w.on('error', (e) => r({ ok: false, why: String(e) }));
    w.postMessage({ ctl, memory, cache: 'gex-pieces', versions });
  });
  if (!ready.ok) fail(`owner not ready: ${ready.why}`);
  const n = cachedRead(url, 0, 0, len);
  const got = Buffer.from(new Uint8Array(memory.buffer, 0, Math.max(n, 0)));
  // The owner keeps what it fetched after it answers.
  await new Promise((r) => setTimeout(r, 500));
  await w.terminate();
  return { n, got };
}

const t1 = new Date('2026-01-01T00:00:00Z'), t2 = new Date('2026-01-02T00:00:00Z');
write(A, t1);
const v1 = { [path]: lazyVersion(A.length, Math.trunc(t1 / 1000)) };
const one = await visit(v1);
if (one.n !== A.length || !one.got.equals(A)) fail(`visit 1 read ${one.n} bytes, not the file's ${A.length}`);

write(B, t2);
const v2 = { [path]: lazyVersion(B.length, Math.trunc(t2 / 1000)) };
const two = await visit(v2);
if (two.n !== B.length) fail(`visit 2 read ${two.n} bytes, not the file's ${B.length}`);
if (!two.got.equals(B)) fail(`visit 2 read kept chunks of the old file (first byte ${two.got[0]}, want ${B[0]})`);
// Kept under that version: with the file gone from the server, the next
// visit still reads it, from the cache alone.
rmSync(file);
const three = await visit(v2, B.length);
if (three.n !== B.length || !three.got.equals(B)) fail(`visit 3 (file gone, same version) read ${three.n} bytes, not the kept ${B.length}`);

// Unversioned, as the cache was keyed before: A's chunks kept under the
// bare URL come back for B.
write(A, t1);
await visit({});
write(B, t2);
const old = await visit({});
const stale = old.n === B.length && !old.got.equals(B);
cleanup();
if (!stale) { console.log('FAIL lazy cache: without versions the changed file read right too; the test does not see the bug'); process.exit(1); }
console.log(`ok   lazy cache: a file changed under its URL reads as its new ${B.length} bytes (unversioned keys gave the old chunks)`);
process.exit(0);
