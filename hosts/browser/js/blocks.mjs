// The host's half of the kernel's block store (gex_kernel::vfs::blocks,
// hosts/kernel `block_io`): the one store file of an `OPFSFilesystem`, in
// the origin private file system.
//
// A realm holding the store's handle (`blocksOpen`: the single-thread
// build's system Worker) reads and writes it here, in the kernel's memory.
// Any other asks the page's store Worker (page/src/blocks-worker.js) through
// the slots in the kernel's memory (`BLOCKS_CTL`) and waits. A page's own
// store (a `Filesystem` subclass of its own) is served from the page, the
// slots by `serveSlots` (the default build only).
//
// A sync access handle's read or write can fail without throwing: it
// returns an error code as an unsigned count (no space near the origin's
// quota while `estimate()` still reports room; a handle gone dead). Every
// count is checked here, and anything but the whole request is a failure.

export const OP_READ = 1, OP_WRITE = 2, OP_GROW = 4, OP_FLUSH = 8;
/** A full store, to the kernel (its ENOSPC); any other failure is -1 (EIO). */
export const NOSPACE = -28;
// A sync access handle's count when the origin is out of quota (-8, unsigned).
const HANDLE_NOSPACE = 0xfffffff8;
// The slots (Int32 indices into BLOCKS_CTL): [0] requests asked (the store
// waits on it), [1] the store is up; slot i at SLOT0 + i * SLOTW:
// [0] state, [1] op, [2] dst, [3] len, [4] result, [10..11] offset.
export const SLOT0 = 16, SLOTW = 16, SLOTS = 32;
export const FREE = 0, ASKED = 1, DONE = 2, CLAIMED = 9;

/** The store's directory and file in the origin private file system. */
export const storeDir = (name) => `gex-fs-${name}`;
export const STORE_FILE = 'blocks';

let own = null; // { h: handle, shared: reads straight into shared memory }

/** Open this realm's own handle on the store `name` (the single-thread
 *  build's system Worker), with what a load before kept in it. */
export async function blocksOpen(name) {
  if (own) return true;
  const root = await navigator.storage.getDirectory();
  const dir = await root.getDirectoryHandle(storeDir(name), { create: true });
  const fh = await dir.getFileHandle(STORE_FILE, { create: true });
  const h = await openHandle(fh);
  own = { h, shared: sharedReads(h) };
  return true;
}

// The stores this page holds (`claimStore`): name -> { n, held, release },
// or the promise of one while its lock is asked for.
const claims = new Map();

/** Hold the store `name` for this page: a Web Lock named for it, held until
 *  every claim is released (or the page goes away), so that two pages of
 *  one origin never open one store (the second one's sync access handle
 *  would fail). Claims in one page share the lock. `wait`: until another
 *  page lets it go (`signal` stops the wait, rejecting with its reason);
 *  otherwise null at once if another page holds it. A browser without Web
 *  Locks: a claim that holds nothing (the handle is then the only guard). */
export async function claimStore(name, { wait = false, signal } = {}) {
  const locks = globalThis.navigator?.locks;
  let c = claims.get(name);
  if (c instanceof Promise) c = await c.catch(() => null);
  if (!c || !claims.has(name)) {
    if (!locks) {
      c = { n: 0, release() {} };
    } else {
      const asked = new Promise((granted, failed) => {
        let release;
        const held = new Promise((r) => { release = r; });
        locks.request(`gex-fs-${name}`, wait ? { signal } : { ifAvailable: true }, (lock) => {
          granted(lock ? { n: 0, release } : null);
          return lock ? held : undefined;
        }).catch(failed);
      });
      claims.set(name, asked);
      try {
        c = await asked;
      } catch (e) {
        if (claims.get(name) === asked) claims.delete(name);
        throw e;
      }
      if (!c) {
        if (claims.get(name) === asked) claims.delete(name);
        return null;
      }
    }
    claims.set(name, c);
  }
  c.n++;
  let done = false;
  return {
    release() {
      if (done) return;
      done = true;
      if (--c.n === 0 && claims.get(name) === c) {
        claims.delete(name);
        c.release();
      }
    },
  };
}

