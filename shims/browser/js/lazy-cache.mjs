// Lazy files kept in the page's Cache Storage (app.js `lazyCache`). A lazy
// file's read happens in a Worker blocked in the kernel (lazy-fetch.mjs),
// which cannot wait on a promise; the cache is asynchronous. So the read is
// asked of one Worker that owns the cache (web/src/lazy-cache-worker.js)
// through slots in a SharedArrayBuffer: the reader fills a slot, wakes the
// owner and waits; the owner answers from the cache, or fetches what the
// cache lacks (a ranged request, as the reader would have made) and keeps
// it, and writes the bytes straight into the kernel's memory, where the
// reader wanted them. Kept by chunk (CHUNK, the kernel's LAZY_CHUNK), under
// the file's URL, its version and the chunk's number (chunkKey), so a later
// visit, after any rebuild, has every chunk any visit read of the same
// bytes. An eject site's lazy file URL is its bytes' hash (eject-site.mjs);
// other lists name files by path, and the version -- the list's size and
// mtime -- is what tells a file changed under the same URL from the one
// kept.
//
// The slots: `ctl`, an Int32Array on a SharedArrayBuffer of SLOTS * SLOT
// bytes after a header of HEAD bytes. ctl[0] counts requests (the owner
// waits on it). A slot, i32 at SLOT/4 * i + HEAD/4:
//   [0] state: FREE, ASKED (the reader waits), TAKEN (the owner reads the
//       cache or fetches; the reader keeps waiting), WRITING (the owner
//       copies), DONE, GIVEN_UP (no owner took the ask in time and the
//       reader went to the network itself: the owner must not write)
//   [1] result: bytes written, or -1
//   [2] dst, [3] len, [4] url length, [5..6] offset (f64 at byte 24)
//   bytes 32..: the URL (a path on the page's origin), at most URL_MAX

export const CHUNK = 1 << 20;
export const SLOTS = 32;
export const HEAD = 64;
export const SLOT = 1056;
export const URL_MAX = SLOT - 32;
export const FREE = 0, ASKED = 1, WRITING = 2, DONE = 3, GIVEN_UP = 4, TAKEN = 5;

/** A lazy file's version from its list entry: size, and mtime when the
 *  list has one. */
export function lazyVersion(size, mtime) {
  return mtime === undefined ? `${size}` : `${size}.${mtime}`;
}

/** The cache key of chunk `k` of `url` (absolute) at `version` (none: the
 *  URL alone names the bytes). */
export function chunkKey(url, version, k) {
  return version ? `${url}?v=${encodeURIComponent(version)}&c=${k}` : `${url}?c=${k}`;
}

/** The control buffer the page makes and hands to the owner and to every
 *  Worker. */
export function makeControl() {
  return new SharedArrayBuffer(HEAD + SLOTS * SLOT);
}

let ctl = null;
let raw = null;
/** Whether this Worker has an owner to ask (`setLazyCache`). */
export function lazyCacheOn() {
  return ctl !== null;
}

/** A Worker's side: `c` is `{ ctl: SharedArrayBuffer }`. */
export function setLazyCache(c) {
  if (!c?.ctl) return;
  ctl = new Int32Array(c.ctl);
  raw = new Uint8Array(c.ctl);
}

// How long a reader waits for the owner to take its ask before it asks the
// network itself (an owner that is gone). Once taken, it waits for the
// owner's answer however long the fetch takes: the owner gives -1 when its
// fetch fails, and the reader's own request would be the same bytes again on
// the same link (a 22 MB lazy DLL fetched twice at a first visit at 0.7 MB/s,
// when the owner's fetch outlasted the old 60 s bound on the whole wait).
const WAIT_MS = 60000;

/** Read `len` bytes of `href` at `from` into the kernel memory at `dst`,
 *  through the owner. Returns the bytes written, or -1 when it could not
 *  (no owner, no free slot, a failure): the caller then fetches. */
export function cachedRead(href, from, dst, len) {
  if (!ctl) return -1;
  const url = new TextEncoder().encode(href);
  if (url.length > URL_MAX) return -1;
  let s = -1;
  for (let i = 0; i < SLOTS; i++) {
    if (Atomics.compareExchange(ctl, (HEAD + i * SLOT) / 4, FREE, ASKED + 100) === FREE) { s = i; break; }
  }
  if (s < 0) return -1;
  const base = (HEAD + s * SLOT) / 4;
  ctl[base + 1] = -1;
  ctl[base + 2] = dst;
  ctl[base + 3] = len;
  ctl[base + 4] = url.length;
  new Float64Array(ctl.buffer, HEAD + s * SLOT + 24, 1)[0] = from;
  raw.set(url, HEAD + s * SLOT + 32);
  // Claimed (ASKED + 100) while being written; asked now.
  Atomics.store(ctl, base, ASKED);
  Atomics.add(ctl, 0, 1);
  Atomics.notify(ctl, 0);
  const t0 = Date.now();
  for (;;) {
    const st = Atomics.load(ctl, base);
    if (st === DONE) break;
    if (st === ASKED && Date.now() - t0 > WAIT_MS) {
      if (Atomics.compareExchange(ctl, base, ASKED, GIVEN_UP) === ASKED) return -1;
      continue;
    }
    Atomics.wait(ctl, base, st, st === ASKED ? 5000 : 1000);
  }
  const n = ctl[base + 1];
  Atomics.store(ctl, base, FREE);
  return n;
}
