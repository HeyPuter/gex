// The background fill's Worker (app.js `startFill`): downloads the fill, a
// packed (zstd) file of lazy files, and decodes it straight into the fill's
// SharedArrayBuffer, off the page's main thread, which pid 1's start-up and
// the compositor need. Every Worker reads the fill through `counter` (how
// many leading bytes are whole); the page hears only what it acts on.
//
// In: { url, zsize, size, buf, counter, zstd, late: [[name, off, size]...], demand: [KEY...],
//       cache: the Cache Storage name (app.js PIECE_CACHE) when the fill is a
//       content-addressed piece (eject-site.mjs --pieces), else absent }
// Out:
//   { kind: 'late', entry }  a late unit whole: [`cgen:KEY`, Module] (compiled
//                            here) or [`file:REL`, Uint8Array on a SAB] (its
//                            other files, and a `demand` unit's c.wasm, which
//                            the first process to map it compiles)
//   { kind: 'bytes', n, cached }  compressed bytes since the last such
//                            message, from the cache or the network
//   { kind: 'done' } | { kind: 'failed', error }

self.onmessage = async ({ data: m }) => {
  const counter = new Int32Array(m.counter);
  const out = new Uint8Array(m.buf);
  let n = 0;
  let at = 0;
  let li = 0;
  let unsent = 0;
  let lastPost = 0;
  const late = m.late;
  const demand = new Set(m.demand ?? []);
  // A late unit whose bytes are all in: compiled (or copied) here, handed
  // to the page.
  const takeLate = () => {
    for (; li < late.length && late[li][1] + late[li][2] <= n; li++) {
      const [name, off, size] = late[li];
      const rel = name.slice('unit:'.length);
      if (rel.endsWith('/c.wasm') && !demand.has(rel.slice(0, -'/c.wasm'.length))) {
        WebAssembly.compile(out.slice(off, off + size)).then(
          (mod) => postMessage({ kind: 'late', entry: [`cgen:${rel.slice(0, -'/c.wasm'.length)}`, mod] }),
          (e) => postMessage({ kind: 'late', error: `${rel}: ${e}` }),
        );
      } else {
        const s = new Uint8Array(new SharedArrayBuffer(size));
        s.set(out.subarray(off, off + size));
        postMessage({ kind: 'late', entry: [`file:${rel}`, s] });
      }
    }
  };
  try {
    const { ZSTDDecoder } = await import(/* @vite-ignore */ m.zstd);
    const d = new ZSTDDecoder();
    await d.init();
    // A content-addressed fill is kept in the page's cache, as its pieces
    // are: a later visit decodes it from there, not the network.
    const key = (() => { const u = new URL(m.url); u.search = ''; return u.href; })();
    let cache = null;
    try { cache = m.cache && typeof caches === 'object' && caches ? await caches.open(m.cache) : null; } catch { cache = null; }
    let res = null;
    try { res = cache ? await cache.match(key) : null; } catch { res = null; }
    const cached = !!res?.ok;
    if (!cached) {
      res = await fetch(m.url);
      if (res.ok && cache) cache.put(key, res.clone()).catch(() => {});
    }
    if (!res.ok) throw new Error(`${m.url}: ${res.status} ${res.statusText}`);
    const emit = (part) => {
      if (n + part.length > m.size) throw new Error(`${m.url}: longer than its ${m.size} bytes`);
      out.set(part, n);
      n += part.length;
    };
    const z = d.stream();
    const reader = res.body.getReader();
    try {
      for (;;) {
        const { done, value } = await reader.read();
        if (done) break;
        z.push(value, emit);
        Atomics.store(counter, 0, n);
        Atomics.notify(counter, 0);
        takeLate();
        at += value.length;
        unsent += value.length;
        const now = performance.now();
        if (now - lastPost >= 100) {
          postMessage({ kind: 'bytes', n: unsent, cached });
          unsent = 0;
          lastPost = now;
        }
      }
    } finally {
      z.end();
    }
    if (unsent) postMessage({ kind: 'bytes', n: unsent, cached });
    if (at !== m.zsize) throw new Error(`${m.url}: ${at} of ${m.zsize} bytes`);
    if (n !== m.size) throw new Error(`${m.url}: decoded ${n} of ${m.size} bytes`);
    postMessage({ kind: 'done' });
  } catch (e) {
    Atomics.store(counter, 0, -1);
    Atomics.notify(counter, 0);
    postMessage({ kind: 'failed', error: String(e?.message ?? e) });
  }
};