/** A store held by another page: its error (code EBUSY). */
export const storeBusy = (name) => Object.assign(new Error(`OPFSFilesystem: the store ${name} is open in another tab or window of this site; close it, or keep the files in a MemoryFilesystem`), { code: 'EBUSY' });

/** A sync access handle on `fh`; a store Worker of a system just stopped
 *  (or of a page just closed) may still hold it a moment after it was
 *  terminated, so a busy handle is tried again for 10 s. */
export async function openHandle(fh) {
  for (let i = 0; ; i++) {
    try {
      return await fh.createSyncAccessHandle();
    } catch (e) {
      if (i >= 100) throw e;
      await new Promise((r) => setTimeout(r, 100));
    }
  }
}

/** Whether `h` reads and writes shared memory directly. */
export function sharedReads(h) {
  if (typeof SharedArrayBuffer !== 'function') return true;
  try {
    h.read(new Uint8Array(new SharedArrayBuffer(8)), { at: 0 });
    return true;
  } catch {
    return false;
  }
}

const zero = new Uint8Array(1);
let bounce = null;
function bounceOf(n) {
  if (!bounce || bounce.length < n) bounce = new Uint8Array(Math.max(n, 1 << 20));
  return bounce.subarray(0, n);
}

let verifyWrites = false;
/** Debugging: every write read straight back and compared. */
export function blocksVerify(on) { verifyWrites = !!on; }

/** One op on handle `h` against `memory`'s `len` bytes at `dst`: `len` (0
 *  for a grow or a flush), NOSPACE, or -1 (the kernel's EIO). */
export function handleOp(h, sharedOk, memory, op, off, dst, len) {
  const isShared = typeof SharedArrayBuffer === 'function' && memory.buffer instanceof SharedArrayBuffer;
  const view = new Uint8Array(memory.buffer, dst, len);
  const direct = !isShared || sharedOk;
  if (op === OP_READ) {
    const b = direct ? view : bounceOf(len);
    const n = h.read(b, { at: off });
    // An error code (as unsigned) is larger than the length; a short count
    // is the file's end (blocks grown ahead, never written: zeros).
    if (n > len) return -1;
    if (!direct) view.set(b.subarray(0, n));
    if (n < len) view.fill(0, n);
    return len;
  }
  if (op === OP_GROW) {
    // `off` is the length wanted, made by writing its last byte, not by
    // truncate: in a context whose storage is in memory (a private window,
    // a test's fresh context) a truncate to between 1 and 2 GiB ends the
    // browser, and one past 2 GiB does nothing; a write there past about
    // 2 GiB is refused (no space), which is the store's ENOSPC.
    if (h.getSize() >= off) return 0;
    const n = h.write(zero, { at: off - 1 });
    if (n !== 1) return (n >>> 0) === HANDLE_NOSPACE ? NOSPACE : -1;
    return h.getSize() >= off ? 0 : NOSPACE;
  }
  if (op === OP_FLUSH) {
    h.flush();
    return 0;
  }
  if (op === OP_WRITE) {
    let b = view;
    if (!direct) {
      b = bounceOf(len);
      b.set(view);
    }
    const n = h.write(b, { at: off });
    if (n !== len) return (n >>> 0) === HANDLE_NOSPACE ? NOSPACE : -1;
    if (verifyWrites) {
      const back = new Uint8Array(len);
      const m = h.read(back, { at: off });
      const now = new Uint8Array(memory.buffer, dst, len);
      let at = -1;
      for (let i = 0; i < len; i++) if (back[i] !== now[i]) { at = i; break; }
      if (m !== len || at >= 0) console.error(`[blocks] write read back wrong at ${off}+${len}: got ${m}, first difference at ${at}`);
    }
    return len;
  }
  return -1;
}

