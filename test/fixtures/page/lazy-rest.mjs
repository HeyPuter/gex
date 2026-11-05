/**
 * Does a rest bundle's failing frame cost a bounded number of requests?
 *
 * A read of a lazy file in the site's rest bundle (`BUNDLE#OFFSET`) fetches
 * the frames over its range. When the server will not give them (a 503 here),
 * the read must try a few times with waits between (lazy-cache.mjs RETRY_MS),
 * then fail (-1, the kernel's EIO), and every read of that frame for a while
 * after (FAILED_HOLD_MS) must fail at once with no request. Both readers:
 *
 *   - a Worker's own (lazy-fetch.mjs, synchronous requests): one read is
 *     1 + RETRY_MS.length requests, the reads after it none, and once the
 *     hold is over and the server answers, the bytes;
 *   - the page's owner of the lazy files (page/src/lazy-cache-worker.js),
 *     asked by readers in other threads: the same counts, and readers of one
 *     frame at once share one request.
 *
 * A plain lazy file (a URL of its own, read by ranges) the same: a read of a
 * failing chunk is 1 + RETRY_MS.length requests, the reads after it none.
 *
 * Usage: node test/fixtures/page/lazy-rest.mjs
 */
import { Worker, isMainThread, workerData, parentPort } from 'node:worker_threads';
import { pathToFileURL as gexPathToFileURL } from 'node:url';
import { join as gexJoin } from 'node:path';
// gex: the checkout these fixtures are in, or GEX_ROOT.
const GEX_ROOT = process.env.GEX_ROOT || gexJoin(import.meta.dirname, '..', '..', '..');
const gexUrl = (p) => gexPathToFileURL(gexJoin(GEX_ROOT, p)).href;
const { RETRY_MS, FAILED_HOLD_MS, OWNER_FAILED } = await import(gexUrl('hosts/browser/js/lazy-cache.mjs'));

const FRAME = 4096;
const NFRAMES = 3;
const TRIES = 1 + RETRY_MS.length;

/** The test's tar: bytes that say where they are. */
function plain() {
  const b = new Uint8Array(FRAME * NFRAMES);
  for (let i = 0; i < b.length; i++) b[i] = (i * 7 + (i >> 12)) & 0xff;
  return b;
}
/** A zstd frame holding `b` as one raw block (single segment, 4-byte size). */
function zstdRaw(b) {
  const out = new Uint8Array(4 + 1 + 4 + 3 + b.length);
  const v = new DataView(out.buffer);
  v.setUint32(0, 0xfd2fb528, true);
  out[4] = 0xa0; // 4-byte content size, single segment
  v.setUint32(5, b.length, true);
  const bh = 1 | (0 << 1) | (b.length << 3); // last, raw
  out[9] = bh & 0xff; out[10] = (bh >> 8) & 0xff; out[11] = (bh >> 16) & 0xff;
  out.set(b, 12);
  return out;
}
/** The bundle (`rest.bin`) and its index (`rest.bin.json`). */
function bundle() {
  const p = plain();
  const parts = [], frames = [];
  let at = 0;
  for (let k = 0; k < NFRAMES; k++) {
    const z = zstdRaw(p.subarray(k * FRAME, (k + 1) * FRAME));
    frames.push([at, z.length]);
    parts.push(z);
    at += z.length;
  }
  const bin = new Uint8Array(at);
  let o = 0;
  for (const z of parts) { bin.set(z, o); o += z.length; }
  return { bin, json: JSON.stringify({ frame: FRAME, frames }), plain: p };
}

const BASE = 'http://site.test/';
const URL_BIN = `${BASE}rest.bin`;

// counts[0]: requests for rest.bin; counts[1]: for rest.bin.json;
// counts[2]: the server's mode (0 a 503 for rest.bin and plain.bin, 1
// answers); counts[3]: requests for plain.bin (the test's tar, unpacked).
function server(counts, delayMs = 0) {
  const { bin, json, plain: p } = bundle();
  return (url, range) => {
    if (url.endsWith('plain.bin')) {
      Atomics.add(counts, 3, 1);
      if (Atomics.load(counts, 2) === 0) return { status: 503, body: new Uint8Array(0) };
      return { status: 206, body: p.slice(range[0], range[1] + 1) };
    }
    const isJson = url.endsWith('.json');
    Atomics.add(counts, isJson ? 1 : 0, 1);
    if (isJson) return { status: 200, body: new TextEncoder().encode(json) };
    if (delayMs) Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, delayMs);
    if (Atomics.load(counts, 2) === 0) return { status: 503, body: new Uint8Array(0) };
    if (!range) return { status: 200, body: bin };
    return { status: 206, body: bin.slice(range[0], range[1] + 1) };
  };
}
const parseRange = (h) => {
  const m = /bytes=(\d+)-(\d+)/.exec(h ?? '');
  return m ? [Number(m[1]), Number(m[2])] : null;
};

