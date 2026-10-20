// The persist worker (see persist.js): copies what the program changed under
// a directory of the global kernel's filesystem into IndexedDB. A Worker, not
// the page, because the kernel's locks may wait (memory.atomic.wait), which
// the main thread may not.
//
// Message in:  { spec: { module, memory }, root, db }   once, then
//              { sync: id }                            per snapshot.
// Message out: { synced: id, wrote, removed, files, bytes } or { error }.

import { openDb, txDone } from './persist-db.js';
import { pick } from './persist-pick.js';
import { lazyFetch, processMemories } from '../../shims/browser/js/lazy-fetch.mjs';

const SCRATCH = 0x1000; // where paths and buffers go in this worker's process memory
const CHUNK = 1 << 20;

let k = null;
let mem = null;
let root = '';
let dbName = '';
let db = null;
// What IndexedDB holds: path -> "size:hash". Each snapshot rereads the files
// the kernel says the program wrote (not by mtime: a write need not move it,
// and the program can set it) and stores the ones that changed.
let known = null;

function bytes() {
  return new Uint8Array(mem.buffer);
}

function ensure(n) {
  const need = Math.ceil((SCRATCH + n) / 65536) - mem.buffer.byteLength / 65536;
  if (need > 0) mem.grow(need);
}

function putPath(p) {
  const b = new TextEncoder().encode(p);
  ensure(b.length);
  bytes().set(b, SCRATCH);
  return b.length;
}

function meta(path) {
  const n = putPath(path);
  const at = SCRATCH + ((n + 15) & ~15);
  ensure(at - SCRATCH + 64);
  if (k.k_fs_path_meta(BigInt(SCRATCH), n, BigInt(at)) < 0) return null;
  const v = new DataView(mem.buffer, at, 36);
  return {
    dir: v.getUint32(0, true) === 1,
    file: v.getUint32(0, true) === 0,
    size: Number(v.getBigUint64(8, true)),
    mtime: Number(v.getBigInt64(24, true)),
  };
}

function open(path) {
  const n = putPath(path);
  return k.k_fs_open(BigInt(SCRATCH), n, 0n, 0);
}

function list(path) {
  const h = open(path);
  if (h < 0) return [];
  try {
    for (let cap = 1 << 16; ; cap *= 4) {
      ensure(cap);
      const n = k.k_fs_read_dir(h, BigInt(SCRATCH), cap);
      if (n === -34) continue;
      if (n < 0) return [];
      const out = [];
      const b = bytes().slice(SCRATCH, SCRATCH + n);
      const v = new DataView(b.buffer);
      for (let at = 0; at + 16 <= n;) {
        const len = v.getUint32(at, true);
        const type = v.getUint32(at + 4, true);
        const name = new TextDecoder().decode(b.subarray(at + 16, at + 16 + len));
        out.push({ name, type });
        at += 16 + len;
      }
      return out;
    }
  } finally {
    k.k_fs_close(h);
  }
}

function read(path, size) {
  const h = open(path);
  if (h < 0) return null;
  try {
    const out = new Uint8Array(size);
    ensure(CHUNK);
    let off = 0;
    while (off < size) {
      const n = k.k_fs_read(h, BigInt(off), BigInt(SCRATCH), Math.min(CHUNK, size - off));
      if (n < 0) return null;
      if (n === 0) break;
      out.set(bytes().subarray(SCRATCH, SCRATCH + n), off);
      off += n;
    }
    return off === size ? out : out.slice(0, off);
  } finally {
    k.k_fs_close(h);
  }
}

/** FNV-1a over 32-bit words (and the tail bytes): a change detector, not a checksum. */
function hash(d) {
  let h = 0x811c9dc5;
  const words = new Uint32Array(d.buffer, d.byteOffset, d.byteOffset % 4 ? 0 : d.length >>> 2);
  for (let i = 0; i < words.length; i++) h = Math.imul(h ^ words[i], 0x01000193);
  for (let i = words.length * 4; i < d.length; i++) h = Math.imul(h ^ d[i], 0x01000193);
  return (h >>> 0).toString(16);
}

/** Every regular file under `dir`, as path -> meta. */
function walk(dir, out) {
  for (const { name, type } of list(dir)) {
    if (name === '.' || name === '..') continue;
    const p = `${dir}/${name}`;
    if (type === 1) walk(p, out);
    else if (type === 0) {
      const m = meta(p);
      if (m?.file) out.set(p, m);
    }
  }
  return out;
}

