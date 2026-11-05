// The owner of the lazy files' cache (hosts/browser/js/lazy-cache.mjs): a
// Worker's read of a lazy file asks here, through the control buffer's
// slots, and waits; this answers from the page's Cache Storage, or fetches
// what it lacks (one ranged request a run of missing chunks), keeps it, and
// writes the bytes into the kernel's memory. No service worker.
//
// A file in the site's rest bundle (`BUNDLE#OFFSET`, lazy-fetch.mjs) is read
// here too: its frames fetched, decoded with the one decoder here and the
// last few kept decoded, for every Worker. A decoder in each Worker was an
// engine memory reservation in each, and a desktop's few hundred Workers ran
// the page out of them.
//
// In: { ctl: SharedArrayBuffer, memory: the kernel's WebAssembly.Memory,
//       cache: the Cache Storage name, versions: { URL path: version } of
//       the lazy files (lazy-cache.mjs lazyVersion), zstd: the decoder's
//       module URL, or true for the one bundled here, for a rest bundle }
// Out: { kind: 'ready', ok, why? } once; then { kind: 'bytes', net, cached }
//      now and then (bytes from the network, from the cache, since the last).

import { CHUNK, SLOTS, HEAD, SLOT, FREE, ASKED, TAKEN, WRITING, DONE, GIVEN_UP, chunkKey, FailedFrames, retryAsync } from '../../hosts/browser/js/lazy-cache.mjs';

let net = 0, kept = 0, posted = 0;
const report = () => {
  if (net + kept === 0 || performance.now() - posted < 250) return;
  posted = performance.now();
  self.postMessage({ kind: 'bytes', net, cached: kept });
  net = 0;
  kept = 0;
};
setInterval(() => { posted = 0; report(); }, 1000);

let versions = {};
const keyOf = (url, k) => {
  let path = url;
  try { path = new URL(url).pathname; } catch { /* a path already */ }
  return chunkKey(url, versions[path], k);
};

// A host that answers a ranged request with the whole file (200: no Range
// support, or a CDN that drops the header) is read once: that one answer is
// kept as it streams in, and every later range of the URL is cut from it as
// its bytes arrive, never fetched again (a whole download a read was the
// rest bundle again and again). Said once per URL.
const wholes = new Map();
class Whole {
  constructor(res) {
    this.buf = new Uint8Array(Number(res.headers.get('Content-Length')) || (1 << 20));
    this.have = 0;
    this.done = false;
    this.err = null;
    this.waits = [];
    this.finished = this.pump(res.body.getReader());
  }
  async pump(rd) {
    try {
      for (;;) {
        const { done, value } = await rd.read();
        if (done) break;
        if (this.have + value.length > this.buf.length) {
          const nb = new Uint8Array(Math.max(this.buf.length * 2, this.have + value.length));
          nb.set(this.buf.subarray(0, this.have));
          this.buf = nb;
        }
        this.buf.set(value, this.have);
        this.have += value.length;
        net += value.length;
        this.wake();
      }
      this.done = true;
    } catch (e) {
      this.err = e ?? new Error('cut short');
    }
    this.wake();
    return this.done;
  }
  wake() {
    this.waits = this.waits.filter((w) => (this.have >= w.hi || this.done || this.err ? (w.r(), false) : true));
  }
  /** Bytes [lo, hi) once they are here; short at the file's end. */
  async slice(lo, hi) {
    if (this.have < hi && !this.done && !this.err) await new Promise((r) => this.waits.push({ hi, r }));
    if (this.have >= hi || this.done) return this.buf.slice(lo, Math.min(hi, this.have));
    throw this.err;
  }
}
/** Bytes [lo, hi) of `url` from the network (or its kept whole answer):
 *  null if the host gave nothing usable. */
async function fetchRange(url, lo, hi) {
  let w = wholes.get(url);
  if (!w) {
    const res = await fetch(url, { headers: { Range: `bytes=${lo}-${hi - 1}` }, cache: 'no-store' });
    if (res.status === 206) {
      const b = new Uint8Array(await res.arrayBuffer());
      net += b.length;
      return b;
    }
    if (res.status !== 200 || !res.body) {
      res.body?.cancel().catch(() => {});
      return null;
    }
    w = wholes.get(url);
    // Another read's whole answer is on its way already: this one is let go.
    if (w) res.body.cancel().catch(() => {});
    else {
      w = new Whole(res);
      wholes.set(url, w);
      console.warn(`[lazy-cache] ${url}: the host ignores Range; its whole answer is kept and read from`);
    }
  }
  try {
    return await w.slice(lo, hi);
  } catch {
    if (wholes.get(url) === w) wholes.delete(url);
    return null;
  }
}

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
    // Tried a few times with waits between, then held as failed by chunk
    // (as a rest bundle's frame): reads of it fail at once until the hold ends.
    for (let c = k; c <= stop; c++) if (failed.held(`${url}#c${c}`)) throw heldError;
    const got = await retryAsync(() => fetchRange(url, lo, hi + 1));
    if (!got) {
      for (let c = k; c <= stop; c++) failed.failed(`${url}#c${c}`);
      throw new Error(`${url} @${lo}: no answer`);
    }
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

