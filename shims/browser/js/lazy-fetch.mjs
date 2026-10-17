// The global kernel's `env.lazy_fetch` (shims/kernel: `TmpFs::add_lazy`):
// a lazy file's bytes, fetched when a guest reads them. A synchronous ranged
// request, made from the Worker whose guest is reading (the read blocks
// there anyway); it fills `len` bytes at `dst` in the kernel's memory.
// Where there is no XMLHttpRequest (Node), lazy files cannot be read.
import { noteFetch } from './fetch-note.mjs';
import { cachedRead } from './lazy-cache.mjs';

// The background fill (app.js `fill`): lazy files the page downloads, packed
// one after another in one stream, while the guest runs. `buf` (a
// SharedArrayBuffer) holds them as decoded so far, `at` (an Int32Array on a
// SharedArrayBuffer) how many bytes of `buf` are there (-1: the fill failed),
// `files` their URL paths and where each starts. A read of one of them waits
// for its bytes here instead of asking the server for them (a second download
// of the same bytes, beside the fill's, on the same link).
let fill = null;
export function setFill(f) {
  if (!f?.buf || !f?.at || !f?.files) return;
  fill = { buf: new Uint8Array(f.buf), at: f.at, files: new Map(f.files.map(([path, off, size]) => [path, [off, size]])) };
}
// Wait until the fill holds `end` bytes; false if it failed or stalled.
function fillHas(end) {
  for (;;) {
    const at = Atomics.load(fill.at, 0);
    if (at < 0) return false;
    if (at >= end) return true;
    // Workers may block; the page's thread never reads lazy files.
    if (Atomics.wait(fill.at, 0, at, 10000) === 'timed-out' && Atomics.load(fill.at, 0) === at) return false;
  }
}
/** A fill entry that is not a lazy file (`unit:KEY/c.wasm`, a late unit's
 *  file: eject-site.mjs --late), once its bytes are there; an unshared copy,
 *  or null when the fill has no such entry. */
export function fillFile(name) {
  const f = fill?.files.get(name);
  if (!f || !fillHas(f[0] + f[1])) return null;
  return fill.buf.slice(f[0], f[0] + f[1]);
}
function fromFill(href, from, memory, dst, len) {
  if (!fill) return -1;
  let path;
  try { path = decodeURIComponent(new URL(href, 'http://x').pathname); } catch { return -1; }
  const f = fill.files.get(path);
  if (!f || from + len > f[1]) return -1;
  const end = f[0] + from + len;
  if (!fillHas(end)) return -1;
  new Uint8Array(memory.buffer, dst, len).set(fill.buf.subarray(f[0] + from, end));
  return len;
}

/** `lazy_fetch(url, urlLen, offset, dst, len)` against the kernel memory `memory`. */
export function lazyFetch(memory) {
  return (url, urlLen, offset, dst, len) => {
    url >>>= 0; urlLen >>>= 0; dst >>>= 0; len >>>= 0;
    const from = Number(offset);
    const href = new TextDecoder().decode(new Uint8Array(memory.buffer, url, urlLen).slice());
    if (fill) {
      const n = fromFill(href, from, memory, dst, len);
      if (n >= 0) return n;
    }
    const done = noteFetch(href, len);
    // The page's cache of lazy files, when it keeps one (lazy-cache.mjs).
    if (cachedRead(href, from, dst, len) === len) {
      done(len);
      return len;
    }
    if (typeof XMLHttpRequest !== 'function') { done(0); return -1; }
    let last = 'no response';
    try {
    for (let attempt = 0; attempt < 3; attempt++) {
      try {
        const x = new XMLHttpRequest();
        x.open('GET', href, false);
        x.responseType = 'arraybuffer';
        x.setRequestHeader('Range', `bytes=${from}-${from + len - 1}`);
        // The HTTP cache can answer a range from a partial entry badly (a
        // later range of a file whose first chunk it holds); the guest keeps
        // its own copy of each chunk, so the cache buys nothing here.
        x.setRequestHeader('Cache-Control', 'no-store');
        x.send();
        const got = new Uint8Array(x.response ?? new ArrayBuffer(0));
        last = `status ${x.status}, ${got.length} bytes`;
        // 206 is the range; a server without ranges sends the whole file (200).
        const at = x.status === 206 ? 0 : x.status === 200 ? from : -1;
        if (at < 0 || got.length < at + len) continue;
        new Uint8Array(memory.buffer, dst, len).set(got.subarray(at, at + len));
        done(len);
        return len;
      } catch (e) {
        last = String(e);
      }
    }
    } finally {
      done(0);
    }
    console.error(`[lazy] ${href} @${from}+${len}: failed (${last})`);
    return -1;
  };
}