if (isMainThread) {
  let bad = 0;
  const check = (what, ok, more = '') => {
    if (!ok) bad++;
    console.log(`${ok ? 'ok  ' : 'FAIL'} ${what}${ok ? '' : ` ${more}`}`);
  };
  const { plain: want } = bundle();

  // -- a Worker's own reads (lazy-fetch.mjs) ---------------------------------
  {
    const counts = new Int32Array(new SharedArrayBuffer(16));
    const serve = server(counts);
    globalThis.XMLHttpRequest = class {
      open(_m, url) { this.url = url; this.headers = {}; }
      setRequestHeader(k, v) { this.headers[k] = v; }
      send() {
        const r = serve(this.url, parseRange(this.headers.Range));
        this.status = r.status;
        this.response = r.body.buffer.slice(r.body.byteOffset, r.body.byteOffset + r.body.byteLength);
      }
    };
    globalThis.GEX_BASE = BASE;
    globalThis.GEX_REST_BUNDLE = 1;
    const { lazyFetch, lazyReady } = await import(gexUrl('hosts/browser/js/lazy-fetch.mjs'));
    await lazyReady();
    const memory = new WebAssembly.Memory({ initial: 1 });
    const fetchLazy = lazyFetch(memory);
    const url = new TextEncoder().encode('rest.bin#0');
    new Uint8Array(memory.buffer).set(url, 0);
    const read = (from, len) => fetchLazy(0, url.length, BigInt(from), 1024, len);
    const quiet = console.error;
    console.error = () => {};
    const t0 = Date.now();
    const r1 = read(100, 200);
    const took = Date.now() - t0;
    check(`worker: a read of a failing frame fails (${r1})`, r1 === -1);
    check(`worker: it asked ${TRIES} times (${counts[0]})`, counts[0] === TRIES);
    check(`worker: and waited between (${took} ms)`, took >= RETRY_MS.reduce((a, b) => a + b, 0) - 50);
    for (let i = 0; i < 20; i++) read(100 + i, 50);
    check(`worker: 20 reads more of it, no request (${counts[0] - TRIES})`, counts[0] === TRIES);
    read(FRAME + 10, 10);
    check(`worker: another frame, ${TRIES} more (${counts[0] - TRIES})`, counts[0] === 2 * TRIES);
    check(`worker: the index asked for once (${counts[1]})`, counts[1] === 1);
    const purl = new TextEncoder().encode('plain.bin');
    new Uint8Array(memory.buffer).set(purl, 512);
    const readPlain = (from, len) => fetchLazy(512, purl.length, BigInt(from), 2048, len);
    const p1 = readPlain(100, 200);
    check(`worker: a plain file's failing chunk fails (${p1}) after ${TRIES} requests (${counts[3]})`, p1 === -1 && counts[3] === TRIES);
    for (let i = 0; i < 20; i++) readPlain(100 + i, 50);
    check(`worker: 20 reads more of it, no request (${counts[3] - TRIES})`, counts[3] === TRIES);
    // The hold over and the server back: the bytes.
    Atomics.store(counts, 2, 1);
    const now = Date.now;
    Date.now = () => now() + FAILED_HOLD_MS + 1000;
    const before = counts[0];
    const r2 = read(100, 200);
    const pb = counts[3];
    const p2 = readPlain(300, 100);
    const pgot = new Uint8Array(memory.buffer, 2048, 100).slice();
    check(`worker: the plain file after the hold, one request (${p2}, ${counts[3] - pb})`,
      p2 === 100 && counts[3] - pb === 1 && pgot.every((x, i) => x === want[300 + i]));
    Date.now = now;
    console.error = quiet;
    const got = new Uint8Array(memory.buffer, 1024, 200);
    check(`worker: after the hold, read with one request (${r2}, ${counts[0] - before})`,
      r2 === 200 && counts[0] - before === 1 && got.every((x, i) => x === want[100 + i]));
  }

  // -- the owner's reads (lazy-cache-worker.js) -------------------------------
  {
    const counts = new Int32Array(new SharedArrayBuffer(16));
    const ctl = new SharedArrayBuffer((await import(gexUrl('hosts/browser/js/lazy-cache.mjs'))).HEAD + 32 * 1056);
    const memory = new WebAssembly.Memory({ initial: 4, maximum: 4, shared: true });
    const owner = new Worker(new URL(import.meta.url), { workerData: { role: 'owner', counts, delayMs: 300 } });
    owner.on('error', (e) => { console.log(`FAIL owner: ${e?.message ?? e}`); process.exit(1); });
    owner.postMessage({
      ctl, memory, cache: 'test', versions: {},
      zstd: new URL(gexUrl('page/vendor/zstddec/zstddec-stream.mjs')).href,
    });
    await new Promise((r) => owner.once('message', r));
    const readers = (n, from, len, url = URL_BIN + '#0') => Promise.all(Array.from({ length: n }, (_, i) => new Promise((resolve, reject) => {
      const w = new Worker(new URL(import.meta.url), { workerData: { role: 'reader', ctl, url, from: from + i, len, dst: 4096 + i * 512 } });
      w.once('message', resolve);
      w.once('error', reject);
    })));
    let got = await readers(1, 100, 200, `${BASE}plain.bin`);
    // (OWNER_FAILED: the reader does not ask the server itself after that)
    check(`owner: a plain file's failing chunk fails (${got[0]}) after ${TRIES} requests (${counts[3]})`, got[0] === OWNER_FAILED && counts[3] === TRIES);
    got = await readers(4, 100, 50, `${BASE}plain.bin`);
    check(`owner: 4 reads more of it, no request (${counts[3] - TRIES})`, got.every((n) => n === OWNER_FAILED) && counts[3] === TRIES);
    got = await readers(1, 100, 200);
    check(`owner: a read of a failing frame fails (${got[0]})`, got[0] === OWNER_FAILED);
    check(`owner: it asked ${TRIES} times (${counts[0]})`, counts[0] === TRIES);
    got = await readers(8, 100, 50);
    check(`owner: 8 reads more of it, no request (${counts[0] - TRIES})`, got.every((n) => n === OWNER_FAILED) && counts[0] === TRIES);
    // Readers of a frame not asked for yet, at once, with the server
    // answering: one request between them.
    Atomics.store(counts, 2, 1);
    const before = counts[0];
    got = await readers(8, 2 * FRAME + 10, 100);
    const mem = new Uint8Array(memory.buffer);
    const right = got.every((n, i) => n === 100 && mem.subarray(4096 + i * 512, 4096 + i * 512 + 100).every((x, j) => x === want[2 * FRAME + 10 + i + j]));
    check(`owner: 8 readers of one frame at once share one request (${counts[0] - before}), bytes right`, right && counts[0] - before === 1);
    await owner.terminate();
  }
  console.log(bad ? `${bad} FAILED` : 'lazy rest check ok');
  process.exit(bad ? 1 : 0);
} else if (workerData.role === 'owner') {
  const counts = workerData.counts;
  const serve = server(counts, workerData.delayMs);
  globalThis.caches = { open: async () => ({ put: async () => {}, delete: async () => {}, match: async () => null }) };
  const realFetch = globalThis.fetch;
  globalThis.fetch = async (url, opts = {}) => {
    // (the decoder's own module and wasm)
    if (!String(url).startsWith(BASE)) return realFetch(url, opts);
    const r = serve(String(url), parseRange(opts.headers?.Range));
    return new Response(r.body, { status: r.status });
  };
  globalThis.self = { postMessage: (m) => parentPort.postMessage(m) };
  await import(gexUrl('page/src/lazy-cache-worker.js'));
  parentPort.once('message', (data) => self.onmessage({ data }));
} else {
  const { setLazyCache, cachedRead } = await import(gexUrl('hosts/browser/js/lazy-cache.mjs'));
  setLazyCache({ ctl: workerData.ctl });
  parentPort.postMessage(cachedRead(workerData.url, workerData.from, workerData.dst, workerData.len));
}