let waitCell = null;
/** Ask the store Worker through the kernel's slots at `ctl`. */
function ask(memory, ctl, op, off, dst, len) {
  const c = new Int32Array(memory.buffer, ctl, 1024);
  if (!Atomics.load(c, 1)) return -1;
  let s = -1;
  for (;;) {
    for (let i = 0; i < SLOTS; i++) {
      if (Atomics.compareExchange(c, SLOT0 + i * SLOTW, FREE, CLAIMED) === FREE) { s = SLOT0 + i * SLOTW; break; }
    }
    if (s >= 0) break;
    Atomics.wait(waitCell ??= new Int32Array(new SharedArrayBuffer(4)), 0, 0, 1);
  }
  c[s + 1] = op; c[s + 2] = dst; c[s + 3] = len; c[s + 4] = -1;
  c[s + 10] = off % 0x100000000; c[s + 11] = Math.floor(off / 0x100000000);
  Atomics.store(c, s, ASKED);
  Atomics.add(c, 0, 1);
  Atomics.notify(c, 0);
  for (;;) {
    const st = Atomics.load(c, s);
    if (st === DONE) break;
    Atomics.wait(c, s, st, 1000);
  }
  const r = c[s + 4];
  Atomics.store(c, s, FREE);
  return r;
}

/** The kernel's `block_io` import over `memory` (the kernel's). */
export function blockIo(memory) {
  return (op, ctl, offset, dst, len) => {
    dst >>>= 0; len >>>= 0; ctl >>>= 0;
    const off = Number(offset);
    try {
      return own ? handleOp(own.h, own.shared, memory, op, off, dst, len) : ask(memory, ctl, op, off, dst, len);
    } catch (e) {
      console.error(`[blocks] op ${op} at ${off}+${len}: ${e?.message ?? e}`);
      return -1;
    }
  };
}

/** One request on a page's store `fs` (a Filesystem's `read`, `write`,
 *  `size`, `truncate`): `[len or 0, bytes read]`, or `[-1]`. */
export async function storeOp(fs, op, off, len, bytes) {
  try {
    if (op === OP_READ) {
      const data = await fs.read(off, len);
      if (!(data instanceof Uint8Array) || data.length > len) throw new TypeError(`read(${off}, ${len}) gave ${data?.length} bytes`);
      return [len, data];
    }
    if (op === OP_WRITE) {
      await fs.write(off, bytes);
      return [len, null];
    }
    if (op === OP_GROW) {
      // A page's store grows by its own truncate (Filesystem's contract,
      // where it says ENOSPC), not as handleOp grows the OPFS file.
      if (await fs.size() < off) await fs.truncate(off);
      return [0, null];
    }
    if (op === OP_FLUSH) {
      await fs.flush();
      return [0, null];
    }
  } catch (e) {
    if (e?.code === 'ENOSPC') return [NOSPACE, null];
    console.error(`[blocks] the store's ${{ 1: 'read', 2: 'write', 4: 'truncate', 8: 'flush' }[op]} at ${off}+${len}: ${e?.message ?? e}`);
  }
  return [-1, null];
}

/** Serve the slots at `ctl` in the kernel's `memory` from the page's store
 *  `fs`, one request at a time, until the returned function is called. */
export function serveSlots(memory, ctl, fs) {
  let stopped = false;
  const c = () => new Int32Array(memory.buffer, ctl, 1024);
  Atomics.store(c(), 1, 1);
  (async () => {
    while (!stopped) {
      const seen = Atomics.load(c(), 0);
      for (let i = 0; i < SLOTS && !stopped; i++) {
        const s = SLOT0 + i * SLOTW;
        let v = c();
        if (Atomics.load(v, s) !== ASKED) continue;
        const op = v[s + 1], dst = v[s + 2] >>> 0, len = v[s + 3] >>> 0;
        const off = (v[s + 10] >>> 0) + (v[s + 11] >>> 0) * 0x100000000;
        const bytes = op === OP_WRITE ? new Uint8Array(memory.buffer, dst, len).slice() : null;
        const [r, data] = await storeOp(fs, op, off, len, bytes);
        if (r >= 0 && op === OP_READ) {
          const view = new Uint8Array(memory.buffer, dst, len);
          view.set(data);
          if (data.length < len) view.fill(0, data.length);
        }
        v = c();
        v[s + 4] = r;
        Atomics.store(v, s, DONE);
        Atomics.notify(v, s);
      }
      if (stopped) break;
      const w = Atomics.waitAsync(c(), 0, seen, 5000);
      if (w.async) await w.value;
    }
  })();
  return () => {
    stopped = true;
    Atomics.store(c(), 1, 0);
  };
}
