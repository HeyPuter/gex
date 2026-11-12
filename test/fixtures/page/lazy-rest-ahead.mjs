/**
 * Does the owner of the lazy files (page/src/lazy-cache-worker.js) download a rest bundle in the background
 * as the page asks (`{kind: 'ahead', bundles, paused}`, once the title files are in)?
 *
 *   - paused: no request; unpaused: one request at fetch priority 'low', from the first frame to the end,
 *     each frame stored in the page's cache as it completes; the page hears its progress and its end;
 *   - reads of every frame after it: no request, the bytes right;
 *   - asked again (a later visit, the cache holding every frame): no request at all;
 *   - paused in the middle: the request stops; unpaused, it is taken up from the first frame not held;
 *   - a read of a frame the download has not reached yet: its own ranged request at once, not 'low'.
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
  return { bin, frames, json: JSON.stringify({ frame: FRAME, frames, h: 'cafe' }), plain: p };
}
const BASE = 'http://site.test/';
const URL_BIN = `${BASE}rest.bin`;
const parseRange = (h) => {
  const m = /bytes=(\d+)-(\d+)/.exec(h ?? '');
  return m ? [Number(m[1]), Number(m[2])] : null;
};
// log: [first frame, low (1/0), frames] a request, as many as LOG; log[0] the count
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

  const doneMsg = (r) => r.msgs.find((m) => m.kind === 'ahead');
  const until = async (f, n = 400) => { for (let i = 0; i < n && !f(); i++) await sleep(20); };
  // -- paused, then not: one low request for all of it; reads after it, and a later visit, ask for nothing ----
  {
    const r = await run(20);
    r.owner.postMessage({ kind: 'ahead', bundles: [URL_BIN], paused: true });
    await sleep(300);
    check(`paused: no request (${r.reqs().length})`, r.reqs().length === 0);
    r.owner.postMessage({ kind: 'ahead', paused: false });
    await until(() => doneMsg(r));
    const q = r.reqs();
    check(`unpaused: one request, frames 0 to the end (${JSON.stringify(q)})`, q.length === 1 && q[0].frame === 0 && q[0].inflight === NFRAMES);
    check('at priority low', q.every((x) => x.low === 1));
    const done = doneMsg(r), prog = r.msgs.filter((m) => m.kind === 'rest');
    check(`it says so (${JSON.stringify(done)})`, done?.frames === NFRAMES && done?.bytes > NFRAMES * FRAME);
    check(`and how far it got (${JSON.stringify(prog.at(-1))})`, prog.length > 0 && prog.at(-1).done === prog.at(-1).total && prog.at(-1).total > NFRAMES * FRAME);
    await sleep(50);
    const got = await Promise.all(Array.from({ length: NFRAMES }, (_, k) => r.read(k * FRAME + 5, 100, 4096 + k * 512)));
    check(`reads of every frame after it: no request (${r.reqs().length - 1})`, r.reqs().length === 1);
    check('and the bytes right', got.every((n, k) => n === 100 && r.mem.subarray(4096 + k * 512, 4096 + k * 512 + 100).every((x, j) => x === want[k * FRAME + 5 + j])));
    r.msgs.length = 0;
    r.owner.postMessage({ kind: 'ahead', bundles: [URL_BIN] });
    await until(() => doneMsg(r));
    const later = r.msgs.filter((m) => m.kind === 'rest').at(-1);
    check(`a later visit: no request (${r.reqs().length - 1}), nothing to download (${JSON.stringify(later)})`, r.reqs().length === 1 && later?.total === 0);
    await r.owner.terminate();
  }

  // -- paused in the middle: stopped, then taken up from the first frame not held ------------------------
  {
    const r = await run(100);
    r.owner.postMessage({ kind: 'ahead', bundles: [URL_BIN] });
    await until(() => r.msgs.some((m) => m.kind === 'rest' && m.done >= 2 * FRAME));
    r.owner.postMessage({ kind: 'ahead', paused: true });
    await sleep(400);
    const n0 = r.reqs().length;
    r.owner.postMessage({ kind: 'ahead', paused: false });
    await until(() => doneMsg(r));
    const q = r.reqs();
    check(`taken up from a later frame (${JSON.stringify(q)})`, n0 === 1 && q.length === 2 && q[1].frame > 0 && q[1].frame + q[1].inflight === NFRAMES);
    const got = await Promise.all(Array.from({ length: NFRAMES }, (_, k) => r.read(k * FRAME + 9, 64, 4096 + k * 512)));
    check(`every frame there after it (${r.reqs().length - 2} more requests)`, r.reqs().length === 2 && got.every((n, k) => n === 64 && r.mem.subarray(4096 + k * 512, 4096 + k * 512 + 64).every((x, j) => x === want[k * FRAME + 9 + j])));
    await r.owner.terminate();
  }

  // -- a read of a frame the download has not reached: its own request at once, not low ------------------
  {
    const r = await run(150);
    r.owner.postMessage({ kind: 'ahead', bundles: [URL_BIN] });
    await sleep(60);
    const t0 = Date.now();
    const n = await r.read((NFRAMES - 1) * FRAME + 7, 50, 4096);
    const took = Date.now() - t0;
    const mine = r.reqs().find((x) => x.frame === NFRAMES - 1);
    check(`the read's frame asked for at once, not low (${JSON.stringify(mine)}, ${took} ms)`, mine && mine.low === 0 && took < 150 * 3);
    check('its bytes right', n === 50 && r.mem.subarray(4096, 4146).every((x, j) => x === want[(NFRAMES - 1) * FRAME + 7 + j]));
    await until(() => doneMsg(r));
    check('the download still ends', !!doneMsg(r));
    await r.owner.terminate();
  }
  console.log(bad ? `${bad} FAILED` : 'lazy rest ahead check ok');
  process.exit(bad ? 1 : 0);
} else if (workerData.role === 'owner') {
  const { bin, frames, json } = bundle();
  const log = workerData.log;
  // the page's cache: kept in this realm
  const store = new Map();
  globalThis.caches = { open: async () => ({
    put: async (k, res) => { store.set(String(k), new Uint8Array(await res.arrayBuffer())); },
    delete: async (k) => store.delete(String(k?.url ?? k)),
    match: async (k) => (store.has(String(k?.url ?? k)) ? new Response(store.get(String(k?.url ?? k))) : undefined),
    keys: async () => [...store.keys()].map((url) => ({ url })),
  }) };
  const realFetch = globalThis.fetch;
  // a frame's worth of bytes each `delayMs`, stopped by the request's signal
  globalThis.fetch = async (url, opts = {}) => {
    if (!String(url).startsWith(BASE)) return realFetch(url, opts);
    if (String(url).endsWith('.json')) return new Response(json, { status: 200 });
    const range = parseRange(opts.headers?.Range);
    const k = frames.findIndex(([o]) => o === range[0]);
    const kEnd = frames.findIndex(([o, n]) => o + n === range[1] + 1);
    const i = Atomics.add(log, 0, 1);
    if (i < LOG) { log[1 + 3 * i] = k; log[2 + 3 * i] = opts.priority === 'low' ? 1 : 0; log[3 + 3 * i] = kEnd - k + 1; }
    const body = bin.slice(range[0], range[1] + 1);
    let at = 0;
    const stream = new ReadableStream({
      async pull(c) {
        await new Promise((r) => setTimeout(r, workerData.delayMs));
        if (opts.signal?.aborted) { c.error(new Error('aborted')); return; }
        if (at >= body.length) { c.close(); return; }
        const n = Math.min(frames[0][1], body.length - at);
        c.enqueue(body.slice(at, at + n));
        at += n;
      },
    });
    return new Response(stream, { status: 206 });
  };
  globalThis.self = { postMessage: (m) => parentPort.postMessage(m) };
  await import(gexUrl('page/src/lazy-cache-worker.js'));
  parentPort.on('message', (data) => self.onmessage({ data }));
} else {
  const { setLazyCache, cachedRead } = await import(gexUrl('hosts/browser/js/lazy-cache.mjs'));
  setLazyCache({ ctl: workerData.ctl });
  parentPort.postMessage(cachedRead(workerData.url, workerData.from, workerData.dst, workerData.len));
}