// The rest bundles: URL -> { frame, frames } from BUNDLE.json, as a promise.
const bundles = new Map();
// Decoded frames, `URL#k` -> promise of the bytes, most recently used last.
const decoded = new Map();
const FRAMES_KEPT = 4;
// Fetched frames as they came, compressed, `URL#k` -> bytes, most recently
// used last, up to PACKED_BUDGET bytes: a decoded frame that was let go is
// decoded again from here, not fetched again. Only the last few frames are
// kept decoded (8 MiB each), and a program's reads move between frames, so
// a launch fetched the same frames over and over (Notepad++ in the Wine
// desktop page: 33 fetches of 17 frames, about 43 MB for 24).
const packed = new Map();
const PACKED_BUDGET = 96 << 20;
let packedBytes = 0;
function keepPacked(key, z) {
  if (z.length > PACKED_BUDGET) return;
  packed.set(key, z);
  packedBytes += z.length;
  for (const [k, v] of packed) {
    if (packedBytes <= PACKED_BUDGET) break;
    packed.delete(k);
    packedBytes -= v.length;
  }
}
function packedFrame(key) {
  const z = packed.get(key);
  if (z) {
    packed.delete(key);
    packed.set(key, z);
  }
  return z;
}
let zstdUrl = null, zstd = null;
async function decoder() {
  // `true`: the one bundled with this Worker (the runtime package's).
  zstd ??= (zstdUrl === true ? import('../vendor/zstddec/zstddec-stream.mjs') : import(zstdUrl))
    .then(async ({ ZSTDDecoder }) => { const d = new ZSTDDecoder(); await d.init(); return d; });
  return zstd;
}
// The bundles' index and frames given up (lazy-cache.mjs FailedFrames):
// a read of one fails at once until its hold ends, and quietly (the give-up
// said so once).
const failed = new FailedFrames();
const heldError = new Error('given up a moment ago');
async function getRange(url, lo, hi) {
  const body = await retryAsync(async () => {
    // Never the HTTP cache's copy: a range of an older bundle under the same
    // URL would decode as garbage. A bundle with a hash keeps its frames in
    // the page's own cache (`frameOf`) instead.
    const b = await fetchRange(url, lo, hi);
    return b && b.length === hi - lo ? b : null;
  });
  if (!body) throw new Error(`${url} @${lo}: no answer`);
  return body;
}
// A rest bundle whose index names its bytes' hash (BUNDLE.json `h`,
// kde/mksite.sh) keeps its compressed frames in the page's Cache Storage
// under the bundle's URL with that hash and the frame's number, so a second
// visit reads them from there whatever caching headers the host sends, and a
// new bundle (another hash) is never read from an old one's frames, which go
// once it is seen. Without a hash, or a cache, frames come from the network.
let store_ = null;
// What a relative URL is relative to: the Worker's own URL, or, for one a
// page started from a blob (its copy of this file), the URL it stands for.
// (none in a realm without a location, a test's: absolute URLs only)
const here = () => globalThis.GEX_BASE ?? self.location?.href;
const frameKey = (url, b, k) => {
  const u = new URL(url, here());
  u.hash = '';
  u.search = `h=${b.h}&f=${k}`;
  return u.href;
};
const kept_ = (n) => { kept += n; };
async function stored(url, b, k) {
  if (!store_ || !b.h) return null;
  try {
    const r = await store_.match(frameKey(url, b, k));
    return r?.ok ? new Uint8Array(await r.arrayBuffer()) : null;
  } catch { return null; }
}
function store(url, b, k, bytes) {
  if (store_ && b.h) store_.put(frameKey(url, b, k), new Response(bytes)).catch(() => {});
}
/** Frames of `url` under another hash than `h`: an earlier build's. */
async function dropOld(url, h) {
  if (!store_) return;
  try {
    const path = new URL(url, here()).pathname;
    for (const r of await store_.keys()) {
      const u = new URL(r.url);
      if (u.pathname === path && u.searchParams.has('f') && u.searchParams.get('h') !== h) await store_.delete(r);
    }
  } catch { /* kept */ }
}

// A bundle read whole (a host that ignores Range): once all of it is here,
// every frame is kept in the page's cache, so a later visit reads none of it
// from the network again.
const storedWhole = new Set();
function storeWhole(url, b) {
  const w = wholes.get(url);
  if (!w || !b.h || storedWhole.has(w)) return;
  storedWhole.add(w);
  w.finished.then((ok) => {
    if (ok) b.frames.forEach(([o, n], k) => store(url, b, k, w.buf.slice(o, o + n)));
  });
}

