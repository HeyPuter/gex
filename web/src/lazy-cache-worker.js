// The owner of the lazy files' cache (shims/browser/js/lazy-cache.mjs): a
// Worker's read of a lazy file asks here, through the control buffer's
// slots, and waits; this answers from the page's Cache Storage, or fetches
// what it lacks (one ranged request a run of missing chunks), keeps it, and
// writes the bytes into the kernel's memory. No service worker.
//
// In: { ctl: SharedArrayBuffer, memory: the kernel's WebAssembly.Memory,
//       cache: the Cache Storage name }
// Out: { kind: 'ready', ok, why? } once; then { kind: 'bytes', net, cached }
//      now and then (bytes from the network, from the cache, since the last).

import { CHUNK, SLOTS, HEAD, SLOT, FREE, ASKED, WRITING, DONE, GIVEN_UP } from '../../shims/browser/js/lazy-cache.mjs';

let net = 0, kept = 0, posted = 0;
const report = () => {
  if (net + kept === 0 || performance.now() - posted < 250) return;
  posted = performance.now();
  self.postMessage({ kind: 'bytes', net, cached: kept });
  net = 0;
  kept = 0;
};
setInterval(() => { posted = 0; report(); }, 1000);

const keyOf = (url, k) => `${url}?c=${k}`;

/** The bytes of `url` from `from`, `len` of them: each chunk from the cache,
 *  the missing ones fetched in runs and kept. */
async function read(cache, url, from, len) {
  const first = Math.floor(from / CHUNK), last = Math.floor((from + len - 1) / CHUNK);
  const chunks = new Map();
  await Promise.all(Array.from({ length: last - first + 1 }, async (_, i) => {
    const k = first + i;
    const r = await cache.match(keyOf(url, k)).catch(() => null);
    if (r?.ok) {
      const b = new Uint8Array(await r.arrayBuffer());
      chunks.set(k, b);
      kept += b.length;
    }
  }));
  for (let k = first; k <= last;) {
    if (chunks.has(k)) { k++; continue; }
    let stop = k;
    while (stop < last && !chunks.has(stop + 1)) stop++;
    const lo = k * CHUNK, hi = (stop + 1) * CHUNK - 1;
    let got = null;
    for (let attempt = 0; attempt < 3 && !got; attempt++) {
      try {
        const res = await fetch(url, { headers: { Range: `bytes=${lo}-${hi}` }, cache: 'no-store' });
        const body = new Uint8Array(await res.arrayBuffer());
        if (res.status === 206) got = body;
        else if (res.status === 200) got = body.subarray(lo, hi + 1);
      } catch { /* again */ }
    }
    if (!got) throw new Error(`${url} @${lo}: no answer`);
    net += got.length;
    for (let c = k; c <= stop; c++) {
      const b = got.slice((c - k) * CHUNK, (c - k + 1) * CHUNK);
      if (!b.length) break;
      chunks.set(c, b);
      // The file's end: a short last chunk is whole; any other is not kept.
      if (b.length === CHUNK || c === stop) cache.put(keyOf(url, c), new Response(b)).catch(() => {});
    }
    k = stop + 1;
  }
  const out = new Uint8Array(len);
  for (let at = from; at < from + len;) {
    const k = Math.floor(at / CHUNK);
    const b = chunks.get(k);
    const off = at - k * CHUNK;
    if (!b || off >= b.length) throw new Error(`${url} @${at}: short`);
    const n = Math.min(b.length - off, from + len - at);
    out.set(b.subarray(off, off + n), at - from);
    at += n;
  }
  return out;
}

self.onmessage = async ({ data: m }) => {
  const ctl = new Int32Array(m.ctl);
  const raw = new Uint8Array(m.ctl);
  const memory = m.memory;
  let cache = null;
  try {
    cache = await caches.open(m.cache);
    // A store that opens but cannot hold anything (some insecure origins).
    await cache.put('/__gex-probe', new Response('1'));
    await cache.delete('/__gex-probe');
  } catch (e) {
    self.postMessage({ kind: 'ready', ok: false, why: String(e?.message ?? e) });
    return;
  }
  if (typeof Atomics.waitAsync !== 'function') {
    self.postMessage({ kind: 'ready', ok: false, why: 'no Atomics.waitAsync' });
    return;
  }
  self.postMessage({ kind: 'ready', ok: true });
  const busy = new Set();
  const serve = async (i) => {
    const base = (HEAD + i * SLOT) / 4;
    const len = ctl[base + 3] >>> 0, dst = ctl[base + 2] >>> 0, ul = ctl[base + 4];
    const from = new Float64Array(m.ctl, HEAD + i * SLOT + 24, 1)[0];
    const url = new TextDecoder().decode(raw.slice(HEAD + i * SLOT + 32, HEAD + i * SLOT + 32 + ul));
    let bytes = null;
    try {
      bytes = await read(cache, url, from, len);
    } catch (e) {
      console.warn(`[lazy-cache] ${e?.message ?? e}`);
    }
    // The reader may have gone to the network meanwhile: then nothing is
    // written into its buffer, which may be gone.
    if (Atomics.compareExchange(ctl, base, ASKED, WRITING) !== ASKED) {
      Atomics.store(ctl, base, FREE);
    } else {
      if (bytes) new Uint8Array(memory.buffer, dst, len).set(bytes);
      ctl[base + 1] = bytes ? len : -1;
      Atomics.store(ctl, base, DONE);
      Atomics.notify(ctl, base);
    }
    busy.delete(i);
    report();
  };
  for (;;) {
    const seen = Atomics.load(ctl, 0);
    for (let i = 0; i < SLOTS; i++) {
      const st = Atomics.load(ctl, (HEAD + i * SLOT) / 4);
      if (st === GIVEN_UP && !busy.has(i)) Atomics.store(ctl, (HEAD + i * SLOT) / 4, FREE);
      else if (st === ASKED && !busy.has(i)) {
        busy.add(i);
        serve(i);
      }
    }
    const w = Atomics.waitAsync(ctl, 0, seen);
    if (w.async) await w.value;
  }
};
