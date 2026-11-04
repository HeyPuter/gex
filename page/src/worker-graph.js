// The guest Worker's module graph, fetched once by the page and kept in its
// Cache Storage, every Worker started from blob URLs of it.
//
// Every guest process is a Worker, and every Worker loads its module and the
// chunks that module imports (worker-*.js, the bridges, log, single...:
// ~170 KB in 7 requests). With an HTTP cache that costs one download; on an
// origin with no HTTP cache (the demo servers' self-signed cert: Chromium
// keeps none for a certificate error) it was all of it again for each of
// the ~110 Workers of a desktop shell's start (16.6 MB, ~600 requests), and a
// round trip more for every process the guest started later.
//
// So the page fetches each module once (from the cache when a visit before
// kept it: a bundler's chunk is named by its content), rewrites its imports
// of the others (`"./name-HASH.js"`, static and dynamic) to their blob URLs,
// and starts each Worker from the entry's blob URL. A module's
// `import.meta.url` (a file beside it: `new URL('./x.wasm', import.meta.url)`)
// is written as the module's real URL. Any other `import.meta`, or an import
// cycle, leaves the graph as it was: Workers then load by URL, as before. A relative URL a Worker resolves at
// run time (a lazy file's path) is resolved against `GEX_BASE`, the
// entry's real URL, which a prelude sets first (a blob URL is no base).

const CACHE = 'gex-modules-v1';
const SPEC = /(["'`])(\.\/[A-Za-z0-9_.-]+\.m?js)\1/g;
// Every static and dynamic import's specifier.
const IMPORT = /(?:\bfrom\s*|\bimport\s*\(?\s*)(["'`])([^"'`\s]+)\1/g;
// A chunk named by its content (vite's `name-HASH.js`): kept; anything else
// is fetched each time.
const hashed = (u) => /-[A-Za-z0-9_-]{8}\.m?js$/.test(new URL(u).pathname);

/** The URL `make` (a function doing `new Worker(URL, ...)`, the shape a
 *  bundler rewrites) would start its Worker from, without starting one. */
export function workerUrlOf(make) {
  const Real = globalThis.Worker;
  let got = null;
  globalThis.Worker = function Capture(u) { got = String(u); };
  try { make(); } catch { got = null; } finally { globalThis.Worker = Real; }
  return got;
}

/** The blob URL of `entry`'s graph, rewritten as above, or null (then the
 *  Workers load by URL). `onBytes(net, cached)` counts what came from where. */
export async function workerGraph(entry, onBytes = () => {}) {
  let cache = null;
  try { cache = typeof caches === 'object' && caches ? await caches.open(CACHE) : null; } catch { cache = null; }
  const text = async (u) => {
    if (cache && hashed(u)) {
      try {
        const r = await cache.match(u);
        if (r?.ok) {
          const t = await r.text();
          onBytes(0, t.length);
          return t;
        }
      } catch { /* fetched */ }
    }
    const r = await fetch(u);
    if (!r.ok) throw new Error(`${u}: ${r.status}`);
    const t = await r.text();
    onBytes(t.length, 0);
    if (cache && hashed(u)) cache.put(u, new Response(t, { headers: { 'Content-Type': 'text/javascript' } })).catch(() => {});
    return t;
  };
  const blobs = new Map(); // URL -> blob URL
  const busy = new Set();
  const made = [];
  const visit = async (u) => {
    if (blobs.has(u)) return blobs.get(u);
    if (busy.has(u)) throw new Error(`an import cycle at ${u}`);
    busy.add(u);
    let code = await text(u);
    // A name, not the URL in place: in a string literal the text changes,
    // never the syntax.
    if (/\bimport\.meta\.url\b/.test(code)) code = `const __gexModuleUrl = ${JSON.stringify(u)};\n${code.replace(/\bimport\.meta\.url\b/g, '__gexModuleUrl')}`;
    if (/\bimport\.meta\b/.test(code)) throw new Error(`${u} reads import.meta`);
    // An import by any other name (a dev server's "/node_modules/..." or
    // "/@fs/...") would not resolve from a blob URL.
    const other = [...code.matchAll(IMPORT)].map((m) => m[2]).find((s) => !s.startsWith('./'));
    if (other) throw new Error(`${u} imports ${other}`);
    const deps = [...new Set([...code.matchAll(SPEC)].map((m) => new URL(m[2], u).href))];
    // One at a time: a shared dependency is fetched once.
    for (const d of deps) await visit(d);
    code = code.replace(SPEC, (m, q, s) => `${q}${blobs.get(new URL(s, u).href)}${q}`);
    const b = URL.createObjectURL(new Blob([code], { type: 'text/javascript' }));
    made.push(b);
    blobs.set(u, b);
    busy.delete(u);
    return b;
  };
  try {
    const prelude = URL.createObjectURL(new Blob([`globalThis.GEX_BASE = ${JSON.stringify(entry)};\n`], { type: 'text/javascript' }));
    made.push(prelude);
    const root = await visit(entry);
    // The entry again, the prelude imported first (evaluated before any of
    // the entry's other imports).
    const top = URL.createObjectURL(new Blob([`import ${JSON.stringify(prelude)};\nimport ${JSON.stringify(root)};\n`], { type: 'text/javascript' }));
    made.push(top);
    // What an older page kept and this one does not name.
    if (cache) {
      const keep = new Set(blobs.keys());
      cache.keys().then((ks) => Promise.all(ks.filter((k) => !keep.has(k.url)).map((k) => cache.delete(k)))).catch(() => {});
    }
    return top;
  } catch (e) {
    for (const b of made) URL.revokeObjectURL(b);
    console.warn(`[gex] Workers load by URL: ${e?.message ?? e}`);
    return null;
  }
}
