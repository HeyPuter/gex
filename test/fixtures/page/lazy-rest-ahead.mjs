/**
 * Does the owner of the lazy files (page/src/lazy-cache-worker.js) download a rest bundle in the background
 * as the page asks (`{kind: 'ahead', bundles, paused}`, once the title files are in)?
 *
 * The server here logs every byte it sends (a request's first byte and how many it sent before it ended or
 * was cut), and every case checks what went over the wire against the bundle:
 *   - paused: no request; unpaused: windows of at most GEX_AHEAD_WINDOW bytes at fetch priority 'low', one
 *     after another, every byte of the bundle sent once; the page hears {done, total} with one total, done
 *     never going back and ending equal to it;
 *   - reads of every frame after it: no request, the bytes right;
 *   - a second visit (a new owner over the same page cache): no request, nothing to download;
 *   - paused in a window and taken up, and a window cut by the network: every byte still sent once (a frame
 *     cut in the middle is taken up from its next byte);
 *   - a read of a frame near the end while the download is about 10% through: its own ranged request at once,
 *     not 'low'; the bytes sent twice at most the window in flight then;
 *   - a first visit cut short, then a second: the second asks only for the frames the first did not keep;
 *   - a page cache that refuses every frame (its quota): every byte still sent once, and no refetch loop.
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
const NFRAMES = 20;
// a window: two frames and a half (each frame's zstd adds 12 bytes)
const WINDOW = Math.floor(FRAME * 2.5);

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
// the bundle as the page names it to the owner (app.js: a path on the origin), a read naming it whole
const PAGE_BIN = '/rest.bin';
const parseRange = (h) => {
  const m = /bytes=(\d+)-(\d+)/.exec(h ?? '');
  return m ? [Number(m[1]), Number(m[2])] : null;
};
// log: per request [first byte, bytes asked, bytes sent, low (1/0)], as many as LOG; log[0] the count
const LOG = 256, F = 4;

if (isMainThread) {
  let bad = 0;
  const check = (what, ok, more = '') => {
    if (!ok) bad++;
    console.log(`${ok ? 'ok  ' : 'FAIL'} ${what}${ok ? '' : ` ${more}`}`);
  };
  const { plain: want, bin, frames } = bundle();
  const SIZE = bin.length;
  const { HEAD } = await import(gexUrl('hosts/browser/js/lazy-cache.mjs'));
  const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
  // owner options: delayMs a chunk (a quarter frame), store: a page cache to start from (a later visit's),
  // quota: every put refused, cut: [request number, bytes]: that request ends in an error after so many bytes
  const run = async ({ delayMs = 5, store = [], quota = false, cut = null } = {}) => {
    const log = new Int32Array(new SharedArrayBuffer(4 * (1 + F * LOG)));
    const ctl = new SharedArrayBuffer(HEAD + 32 * 1056);
    const memory = new WebAssembly.Memory({ initial: 4, maximum: 4, shared: true });
    const owner = new Worker(new URL(import.meta.url), { workerData: { role: 'owner', log, delayMs, store, quota, cut, window: WINDOW } });
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
    const reqs = () => Array.from({ length: Math.min(log[0], LOG) }, (_, i) => ({ lo: log[1 + F * i], asked: log[2 + F * i], sent: log[3 + F * i], low: log[4 + F * i] }));
    const dump = () => new Promise((r) => { owner.on('message', (m) => { if (m.kind === 'store') r(m.entries); }); owner.postMessage({ kind: 'dump' }); });
    return { owner, msgs, read, reqs, dump, mem: new Uint8Array(memory.buffer) };
  };
  // bytes sent, and how many of them were sent before (by an earlier request)
  const wire = (q) => {
    const seen = new Uint8Array(SIZE);
    let sent = 0, twice = 0;
    for (const x of q) for (let i = x.lo; i < x.lo + x.sent; i++) { sent++; if (seen[i]++) twice++; }
    return { sent, twice, missing: seen.filter((c) => !c).length };
  };
  // the progress the page heard: one total, done never back, at most the total, the last done equal to it
  const progress = (r) => {
    const p = r.msgs.filter((m) => m.kind === 'rest');
    const totals = new Set(p.map((m) => m.total));
    const ok = p.length > 0 && totals.size === 1 && p.every((m, i) => m.done <= m.total && (!i || m.done >= p[i - 1].done)) && p.at(-1).done === p.at(-1).total;
    return { ok, last: p.at(-1), n: p.length, totals: [...totals] };
  };
  const doneMsg = (r) => r.msgs.find((m) => m.kind === 'ahead');
  const until = async (f, n = 1500) => { for (let i = 0; i < n && !f(); i++) await sleep(20); };
  const sentBy = (r) => r.reqs().reduce((a, x) => a + x.sent, 0);
  const readsRight = async (r, off) => {
    const got = await Promise.all(Array.from({ length: NFRAMES }, (_, k) => r.read(k * FRAME + off, 64, 4096 + k * 128)));
    return got.every((n, k) => n === 64 && r.mem.subarray(4096 + k * 128, 4096 + k * 128 + 64).every((x, j) => x === want[k * FRAME + off + j]));
  };

  // -- paused, then not: windows at low priority, every byte once; reads after it and a second visit ask nothing
  {
    const r = await run();
    r.owner.postMessage({ kind: 'ahead', bundles: [PAGE_BIN], paused: true });
    await sleep(300);
    check(`paused: no request (${r.reqs().length})`, r.reqs().length === 0);
    r.owner.postMessage({ kind: 'ahead', paused: false });
    await until(() => doneMsg(r));
    const q = r.reqs(), w = wire(q), p = progress(r);
    check(`unpaused: windows of at most ${WINDOW} bytes (${q.length}: ${q.map((x) => x.asked).join(' ')})`, q.length > 1 && q.every((x) => x.asked <= WINDOW || x.asked === frames[0][1]));
    check('at priority low', q.every((x) => x.low === 1));
    check(`every byte of the bundle sent once (${JSON.stringify(w)} of ${SIZE})`, w.sent === SIZE && w.twice === 0 && w.missing === 0);
    check(`the page heard one total, done up to it (${JSON.stringify(p)})`, p.ok && p.last.total === SIZE);
    const done = doneMsg(r);
    check(`and the end (${JSON.stringify(done)})`, done?.frames === NFRAMES && done?.bytes === SIZE);
    await sleep(50);
    const n0 = r.reqs().length;
    check('reads of every frame after it: the bytes right', await readsRight(r, 5));
    check(`and no request (${r.reqs().length - n0})`, r.reqs().length === n0);
    const store = await r.dump();
    await r.owner.terminate();
    const v = await run({ store });
    v.owner.postMessage({ kind: 'ahead', bundles: [PAGE_BIN] });
    await until(() => doneMsg(v));
    const last = v.msgs.filter((m) => m.kind === 'rest').at(-1);
    check(`a second visit: no request (${v.reqs().length}), nothing to download (${JSON.stringify(last)})`, v.reqs().length === 0 && last?.total === 0 && last?.done === 0);
    check('and its reads right, with no request', (await readsRight(v, 11)) && v.reqs().length === 0);
    await v.owner.terminate();
  }

  // -- paused in a window, then taken up; a window the network cuts: every byte still sent once -------------
  for (const how of ['paused', 'cut']) {
    const r = await run(how === 'cut' ? { delayMs: 10, cut: [1, Math.floor(FRAME * 1.4)] } : { delayMs: 10 });
    r.owner.postMessage({ kind: 'ahead', bundles: [PAGE_BIN] });
    if (how === 'paused') {
      await until(() => sentBy(r) >= Math.floor(FRAME * 3.3));
      r.owner.postMessage({ kind: 'ahead', paused: true });
      await sleep(300);
      const s0 = sentBy(r);
      await sleep(300);
      check(`paused in a window: nothing sent while paused (${sentBy(r) - s0})`, sentBy(r) === s0);
      r.owner.postMessage({ kind: 'ahead', paused: false });
    }
    await until(() => doneMsg(r), 3000);
    const q = r.reqs(), w = wire(q), p = progress(r);
    check(`${how}: taken up, every byte sent once (${JSON.stringify(w)} of ${SIZE}, ${q.length} requests)`, w.sent === SIZE && w.twice === 0 && w.missing === 0 && q.some((x) => x.sent < x.asked));
    check(`${how}: one total, done up to it (${JSON.stringify(p)})`, p.ok && p.last.total === SIZE);
    check(`${how}: every frame there after it`, await readsRight(r, 9));
    await r.owner.terminate();
  }

  // -- a read near the end while the download is about 10% through: its own request at once ---------------
  {
    const r = await run({ delayMs: 25 });
    r.owner.postMessage({ kind: 'ahead', bundles: [PAGE_BIN] });
    await until(() => sentBy(r) >= Math.floor(SIZE / 10));
    const inflight = r.reqs().at(-1);
    const t0 = Date.now();
    const n = await r.read((NFRAMES - 2) * FRAME + 7, 50, 4096);
    const took = Date.now() - t0;
    const mine = r.reqs().find((x) => x.lo === frames[NFRAMES - 2][0] && x.low === 0);
    check(`the read's frame asked for at once, not low (${JSON.stringify(mine)}, ${took} ms)`, mine && took < 25 * 4 * 3);
    check('its bytes right', n === 50 && r.mem.subarray(4096, 4146).every((x, j) => x === want[(NFRAMES - 2) * FRAME + 7 + j]));
    await until(() => doneMsg(r), 3000);
    const w = wire(r.reqs()), p = progress(r);
    check(`every byte sent, twice at most the window in flight then (${JSON.stringify(w)} of ${SIZE}; in flight ${JSON.stringify(inflight)})`, w.missing === 0 && w.sent <= SIZE + inflight.asked && w.twice <= inflight.asked);
    check(`the stream never asked for the read's frame (${JSON.stringify(r.reqs().filter((x) => x.low === 1 && x.lo <= frames[NFRAMES - 2][0] && x.lo + x.asked > frames[NFRAMES - 2][0]))})`,
      !r.reqs().some((x) => x.low === 1 && x.lo <= frames[NFRAMES - 2][0] && x.lo + x.asked > frames[NFRAMES - 2][0]));
    check(`one total, done up to it (${JSON.stringify(p)})`, p.ok && p.last.total === SIZE);
    await r.owner.terminate();
  }

  // -- a first visit cut short, then a second: only the frames the first did not keep ---------------------
  {
    const r = await run({ delayMs: 10 });
    r.owner.postMessage({ kind: 'ahead', bundles: [PAGE_BIN] });
    await until(() => sentBy(r) >= Math.floor(SIZE * 0.4));
    const store = await r.dump();
    await r.owner.terminate();
    const keptFrames = store.length, keptBytes = store.reduce((a, [, b]) => a + b.length, 0);
    const v = await run({ store });
    v.owner.postMessage({ kind: 'ahead', bundles: [PAGE_BIN] });
    await until(() => doneMsg(v), 3000);
    const w = wire(v.reqs()), p = progress(v);
    check(`a second visit after a cut first one (${keptFrames} frames kept): only the rest asked for (${JSON.stringify(w)}, want ${SIZE - keptBytes})`,
      keptFrames > 0 && w.sent === SIZE - keptBytes && w.twice === 0);
    check(`its total those bytes, done up to it (${JSON.stringify(p)})`, p.ok && p.last.total === SIZE - keptBytes);
    check('every frame there after it', await readsRight(v, 3));
    await v.owner.terminate();
  }

  // -- a page cache that refuses every frame: each byte still sent once, no loop --------------------------
  {
    const r = await run({ quota: true });
    r.owner.postMessage({ kind: 'ahead', bundles: [PAGE_BIN] });
    await until(() => doneMsg(r), 3000);
    await sleep(200);
    const w = wire(r.reqs()), p = progress(r);
    check(`a full page cache: every byte sent once (${JSON.stringify(w)} of ${SIZE})`, w.sent === SIZE && w.twice === 0);
    check(`one total, done up to it (${JSON.stringify(p)})`, p.ok && p.last.total === SIZE);
    const n0 = r.reqs().length;
    check(`reads after it right, from the kept bytes (${r.reqs().length - n0} requests)`, (await readsRight(r, 1)) && r.reqs().length === n0);
    await r.owner.terminate();
  }
  console.log(bad ? `${bad} FAILED` : 'lazy rest ahead check ok');
  process.exit(bad ? 1 : 0);
} else if (workerData.role === 'owner') {
  const { bin, frames, json } = bundle();
  const { log, quota, cut } = workerData;
  // the page's cache: kept in this realm, from the visit before's
  const store = new Map(workerData.store);
  globalThis.caches = { open: async () => ({
    put: async (k, res) => {
      const b = new Uint8Array(await res.arrayBuffer());
      // (the store's probe goes in; a frame does not)
      if (quota && String(k).includes('f=')) throw new Error('QuotaExceededError');
      store.set(String(k), b);
    },
    delete: async (k) => store.delete(String(k?.url ?? k)),
    match: async (k) => (store.has(String(k?.url ?? k)) ? new Response(store.get(String(k?.url ?? k))) : undefined),
    keys: async () => [...store.keys()].map((url) => ({ url })),
  }) };
  const realFetch = globalThis.fetch;
  // a quarter frame each `delayMs`, stopped by the request's signal; every byte sent logged
  globalThis.fetch = async (url, opts = {}) => {
    if (!String(url).startsWith(BASE)) return realFetch(url, opts);
    if (String(url).endsWith('.json')) return new Response(json, { status: 200 });
    const range = parseRange(opts.headers?.Range);
    const i = Atomics.add(log, 0, 1);
    const at = (f) => (i < LOG ? 1 + F * i + f : 0);
    if (i < LOG) { log[at(0)] = range[0]; log[at(1)] = range[1] + 1 - range[0]; log[at(3)] = opts.priority === 'low' ? 1 : 0; }
    const body = bin.slice(range[0], range[1] + 1);
    let sent = 0;
    const stream = new ReadableStream({
      async pull(c) {
        await new Promise((r) => setTimeout(r, workerData.delayMs));
        if (opts.signal?.aborted) { c.error(new Error('aborted')); return; }
        if (cut && cut[0] === i && sent >= cut[1]) { c.error(new Error('connection reset')); return; }
        if (sent >= body.length) { c.close(); return; }
        let n = Math.min(frames[0][1] >> 2, body.length - sent);
        if (cut && cut[0] === i) n = Math.min(n, cut[1] - sent);
        c.enqueue(body.slice(sent, sent + n));
        sent += n;
        if (i < LOG) Atomics.add(log, at(2), n);
      },
    });
    return new Response(stream, { status: 206 });
  };
  globalThis.self = { postMessage: (m) => parentPort.postMessage(m) };
  globalThis.GEX_BASE = BASE;
  globalThis.GEX_AHEAD_WINDOW = workerData.window;
  await import(gexUrl('page/src/lazy-cache-worker.js'));
  parentPort.on('message', (data) => {
    if (data?.kind === 'dump') parentPort.postMessage({ kind: 'store', entries: [...store].filter(([k]) => k.includes('f=')) });
    else self.onmessage({ data });
  });
} else {
  const { setLazyCache, cachedRead } = await import(gexUrl('hosts/browser/js/lazy-cache.mjs'));
  setLazyCache({ ctl: workerData.ctl });
  parentPort.postMessage(cachedRead(workerData.url, workerData.from, workerData.dst, workerData.len));
}
