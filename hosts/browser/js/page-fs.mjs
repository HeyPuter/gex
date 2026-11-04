// The global kernel's `env.page_fs` (hosts/kernel/src/pagefs.rs): a call on a
// directory the page serves (page/runtime `fs.mount`), handed to the page and
// waited for. The page's filesystem answers asynchronously and the guest's
// call is synchronous, so the Worker making it fills a slot in a
// SharedArrayBuffer, wakes the page and waits; the page (page/runtime/src/
// mount.js) answers into the slot. Only the calling thread waits.
//
// `ctl`, an Int32Array on a SharedArrayBuffer of HEAD + SLOTS * SLOT bytes.
// ctl[0] counts requests (the page waits on it); ctl[1] counts slots freed
// (a caller with none free waits on it). A slot, at HEAD + i * SLOT:
//   i32 [0] state: FREE, CLAIMED (the caller writes), ASKED (the page's to
//       take), TAKEN (the page answers), DONE (the caller copies, frees)
//   i32 [1] result: a count, a handle or a negated errno
//   i32 [2] the request's length, [3] the answer's
//   bytes DATA..: the request, then the answer in its place

export const SLOTS = 8;
export const HEAD = 64;
export const DATA = 16;
/** pagefs.rs CHUNK, and room for the request's header and two paths. */
export const SLOT = DATA + (128 << 10) + (16 << 10);
export const FREE = 0, CLAIMED = 1, ASKED = 2, TAKEN = 3, DONE = 4;
const EIO = 5;

/** The control buffer the page makes and every Worker gets. */
export function makePageFsControl() {
  return new SharedArrayBuffer(HEAD + SLOTS * SLOT);
}

let ctl = null;
let raw = null;
/** A Worker's side: `c` is `{ ctl: SharedArrayBuffer }`. */
export function setPageFs(c) {
  if (!c?.ctl || ctl) return;
  ctl = new Int32Array(c.ctl);
  raw = new Uint8Array(c.ctl);
}

function claim() {
  for (;;) {
    const freed = Atomics.load(ctl, 1);
    for (let i = 0; i < SLOTS; i++) {
      if (Atomics.compareExchange(ctl, (HEAD + i * SLOT) / 4, FREE, CLAIMED) === FREE) return i;
    }
    Atomics.wait(ctl, 1, freed, 1000);
  }
}

/** `page_fs(req, reqLen, out, cap)` against the kernel memory `memory`. */
export function pageFs(memory) {
  return (req, reqLen, out, cap) => {
    req >>>= 0; reqLen >>>= 0; out >>>= 0; cap >>>= 0;
    // No page filesystem here (a check's kernel; a page that never
    // mounted), or a thread that cannot wait (the page's own).
    if (!ctl || globalThis.document || reqLen > SLOT - DATA) return -EIO;
    let s;
    try { s = claim(); } catch { return -EIO; }
    const base = (HEAD + s * SLOT) / 4;
    const at = HEAD + s * SLOT + DATA;
    raw.set(new Uint8Array(memory.buffer, req, reqLen), at);
    ctl[base + 1] = -EIO;
    ctl[base + 2] = reqLen;
    ctl[base + 3] = 0;
    Atomics.store(ctl, base, ASKED);
    Atomics.add(ctl, 0, 1);
    Atomics.notify(ctl, 0);
    for (;;) {
      const st = Atomics.load(ctl, base);
      if (st === DONE) break;
      Atomics.wait(ctl, base, st, 5000);
    }
    const r = ctl[base + 1];
    const n = Math.min(ctl[base + 3] >>> 0, cap);
    if (n > 0) new Uint8Array(memory.buffer, out, n).set(raw.subarray(at, at + n));
    Atomics.store(ctl, base, FREE);
    Atomics.add(ctl, 1, 1);
    Atomics.notify(ctl, 1);
    return r;
  };
}

// The single-thread build: no control buffer and no thread to wait in. The
// system Worker hands a request to the page and the calling fiber's stack
// suspends until the page answers (`WebAssembly.Suspending`).
let ask = null;
/** `fn(req, cap)` -> Promise<[result, bytes]>: the page's answer. */
export function setPageFsAsk(fn) {
  ask = fn;
}

/** `page_fs` for the single-thread build, against the kernel memory `memory`. */
export function pageFsSuspending(memory) {
  return new WebAssembly.Suspending(async (req, reqLen, out, cap) => {
    req >>>= 0; reqLen >>>= 0; out >>>= 0; cap >>>= 0;
    if (!ask) return -EIO;
    const [r, bytes] = await ask(new Uint8Array(memory.buffer, req, reqLen).slice(), cap);
    // The memory may have grown while this stack was suspended.
    if (bytes?.length) new Uint8Array(memory.buffer, out, Math.min(bytes.length, cap)).set(bytes.subarray(0, cap));
    return r;
  });
}