/** The kernel's list of files the program has made or changed since the
 *  root was seeded (`k_fs_written`), or null from a kernel without it. */
function writtenPaths() {
  if (!k.k_fs_written) return null;
  for (let cap = 1 << 16; ; cap *= 4) {
    ensure(cap);
    const n = k.k_fs_written(BigInt(SCRATCH), cap);
    if (n === -34) continue;
    if (n < 0) return null;
    return new TextDecoder().decode(bytes().slice(SCRATCH, SCRATCH + n)).split('\n').filter(Boolean);
  }
}

/** What a snapshot looks at, as path -> meta (see `pick`). A walk of `root`
 *  only from a kernel without `k_fs_written`: it reads every file under the
 *  root, which for a root over the lazy part of a bundle (FL Studio's /pfx,
 *  3.3 GB) faults all of it in. */
function candidates() {
  const written = writtenPaths();
  return written ? pick(root, written, known.keys(), meta) : walk(root, new Map());
}

async function sync() {
  if (!known) {
    known = new Map();
    const tx = db.transaction('meta', 'readonly');
    const req = tx.objectStore('meta').openCursor();
    await new Promise((resolve, reject) => {
      req.onsuccess = () => {
        const c = req.result;
        if (!c) return resolve();
        known.set(c.key, c.value);
        c.continue();
      };
      req.onerror = () => reject(req.error);
    });
  }
  // Not before the bundle is unpacked into the kernel, and not when the
  // directory is not there: an empty walk would delete every stored file.
  if ((k.k_fs_populated && !k.k_fs_populated()) || !meta(root)?.dir) {
    return { wrote: 0, removed: 0, files: 0, bytes: 0, skipped: true };
  }
  const now = candidates();
  const writes = [];
  let bytesOut = 0;
  for (const [p, m] of now) {
    if (m.keep) continue;
    let data;
    try {
      data = read(p, m.size);
    } catch (e) {
      // A trap in the kernel leaves it unusable for this snapshot: say which file.
      throw new Error(`reading ${p} (${m.size} bytes): ${e?.message ?? e}`);
    }
    if (!data) continue;
    const stamp = `${data.length}:${hash(data)}`;
    if (known.get(p) === stamp) continue;
    writes.push([p, stamp, data]);
    bytesOut += data.length;
  }
  const removed = [...known.keys()].filter((p) => !now.has(p));
  if (writes.length || removed.length) {
    const tx = db.transaction(['files', 'meta'], 'readwrite');
    const files = tx.objectStore('files');
    const metas = tx.objectStore('meta');
    for (const [p, stamp, data] of writes) {
      files.put(data, p);
      metas.put(stamp, p);
    }
    for (const p of removed) {
      files.delete(p);
      metas.delete(p);
    }
    await txDone(tx);
    for (const [p, stamp] of writes) known.set(p, stamp);
    for (const p of removed) known.delete(p);
  }
  return { wrote: writes.length, removed: removed.length, files: now.size, bytes: bytesOut };
}

let queue = Promise.resolve();
self.onmessage = (e) => {
  const m = e.data;
  if (m.spec) {
    queue = queue.then(async () => {
      root = m.root.replace(/\/+$/, '');
      dbName = m.db;
      // The process memory as the kernel build declares it: shared (the
      // page's build) or not; a memory import's sharedness is part of its type.
      let inst;
      for (const shared of [true, false]) {
        mem = new WebAssembly.Memory({ initial: 1, maximum: 65535, shared });
        try {
          inst = await WebAssembly.instantiate(m.spec.module, { env: { memory: m.spec.memory, ...processMemories(mem), lazy_fetch: lazyFetch(m.spec.memory) } });
          break;
        } catch (e) {
          if (!shared) throw e;
        }
      }
      k = inst.exports;
      // The stack slot above every Worker's (PageTty's, which an ejected
      // program's page does not use: it has no terminal).
      if (k.k_stack_area && k.__stack_pointer) {
        k.__stack_pointer.value = (k.k_stack_area() + k.k_stack_count() * k.k_stack_size()) | 0;
      }
      db = await openDb(dbName);
    }).catch((err) => self.postMessage({ error: String(err?.stack ?? err) }));
    return;
  }
  if ('sync' in m) {
    queue = queue
      .then(() => sync())
      .then((r) => self.postMessage({ synced: m.sync, ...r }))
      .catch((err) => self.postMessage({ synced: m.sync, error: String(err?.stack ?? err) }));
  }
};
