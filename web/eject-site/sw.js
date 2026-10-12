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

/** FILE's bytes as a cached Response: from the cache, or FILE.zst decoded. */
async function packed(path, entry, build, onBytes = null) {
  const cache = await caches.open(cacheName(build));
  const key = `${scope}${path}?v=${build}`;
  const hit = await cache.match(key);
  if (hit) {
    onBytes?.(entry.zsize);
    return hit;
  }
  if (inflight.has(key)) return (await inflight.get(key)).clone();
  const p = (async () => {
    const res = await fetch(`${scope}${path}.zst?v=${build}`);
    if (!res.ok) throw new Error(`${path}.zst: ${res.status}`);
    // Decoded as it streams, a network chunk at a time: the decoder's wasm
    // memory is module-wide and never shrinks, so a one-shot decode of the
    // mspaint bundle (459 MiB from 40 MiB) left a 498 MiB heap resident for
    // the service worker's life, beside the page's own copy of the bundle.
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
    let out = new Uint8Array(entry.size);
    let n = 0;
    try {
      for (const part of d.decodeStreaming(parts)) {
        if (n + part.length > out.length) throw new Error('longer than packed');
        out.set(part, n);
        n += part.length;
      }
    } catch (e) {
      // The one-shot path, as before, if the stream would not finish.
      console.warn(`[sw] ${path}: streaming decode failed (${e?.message ?? e}); decoding at once`);
      const z = new Uint8Array(entry.zsize);
      let o = 0;
      for (const p of parts) { z.set(p, o); o += p.length; }
      out = d.decode(z, entry.size);
      n = out.length;
    }
    if (n !== entry.size) throw new Error(`${path}: decoded ${n} of ${entry.size} bytes`);
    const ext = path.split('.').pop();
    const response = new Response(out, {
      headers: { 'Content-Type': TYPES[ext] ?? 'application/octet-stream', 'Content-Length': String(entry.size) },
    });
    await cache.put(key, response.clone());
    return response;
  })();
  inflight.set(key, p);
  try {
    return (await p).clone();
  } finally {
    inflight.delete(key);
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
        await packed(path, entry, p.build, tick);
      }
    };
    await Promise.all([worker(), worker(), worker(), worker()]);
    port.postMessage({ got: total, total, done: true });
  })().catch((e) => port.postMessage({ error: String(e?.message ?? e) }));
});
