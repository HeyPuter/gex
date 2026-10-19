// A package overlay, written into the running system's filesystem.
//
// The demo desktop's launcher (web/src/desktop.js) adds a program by fetching
// `pkgs/NAME.tar.zst` -- the program's closure minus the base rootfs, built by
// web/build-demo.sh -- and writing its files straight into the global kernel's
// filesystem while the guest runs. That is this Worker: an instance of the
// global kernel of its own (as `persist-worker.js` is), calling `k_fs_*`
// with the bytes in a scratch "process" memory. A Worker and not the page
// because the kernel's locks may wait, and the page's thread cannot.
//
// The tar's `var/lib/gex-overlay/NAME.status` holds the dpkg status stanzas
// of what it adds; they are appended to /var/lib/dpkg/status, so a later
// `apt` or `dpkg -l` sees the program as installed.
//
// Message in: { spec: { module, memory }, stackTop, url, name }
// Messages out: { progress, got, total }, { done, files, bytes, ms }, { error }

import { lazyFetch, processMemories } from '../../shims/browser/js/lazy-fetch.mjs';
import { untar } from './tar.js';
import { isZstd, unzstd } from './zfetch.js';

const enc = new TextEncoder();
let k = null;
let mem = null;
let view = null;

async function kernel(spec, stackTop) {
  if (k) return;
  let inst;
  for (const shared of [true, false]) {
    mem = new WebAssembly.Memory({ initial: 16, maximum: 65535, shared });
    try {
      inst = await WebAssembly.instantiate(spec.module, {
        env: { memory: spec.memory, ...processMemories(mem), lazy_fetch: lazyFetch(spec.memory) },
      });
      break;
    } catch (e) {
      if (!shared) throw e;
    }
  }
  k = inst.exports;
  if (stackTop && k.__stack_pointer) k.__stack_pointer.value = stackTop | 0;
}

function bytes(need) {
  const have = mem.buffer.byteLength;
  if (need > have) mem.grow(Math.ceil((need - have) / 65536));
  if (!view || view.buffer !== mem.buffer || view.length !== mem.buffer.byteLength) {
    view = new Uint8Array(mem.buffer);
  }
  return view;
}

/** Two strings (or a string and bytes) laid out at 0 and after it. */
function put(a, b) {
  const pa = enc.encode(a);
  const pb = typeof b === 'string' ? enc.encode(b) : b;
  const at = (pa.length + 15) & ~15;
  const v = bytes(at + (pb?.length ?? 0) + 16);
  v.set(pa, 0);
  if (pb) v.set(pb, at);
  return [0, pa.length, at, pb?.length ?? 0];
}

function readFile(path) {
  const [pp, pl] = put(path);
  const h = k.k_fs_open(BigInt(pp), pl, 0n, 0);
  if (h < 0) return null;
  const parts = [];
  let off = 0;
  const at = 1 << 16;
  for (;;) {
    bytes(at + (1 << 20));
    const n = k.k_fs_read(h, BigInt(off), BigInt(at), 1 << 20);
    if (n <= 0) break;
    parts.push(new Uint8Array(mem.buffer).slice(at, at + n));
    off += n;
  }
  k.k_fs_close(h);
  const out = new Uint8Array(off);
  let o = 0;
  for (const p of parts) {
    out.set(p, o);
    o += p.length;
  }
  return out;
}

function addFile(path, data) {
  const [pp, pl, dp, dl] = put(path, data);
  k.k_fs_unlink(BigInt(pp), pl); // a symlink in the way would be written through
  k.k_fs_add(BigInt(pp), pl, BigInt(dp), dl);
}

