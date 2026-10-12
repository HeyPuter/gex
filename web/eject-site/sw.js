// The service worker of an ejected program's static site (web/eject-site.mjs).
//
// 1. Cross-origin isolation. The guest runs threads on SharedArrayBuffer,
//    which needs COOP: same-origin + COEP: require-corp. A server that sends
//    them needs nothing else; for one that can't, every response from this
//    origin gets them here (index.html reloads once after installing this).
// 2. Compressed files. The site ships every large file as FILE.zst (pack.json
//    lists them). A request for FILE is answered from this build's cache, or
//    by fetching FILE.zst, decoding it here and caching the result, so the
//    page, its Workers (synchronous requests included) and Mesa all ask for
//    plain FILE and never see the compression. Any static host works: .zst
//    is served as bytes, with no Content-Encoding.
// 3. Prefetch. index.html asks for every packed file before the program
//    starts ({prefetch: true} on a MessageChannel; progress comes back), so
//    a first load is one progress bar and a reload starts from the cache.

import { ZSTDDecoder } from './vendor/zstddec-stream.mjs';

const scope = new URL(self.registration.scope).pathname.replace(/\/$/, '');
let packP = null;
const pack = () => (packP ??= fetch(`${scope}/pack.json`, { cache: 'no-cache' }).then((r) => r.json()).catch((e) => {
  packP = null;
  throw e;
}));
let decoderP = null;
const decoder = () => (decoderP ??= (async () => {
  const d = new ZSTDDecoder();
  await d.init();
  return d;
})());
const cacheName = (build) => `gex-eject-${build}`;
const inflight = new Map();

self.addEventListener('install', () => self.skipWaiting());
self.addEventListener('activate', (event) => event.waitUntil((async () => {
  await self.clients.claim();
  // Caches of other builds go.
  try {
    const p = await pack();
    for (const k of await caches.keys()) if (k.startsWith('gex-eject-') && k !== cacheName(p.build)) await caches.delete(k);
  } catch {}
})()));

function isolate(response) {
  if (response.status === 0) return response; // opaque: headers can't be set
  const headers = new Headers(response.headers);
  headers.set('Cross-Origin-Opener-Policy', 'same-origin');
  headers.set('Cross-Origin-Embedder-Policy', 'require-corp');
  headers.set('Cross-Origin-Resource-Policy', 'same-origin');
  return new Response(response.body, { status: response.status, statusText: response.statusText, headers });
}

const TYPES = { wasm: 'application/wasm', js: 'text/javascript', mjs: 'text/javascript', json: 'application/json', tar: 'application/x-tar', html: 'text/html' };

/** FILE's bytes as a Response: from the cache, or FILE.zst decoded as it
 *  streams (and stored in the cache as it goes). With `serve` false (the
 *  launcher's prefetch), only stored: null once it is in the cache.
 *
 *  Never held whole here. A decoded copy kept by this worker (as a
 *  Response's body, a buffer one was made from, or a clone nobody read)
 *  stays resident in the page's renderer for the page's whole life, beside
 *  the page's own copy: 459 MiB for the mspaint bundle. */
