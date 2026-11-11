/**
 * Does the owner of the lazy files (page/src/lazy-cache-worker.js) fetch a rest bundle in the background
 * as the page asks (`{kind: 'ahead', bundles, paused}`), the way the page does once the start-up is over?
 *
 *   - paused: no request; unpaused: every frame, one request each, each at fetch priority 'low', one at a time;
 *   - reads of every frame after it: no request, the bytes right;
 *   - a read of a frame not fetched yet while the background runs: its own request goes out at once (not
 *     'low', not behind the background's), and no frame is asked for twice.
 *
 * Usage: node test/fixtures/page/lazy-rest-ahead.mjs
 */
import { Worker, isMainThread, workerData, parentPort } from 'node:worker_threads';
import { pathToFileURL as gexPathToFileURL } from 'node:url';
import { join as gexJoin } from 'node:path';
// gex: the checkout these fixtures are in, or GEX_ROOT.
const GEX_ROOT = process.env.GEX_ROOT || gexJoin(import.meta.dirname, '..', '..', '..');
const gexUrl = (p) => gexPathToFileURL(gexJoin(GEX_ROOT, p)).href;

const FRAME = 4096;
const NFRAMES = 6;

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
  out[4] = 0xa0;
  v.setUint32(5, b.length, true);
  const bh = 1 | (0 << 1) | (b.length << 3);
  out[9] = bh & 0xff; out[10] = (bh >> 8) & 0xff; out[11] = (bh >> 16) & 0xff;
  out.set(b, 12);
  return out;
}
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
  return { bin, frames, json: JSON.stringify({ frame: FRAME, frames }), plain: p };
}
const BASE = 'http://site.test/';
const URL_BIN = `${BASE}rest.bin`;
const parseRange = (h) => {
  const m = /bytes=(\d+)-(\d+)/.exec(h ?? '');
  return m ? [Number(m[1]), Number(m[2])] : null;
};
// log: [frame, low (1/0), in flight at its start] a request, as many as LOG; log[0] the count
const LOG = 64;