async function overlay({ url, name }) {
  const t0 = performance.now();
  const res = await fetch(url);
  if (!res.ok) throw new Error(`${url}: ${res.status}`);
  const total = Number(res.headers.get('content-length')) || 0;
  const chunks = [];
  let got = 0;
  const reader = res.body.getReader();
  let last = 0;
  for (;;) {
    const { done, value } = await reader.read();
    if (done) break;
    chunks.push(value);
    got += value.length;
    if (performance.now() - last > 100) {
      last = performance.now();
      self.postMessage({ progress: name, got, total });
    }
  }
  let z = new Uint8Array(got);
  let o = 0;
  for (const c of chunks) {
    z.set(c, o);
    o += c.length;
  }
  const tFetched = performance.now();
  if (!isZstd(z)) throw new Error(`${url}: not a zstd file`);
  const raw = await unzstd(z);
  z = null;
  const tree = untar(raw.buffer.slice(raw.byteOffset, raw.byteOffset + raw.byteLength));
  const tDecoded = performance.now();

  let n = 0;
  let size = 0;
  for (const d of tree.dirs) {
    const [pp, pl] = put(d);
    k.k_fs_mkdir(BigInt(pp), pl, 0o755);
  }
  let status = '';
  // Files fetched from the page as the guest reads them (`k_fs_add_lazy`):
  // [guest path, size, same-origin path]. The apt overlay's own .debs.
  const lazy = [];
  for (const [p, data] of Object.entries(tree.files)) {
    if (p.startsWith('/var/lib/gex-overlay/') && p.endsWith('.status')) {
      status += new TextDecoder().decode(data);
    }
    if (p.startsWith('/var/lib/gex-overlay/') && p.endsWith('.lazy')) {
      lazy.push(...JSON.parse(new TextDecoder().decode(data)));
    }
    addFile(p, data);
    if (tree.modes[p] !== undefined) {
      const [pp, pl] = put(p);
      k.k_fs_chmod(BigInt(pp), pl, tree.modes[p]);
    }
    n++;
    size += data.length;
  }
  for (const [p, target] of Object.entries(tree.hard)) {
    const [tp, tl, pp, pl] = put(target, p);
    k.k_fs_unlink(BigInt(pp), pl);
    k.k_fs_link(BigInt(tp), tl, BigInt(pp), pl);
    n++;
  }
  for (const [p, target] of Object.entries(tree.links)) {
    const [tp, tl, pp, pl] = put(target, p);
    k.k_fs_unlink(BigInt(pp), pl);
    k.k_fs_symlink(BigInt(tp), tl, BigInt(pp), pl);
    n++;
  }
  for (const [p, size, url] of lazy) {
    const [pp, pl, up, ul] = put(p, url);
    k.k_fs_add_lazy(BigInt(pp), pl, BigInt(up), ul, BigInt(size));
    n++;
  }
  if (status) {
    const cur = readFile('/var/lib/dpkg/status') ?? new Uint8Array();
    const old = new TextDecoder().decode(cur);
    // Two overlays may carry the same package (a library both need): one
    // stanza per package and architecture, or dpkg refuses the database.
    const key = (st) => `${/^Package: (.*)$/m.exec(st)?.[1]}:${/^Architecture: (.*)$/m.exec(st)?.[1]}`;
    const have = new Set(old.split(/\n\n+/).filter((x) => x.trim()).map(key));
    const add = status.split(/\n\n+/).filter((x) => x.trim() && !have.has(key(x)));
    const text = old.replace(/\n*$/, '\n\n') + add.map((x) => `${x.replace(/\n+$/, '')}\n\n`).join('');
    addFile('/var/lib/dpkg/status', enc.encode(text));
  }
  k.k_wake?.();
  return {
    files: n,
    bytes: size,
    fetchMs: Math.round(tFetched - t0),
    decodeMs: Math.round(tDecoded - tFetched),
    writeMs: Math.round(performance.now() - tDecoded),
    ms: Math.round(performance.now() - t0),
    compressed: got,
  };
}

let queue = Promise.resolve();
self.onmessage = (e) => {
  const m = e.data;
  queue = queue
    .then(async () => {
      await kernel(m.spec, m.stackTop);
      const r = await overlay(m);
      self.postMessage({ done: m.name, ...r });
    })
    .catch((err) => self.postMessage({ error: String(err?.stack ?? err), name: m.name }));
};