// One fetch a frame however many readers want it: they share its promise
// (`decoded`) while it is in flight.
function frameOf(url, b, k) {
  const key = `${url}#${k}`;
  let f = decoded.get(key);
  if (f) {
    decoded.delete(key);
  } else {
    if (failed.held(key)) return Promise.reject(heldError);
    const [o, n] = b.frames[k];
    const kept = packedFrame(key);
    const z = kept ? Promise.resolve(kept) : stored(url, b, k).then((have) => {
      if (have) {
        kept_(have.length);
        keepPacked(key, have);
        return have;
      }
      return getRange(url, o, o + n).then((got) => {
        keepPacked(key, got);
        store(url, b, k, got);
        storeWhole(url, b);
        return got;
      });
    });
    f = Promise.all([z, decoder()]).then(([zb, d]) => d.decode(zb, k === b.frames.length - 1 ? 0 : b.frame));
    f.catch(() => { decoded.delete(key); failed.failed(key); });
  }
  decoded.set(key, f);
  while (decoded.size > FRAMES_KEPT) decoded.delete(decoded.keys().next().value);
  return f;
}
/** `len` bytes at `from` of the file at `href` (`BUNDLE#OFFSET`). */
async function bundleRead(href, from, len) {
  const cut = href.lastIndexOf('#');
  const url = href.slice(0, cut), start = Number(href.slice(cut + 1)) + from;
  let b = bundles.get(url);
  if (!b) {
    const key = `${url}.json`;
    if (failed.held(key)) throw heldError;
    b = retryAsync(async () => {
      // The index of this deploy's bundle, never an older cached one.
      const r = await fetch(key, { cache: 'no-cache' });
      return r.ok ? r.json() : null;
    }).then((j) => j ?? Promise.reject(new Error(`${key}: no answer`)));
    b.then((j) => { if (j.h) dropOld(url, j.h); }, () => {});
    bundles.set(url, b);
    b.catch(() => { bundles.delete(url); failed.failed(key); });
  }
  b = await b;
  const first = Math.floor(start / b.frame), last = Math.floor((start + len - 1) / b.frame);
  const out = new Uint8Array(len);
  for (let k = first, at = start; k <= last; k++) {
    const f = await frameOf(url, b, k);
    const a = at - k * b.frame, n = Math.min(f.length - a, start + len - at);
    if (n <= 0) throw new Error(`${href} @${from}: short`);
    out.set(f.subarray(a, a + n), at - start);
    at += n;
  }
  return out;
}

self.onmessage = async ({ data: m }) => {
  const ctl = new Int32Array(m.ctl);
  const raw = new Uint8Array(m.ctl);
  const memory = m.memory;
  versions = m.versions ?? {};
  zstdUrl = m.zstd ?? null;
  let cache = null;
  try {
    cache = await caches.open(m.cache);
    // A store that opens but cannot hold anything (some insecure origins).
    const base = here();
    const probe = base ? new URL('/__gex-probe', base).href : '/__gex-probe';
    await cache.put(probe, new Response('1'));
    await cache.delete(probe);
  } catch (e) {
    self.postMessage({ kind: 'ready', ok: false, why: String(e?.message ?? e) });
    return;
  }
  if (typeof Atomics.waitAsync !== 'function') {
    self.postMessage({ kind: 'ready', ok: false, why: 'no Atomics.waitAsync' });
    return;
  }
  store_ = cache;
  self.postMessage({ kind: 'ready', ok: true });
  const busy = new Set();
  const serve = async (i) => {
    const base = (HEAD + i * SLOT) / 4;
    // Taken: the reader waits for this answer now, not its own fetch.
    if (Atomics.compareExchange(ctl, base, ASKED, TAKEN) !== ASKED) {
      if (Atomics.load(ctl, base) === GIVEN_UP) Atomics.store(ctl, base, FREE);
      busy.delete(i);
      return;
    }
    const len = ctl[base + 3] >>> 0, dst = ctl[base + 2] >>> 0, ul = ctl[base + 4];
    const from = new Float64Array(m.ctl, HEAD + i * SLOT + 24, 1)[0];
    const url = new TextDecoder().decode(raw.slice(HEAD + i * SLOT + 32, HEAD + i * SLOT + 32 + ul));
    let bytes = null;
    try {
      bytes = url.includes('#') && zstdUrl ? await bundleRead(url, from, len) : await read(cache, url, from, len);
    } catch (e) {
      if (e !== heldError) console.warn(`[lazy-cache] ${e?.message ?? e}`);
    }
    // (A reader no longer gives up once its ask is taken; should one ever,
    // nothing is written into its buffer, which may be gone.)
    if (Atomics.compareExchange(ctl, base, TAKEN, WRITING) !== TAKEN) {
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