async function packed(path, entry, build, onBytes = null, serve = true) {
  const cache = await caches.open(cacheName(build));
  const key = `${scope}${path}?v=${build}`;
  const fromCache = async () => {
    const hit = await cache.match(key);
    if (!hit) throw new Error(`${path}: not in the cache after it was stored`);
    return serve ? hit : null;
  };
  const hit = await cache.match(key);
  if (hit) {
    onBytes?.(entry.zsize);
    return serve ? hit : null;
  }
  if (inflight.has(key)) {
    await inflight.get(key);
    return fromCache();
  }
  let stored;
  const job = new Promise((r) => { stored = r; });
  // Waiters only need it settled; they read the cache (and fail there).
  inflight.set(key, job);
  let teed = false;
  try {
    const res = await fetch(`${scope}${path}.zst?v=${build}`);
    if (!res.ok) throw new Error(`${path}.zst: ${res.status}`);
    // The compressed file is read whole first (40 MiB for mspaint's 459):
    // the decoder's wasm memory is module-wide and never shrinks, so it is
    // fed a network chunk at a time rather than given one buffer.
    const parts = [];
    let at = 0;
    const reader = res.body.getReader();
    for (;;) {
      const r = await reader.read();
      if (r.done) break;
      parts.push(r.value);
      at += r.value.length;
      onBytes?.(r.value.length);
    }
    if (at !== entry.zsize) throw new Error(`${path}.zst: ${at} of ${entry.zsize} bytes`);
    const d = await decoder();
    const gen = d.decodeStreaming(parts);
    const CHUNK = 4 << 20;
    let n = 0;
    let oneShot = null;
    const pull = (c) => {
      if (oneShot) {
        const end = Math.min(oneShot.length, n + CHUNK);
        if (end > n) c.enqueue(oneShot.slice(n, end));
        n = end;
      } else {
        // Coalesced a few MiB at a time: the decoder gives 128 KiB.
        const got = [];
        let len = 0;
        while (len < CHUNK) {
          const r = gen.next();
          if (r.done) break;
          got.push(r.value);
          len += r.value.length;
        }
        if (len) {
          if (n + len > entry.size) throw new Error('longer than packed');
          let buf = got[0];
          if (got.length > 1) {
            buf = new Uint8Array(len);
            let o = 0;
            for (const g of got) { buf.set(g, o); o += g.length; }
          }
          c.enqueue(buf);
          n += len;
          return;
        }
      }
      if (n === entry.size) {
        parts.length = 0;
        oneShot = null;
        c.close();
      } else if (!oneShot) {
        throw new Error(`decoded ${n} of ${entry.size} bytes`);
      }
    };
    const body = new ReadableStream({
      pull(c) {
        try {
          pull(c);
        } catch (e) {
          if (oneShot) { c.error(e); return; }
          // The one-shot path, as before, if the stream would not finish:
          // the rest from a whole decode.
          console.warn(`[sw] ${path}: streaming decode failed (${e?.message ?? e}); decoding at once`);
          const z = new Uint8Array(entry.zsize);
          let o = 0;
          for (const q of parts) { z.set(q, o); o += q.length; }
          const whole = d.decode(z, entry.size);
          if (whole.length !== entry.size) { c.error(new Error(`${path}: decoded ${whole.length} of ${entry.size} bytes`)); return; }
          oneShot = whole;
          try { pull(c); } catch (e2) { c.error(e2); }
        }
      },
    }, { highWaterMark: 0 });
    const ext = path.split('.').pop();
    const headers = { 'Content-Type': TYPES[ext] ?? 'application/octet-stream', 'Content-Length': String(entry.size) };
    if (!serve) {
      await cache.put(key, new Response(body, { headers }));
      return null;
    }
    // Teed: what one side has not read yet is all that is held.
    const [toCache, toPage] = body.tee();
    teed = true;
    cache.put(key, new Response(toCache, { headers }))
      .catch((e) => console.warn(`[sw] ${path}: not cached (${e?.message ?? e})`))
      .finally(() => stored());
    return new Response(toPage, { headers });
  } finally {
    // Without a tee in flight, settled now (stored or failed).
    if (!teed) stored();
    job.then(() => { if (inflight.get(key) === job) inflight.delete(key); });
  }
}

self.addEventListener('fetch', (event) => {
  const request = event.request;
  if (request.cache === 'only-if-cached' && request.mode !== 'same-origin') return;
  const url = new URL(request.url);
  if (request.method === 'GET' && url.origin === self.location.origin && url.pathname.startsWith(scope)) {
    const path = url.pathname.slice(scope.length);
    event.respondWith((async () => {
      try {
        const p = await pack();
        const entry = p.files[path];
        if (entry) return isolate(await packed(path, entry, p.build));
      } catch (e) {
        console.error(`[sw] ${path}: ${e?.message ?? e}`);
        return new Response(`service worker: ${e?.message ?? e}`, { status: 500 });
      }
      return isolate(await fetch(request));
    })());
    return;
  }
  event.respondWith(fetch(request).then(isolate));
});

self.addEventListener('message', (event) => {
  const port = event.ports[0];
  if (!event.data?.prefetch || !port) return;
  (async () => {
    const p = await pack();
    const list = Object.entries(p.files);
    const total = list.reduce((a, [, e]) => a + e.zsize, 0);
    let got = 0;
    let last = 0;
    const tick = (n) => {
      got += n;
      const now = Date.now();
      if (now - last > 100) {
        last = now;
        port.postMessage({ got, total });
      }
    };
    // A few at a time, largest first: the big unit overlaps the rest.
    list.sort((a, b) => b[1].zsize - a[1].zsize);
    let i = 0;
    const worker = async () => {
      while (i < list.length) {
        const [path, entry] = list[i++];
        await packed(path, entry, p.build, tick, false);
      }
    };
    await Promise.all([worker(), worker(), worker(), worker()]);
    port.postMessage({ got: total, total, done: true });
  })().catch((e) => port.postMessage({ error: String(e?.message ?? e) }));
});