if (isMainThread) {
  let bad = 0;
  const check = (what, ok, more = '') => {
    if (!ok) bad++;
    console.log(`${ok ? 'ok  ' : 'FAIL'} ${what}${ok ? '' : ` ${more}`}`);
  };
  const { plain: want } = bundle();
  const { HEAD } = await import(gexUrl('hosts/browser/js/lazy-cache.mjs'));
  const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
  const run = async (delayMs) => {
    const log = new Int32Array(new SharedArrayBuffer(4 * (1 + 3 * LOG)));
    const ctl = new SharedArrayBuffer(HEAD + 32 * 1056);
    const memory = new WebAssembly.Memory({ initial: 4, maximum: 4, shared: true });
    const owner = new Worker(new URL(import.meta.url), { workerData: { role: 'owner', log, delayMs } });
    owner.on('error', (e) => { console.log(`FAIL owner: ${e?.message ?? e}`); process.exit(1); });
    const msgs = [];
    owner.on('message', (m) => msgs.push(m));
    owner.postMessage({ ctl, memory, cache: 'test', versions: {}, zstd: new URL(gexUrl('page/vendor/zstddec/zstddec-stream.mjs')).href });
    while (!msgs.length) await sleep(10);
    const read = (from, len, dst) => new Promise((resolve, reject) => {
      const w = new Worker(new URL(import.meta.url), { workerData: { role: 'reader', ctl, url: URL_BIN + '#0', from, len, dst } });
      w.once('message', resolve);
      w.once('error', reject);
    });
    const reqs = () => Array.from({ length: log[0] }, (_, i) => ({ frame: log[1 + 3 * i], low: log[2 + 3 * i], inflight: log[3 + 3 * i] }));
    return { owner, msgs, read, reqs, log, mem: new Uint8Array(memory.buffer) };
  };

  // -- paused, then not: every frame once, low, one at a time; reads after it ask for nothing ---------------
  {
    const r = await run(20);
    r.owner.postMessage({ kind: 'ahead', bundles: [URL_BIN], paused: true });
    await sleep(300);
    check(`paused: no request (${r.reqs().length})`, r.reqs().length === 0);
    r.owner.postMessage({ kind: 'ahead', paused: false });
    for (let i = 0; i < 200 && !r.msgs.some((m) => m.kind === 'ahead'); i++) await sleep(20);
    const q = r.reqs();
    const done = r.msgs.find((m) => m.kind === 'ahead');
    check(`unpaused: every frame once (${q.map((x) => x.frame).join(',')})`, q.length === NFRAMES && new Set(q.map((x) => x.frame)).size === NFRAMES);
    check('each at priority low', q.every((x) => x.low === 1));
    check('one at a time', q.every((x) => x.inflight === 0));
    check(`it says so (${JSON.stringify(done)})`, done?.frames === NFRAMES && done?.bytes > NFRAMES * FRAME);
    const got = await Promise.all(Array.from({ length: NFRAMES }, (_, k) => r.read(k * FRAME + 5, 100, 4096 + k * 512)));
    check(`reads of every frame after it: no request (${r.reqs().length - NFRAMES})`, r.reqs().length === NFRAMES);
    check('and the bytes right', got.every((n, k) => n === 100 && r.mem.subarray(4096 + k * 512, 4096 + k * 512 + 100).every((x, j) => x === want[k * FRAME + 5 + j])));
    await r.owner.terminate();
  }

  // -- a read while the background runs: its own request at once, not low; no frame twice ------------------
  {
    const r = await run(150);
    r.owner.postMessage({ kind: 'ahead', bundles: [URL_BIN] });
    await sleep(60);
    const t0 = Date.now();
    const n = await r.read((NFRAMES - 1) * FRAME + 7, 50, 4096);
    const took = Date.now() - t0;
    const q = r.reqs();
    const mine = q.find((x) => x.frame === NFRAMES - 1);
    check(`the read's frame asked for at once, not low (${JSON.stringify(mine)}, ${took} ms)`, mine && mine.low === 0 && took < 150 * 3);
    check('its bytes right', n === 50 && r.mem.subarray(4096, 4146).every((x, j) => x === want[(NFRAMES - 1) * FRAME + 7 + j]));
    for (let i = 0; i < 300 && !r.msgs.some((m) => m.kind === 'ahead'); i++) await sleep(20);
    const all = r.reqs().map((x) => x.frame);
    check(`no frame twice (${all.join(',')})`, all.length === NFRAMES && new Set(all).size === NFRAMES);
    await r.owner.terminate();
  }
  console.log(bad ? `${bad} FAILED` : 'lazy rest ahead check ok');
  process.exit(bad ? 1 : 0);
} else if (workerData.role === 'owner') {
  const { bin, frames, json } = bundle();
  const log = workerData.log;
  let inflight = 0;
  globalThis.caches = { open: async () => ({ put: async () => {}, delete: async () => {}, match: async () => null }) };
  const realFetch = globalThis.fetch;
  globalThis.fetch = async (url, opts = {}) => {
    if (!String(url).startsWith(BASE)) return realFetch(url, opts);
    if (String(url).endsWith('.json')) return new Response(json, { status: 200 });
    const range = parseRange(opts.headers?.Range);
    const k = frames.findIndex(([o]) => o === range[0]);
    const i = Atomics.add(log, 0, 1);
    if (i < LOG) { log[1 + 3 * i] = k; log[2 + 3 * i] = opts.priority === 'low' ? 1 : 0; log[3 + 3 * i] = inflight; }
    inflight++;
    await new Promise((r) => setTimeout(r, workerData.delayMs));
    inflight--;
    return new Response(bin.slice(range[0], range[1] + 1), { status: 206 });
  };
  globalThis.self = { postMessage: (m) => parentPort.postMessage(m) };
  await import(gexUrl('page/src/lazy-cache-worker.js'));
  parentPort.on('message', (data) => self.onmessage({ data }));
} else {
  const { setLazyCache, cachedRead } = await import(gexUrl('hosts/browser/js/lazy-cache.mjs'));
  setLazyCache({ ctl: workerData.ctl });
  parentPort.postMessage(cachedRead(workerData.url, workerData.from, workerData.dst, workerData.len));
}
