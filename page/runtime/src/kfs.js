// The guest's files from the page: the kernel's `k_fs_*` exports through the
// kernel-call Worker (kernel.js), for the Filesystem (fs.js), the layers it
// lays into the root.

import { errno } from './kernel.js';

const enc = new TextEncoder();
const dec = new TextDecoder();
const ERANGE = -34;
// Linux open flags.
const O_WRONLY = 1, O_CREAT = 0o100, O_EXCL = 0o200, O_TRUNC = 0o1000, O_APPEND = 0o2000, O_DIRECTORY = 0o200000, O_NOFOLLOW = 0o400000;
const READ_MAX = 4 << 20;
/** A tar's or a file's bytes go to the kernel in parts of this. */
const PART = 16 << 20;
const META_BYTES = 72;
/** gex-kernel vfs::remote::xattr_op::SET. */
const XATTR_SET = 1;
const TYPES = ['file', 'directory', 'symlink', 'chardev', 'fifo', 'socket'];
/** The time the kernel stamps a change with (it has no clock): seconds. */
const nowSecs = () => BigInt(Math.trunc(Date.now() / 1000));
/** The most bytes one batch carries to the Worker. */
const BATCH_BYTES = 32 << 20;

/** Calls in batches of at most BATCH_BYTES of data; each call's result
 *  checked by `check(result, call)` when given. */
async function batched(k, list, check) {
  let part = [];
  let bytes = 0;
  const send = async () => {
    if (!part.length) return;
    const t0 = performance.now();
    const res = await k.calls(part);
    if (globalThis.__gexBootTrace) {
      const kinds = {};
      for (const c of part) kinds[c.f] = (kinds[c.f] ?? 0) + 1;
      console.warn(`[boot] batch ${part.length} calls, ${(bytes / 1048576).toFixed(1)} MiB, ${(performance.now() - t0).toFixed(0)} ms: ${JSON.stringify(kinds)}`);
    }
    if (check) res.forEach((r, i) => check(r.r, part[i]));
    part = [];
    bytes = 0;
  };
  for (const c of list) {
    const n = (c.a ?? []).reduce((s, x) => s + (x?.in?.length ?? 0), 0);
    if (bytes + n > BATCH_BYTES) await send();
    part.push(c);
    bytes += n;
  }
  await send();
}

/** The most bytes of one `k_fs_seed` call's records. */
const SEED_BYTES = 4 << 20;

/** Seeding records for the kernel's `k_fs_seed` (its comment has the
 *  format), pushed onto a call list as calls of at most SEED_BYTES each,
 *  in order with the calls around them. Without that export (an older
 *  kernel), each record is its own call, as before. */
class SeedRecords {
  #list;
  #seed;
  #buf = new Uint8Array(SEED_BYTES + 65536);
  #view = new DataView(this.#buf.buffer);
  #at = 0;

  constructor(list, seed) {
    this.#list = list;
    this.#seed = seed;
  }

  #room(n) {
    if (this.#at + n > SEED_BYTES) this.flush();
    if (n > this.#buf.length) {
      this.#buf = new Uint8Array(n);
      this.#view = new DataView(this.#buf.buffer);
    }
  }

  #u8(v) { this.#view.setUint8(this.#at, v); this.#at += 1; }
  #u32(v) { this.#view.setUint32(this.#at, v >>> 0, true); this.#at += 4; }
  #u64(v) { this.#view.setBigUint64(this.#at, BigInt.asUintN(64, BigInt(v)), true); this.#at += 8; }
  #text(b) { this.#u32(b.length); this.#buf.set(b, this.#at); this.#at += b.length; }

  lazy(p, url, size, mode) {
    if (!this.#seed) {
      this.#list.push({ f: 'k_fs_add_lazy', a: [{ in: p }, { in: url }, BigInt(size)] });
      if (mode !== undefined) this.#list.push({ f: 'k_fs_chmod', a: [{ in: p }, mode] });
      return;
    }
    const pb = enc.encode(p), ub = enc.encode(url);
    this.#room(25 + pb.length + ub.length);
    this.#u8(1); this.#u32(mode ?? 0xffffffff); this.#u64(size); this.#text(pb); this.#text(ub);
  }

  symlink(target, p) {
    if (!this.#seed) return void this.#list.push({ f: 'k_fs_symlink', a: [{ in: target }, { in: p }] });
    const tb = enc.encode(target), pb = enc.encode(p);
    this.#room(9 + tb.length + pb.length);
    this.#u8(2); this.#text(tb); this.#text(pb);
  }

  mtime(p, secs) {
    if (!this.#seed) return void this.#list.push({ f: 'k_fs_set_mtime', a: [{ in: p }, BigInt(Math.trunc(secs))] });
    const pb = enc.encode(p);
    this.#room(13 + pb.length);
    this.#u8(3); this.#u64(Math.trunc(secs)); this.#text(pb);
  }

  chmod(p, mode) {
    if (!this.#seed) return void this.#list.push({ f: 'k_fs_chmod', a: [{ in: p }, mode] });
    const pb = enc.encode(p);
    this.#room(9 + pb.length);
    this.#u8(4); this.#u32(mode); this.#text(pb);
  }

  /** The records so far, as one call on the list. */
  flush() {
    if (!this.#at) return;
    this.#list.push({ f: 'k_fs_seed', a: [{ in: this.#buf.slice(0, this.#at) }] });
    this.#at = 0;
  }
}

/** A file's calls: its bytes in parts of at most PART, and its mode when
 *  `modes` has one. */
function fileCalls(list, p, data, modes) {
  const d = typeof data === 'string' ? enc.encode(data) : data;
  list.push({ f: 'k_fs_add', a: [{ in: p }, { in: d.subarray(0, PART) }] });
  for (let off = PART; off < d.length; off += PART) list.push({ f: 'k_fs_add_part', a: [{ in: p }, BigInt(off), { in: d.subarray(off, off + PART) }] });
  if (modes[p] !== undefined) list.push({ f: 'k_fs_chmod', a: [{ in: p }, modes[p]] });
}

export class Files {
  #k;
  constructor(k) {
    this.#k = k;
  }

  /** A bundle still being laid while the guest runs (`#layEarly`): its
   *  end, or its failure (every file it did not bring is EIO by then). */
  background = null;

  /** `path` with its symlinks followed (the last too unless `last` is
   *  false), as the guest's own lookups follow them; `path` itself when the
   *  walk fails, for the call that follows to say why. */
  async real(path, last = true) {
    const { r, out: [b] } = await this.#k.call('k_fs_walk', { in: path }, { in: '' }, last ? 1 : 0, { out: 4096 });
    if (r < 0) return path;
    const out = dec.decode(b.subarray(0, r & ((1 << 30) - 1)));
    return out.startsWith('/') ? out : `/${out}`;
  }

  async open(path, flags = 0, mode = 0) {
    const h = await this.#k.r('k_fs_open', { in: await this.real(path, !(flags & O_NOFOLLOW)) }, BigInt(flags), mode);
    if (h < 0) throw errno(h, path);
    return h;
  }

  /** `lstat` (`follow` false: a symlink is itself) or `stat`. */
  async stat(path, follow = true) {
    const { r, out: [b] } = await this.#k.call('k_fs_path_meta', { in: await this.real(path, follow) }, { outPtr: META_BYTES });
    if (r < 0) throw errno(r, path);
    return meta(b);
  }

  async readFile(path) {
    const h = await this.open(path, 0);
    try {
      const parts = [];
      let total = 0;
      for (;;) {
        const { r, out: [b] } = await this.#k.call('k_fs_read', h, BigInt(total), { out: READ_MAX });
        if (r < 0) throw errno(r, path);
        if (r === 0) break;
        parts.push(b);
        total += r;
      }
      return concat(parts, total);
    } finally {
      await this.#k.r('k_fs_close', h);
    }
  }

  async writeFile(path, data, { mode = 0o644, append = false, exclusive = false } = {}) {
    const bytes = typeof data === 'string' ? enc.encode(data) : new Uint8Array(data);
    const flags = O_WRONLY | O_CREAT | (append ? O_APPEND : O_TRUNC) | (exclusive ? O_EXCL : 0);
    const h = await this.open(path, flags, mode);
    try {
      let off = append ? (await this.stat(path)).size : 0;
      const list = [];
      for (let done = 0; done < bytes.length; done += READ_MAX) {
        const piece = bytes.subarray(done, done + READ_MAX);
        list.push({ f: 'k_fs_write', a: [h, BigInt(off + done), { in: piece }] });
      }
      list.push({ f: 'k_fs_touch', a: [h, BigInt(Math.floor(Date.now() / 1000))] });
      await batched(this.#k, list, (r, c) => {
        if (c.f === 'k_fs_write' && r < 0) throw errno(r, path);
      });
    } finally {
      await this.#k.r('k_fs_close', h);
    }
  }

  async readdir(path) {
    const h = await this.open(path, O_DIRECTORY);
    try {
      for (let cap = 1 << 16; ; cap *= 4) {
        const { r, out: [b] } = await this.#k.call('k_fs_read_dir', h, { out: cap });
        if (r === ERANGE) continue;
        if (r < 0) throw errno(r, path);
        const v = new DataView(b.buffer, b.byteOffset, b.length);
        const out = [];
        for (let at = 0; at + 16 <= r;) {
          const len = v.getUint32(at, true);
          const name = dec.decode(b.subarray(at + 16, at + 16 + len));
          if (name !== '.' && name !== '..') out.push({ name, type: TYPES[v.getUint32(at + 4, true)] ?? 'file' });
          at += 16 + len;
        }
        return out;
      }
    } finally {
      await this.#k.r('k_fs_close', h);
    }
  }

  async mkdir(path, { recursive = false, mode = 0o755 } = {}) {
    // Dated now, as a program's mkdir is (the kernel has no clock).
    const now = nowSecs();
    if (!recursive) {
      const r = await this.#k.r('k_fs_mkdir_now', { in: path }, mode, now);
      if (r < 0) throw errno(r, path);
      return;
    }
    const list = [];
    let p = '';
    for (const part of path.split('/').filter(Boolean)) {
      p += `/${part}`;
      list.push({ f: 'k_fs_mkdir_now', a: [{ in: p }, mode, now] });
    }
    const res = await this.#k.calls(list);
    res.forEach(({ r }, i) => {
      if (r < 0 && r !== -17) throw errno(r, list[i].a[0].in);
    });
  }

  async rm(path, { recursive = false, force = false } = {}) {
    let st;
    try {
      st = await this.stat(path, false);
    } catch (e) {
      if (force && e.errno === 2) return;
      throw e;
    }
    if (st.type === 'directory') {
      if (recursive) for (const { name } of await this.readdir(path)) await this.rm(`${path.replace(/\/+$/, '')}/${name}`, { recursive, force });
      const r = await this.#k.r('k_fs_rmdir_now', { in: path }, nowSecs());
      if (r < 0) throw errno(r, path);
      return;
    }
    const r = await this.#k.r('k_fs_unlink_now', { in: path }, nowSecs());
    if (r < 0 && !(force && r === -2)) throw errno(r, path);
  }

  async rename(from, to) {
    const r = await this.#k.r('k_fs_rename_now', { in: from }, { in: to }, 0, nowSecs());
    if (r < 0) throw errno(r, `${from} -> ${to}`);
  }

  async symlink(target, path) {
    const r = await this.#k.r('k_fs_symlink', { in: target }, { in: path });
    if (r < 0) throw errno(r, path);
  }

  async readlink(path) {
    const { r, out: [b] } = await this.#k.call('k_fs_read_link', { in: path }, { out: 4096 });
    if (r < 0) throw errno(r, path);
    return dec.decode(b);
  }

  async chmod(path, mode) {
    const r = await this.#k.r('k_fs_chmod', { in: await this.real(path) }, mode);
    if (r < 0) throw errno(r, path);
  }

  /** A layer (fs.js) into the root, over whatever is there: a tar (its
   *  bytes, unpacked in the kernel as they come, pagetar.rs) laid at `at`,
   *  single files with their modes, lazy files (`[path, size, url, mode,
   *  mtime]`, mtime in seconds; either may be undefined).
   *  Seeding, not a program's writes: the store keeps none of it. The
   *  count of files laid. */
  async lay({ tar, at = '/', files = {}, modes = {}, lazy = [] }) {
    const k = this.#k;
    let n = 0;
    if (tar) {
      const id = await k.r('k_fs_tar_open', { in: at });
      const list = [];
      for (let off = 0; off < tar.length; off += PART) list.push({ f: 'k_fs_tar_feed', a: [id, { in: tar.subarray(off, off + PART) }] });
      await batched(k, list);
      n += await k.r('k_fs_tar_close', id);
    }
    const list = [];
    // Single files are the page's, made now: dated so, and their folders
    // with them (a folder seeded without a time takes its newest entry's).
    const now = BigInt(Math.trunc(Date.now() / 1000));
    for (const [p, data] of Object.entries(files)) {
      const d = typeof data === 'string' ? enc.encode(data) : data;
      // A large file in parts, so the Worker's memory never holds a whole one.
      list.push({ f: 'k_fs_add', a: [{ in: p }, { in: d.subarray(0, PART) }] });
      for (let off = PART; off < d.length; off += PART) list.push({ f: 'k_fs_add_part', a: [{ in: p }, BigInt(off), { in: d.subarray(off, off + PART) }] });
      if (modes[p] !== undefined) list.push({ f: 'k_fs_chmod', a: [{ in: p }, modes[p]] });
      list.push({ f: 'k_fs_set_mtime', a: [{ in: p }, now] });
      n++;
    }
    for (const [p, size, url, mode, mtime] of lazy) {
      list.push({ f: 'k_fs_unlink', a: [{ in: p }] }, { f: 'k_fs_add_lazy', a: [{ in: p }, { in: url }, BigInt(size)] });
      if (mode !== undefined) list.push({ f: 'k_fs_chmod', a: [{ in: p }, mode] });
      if (mtime !== undefined) list.push({ f: 'k_fs_set_mtime', a: [{ in: p }, BigInt(Math.trunc(mtime))] });
      n++;
    }
    await batched(k, list, (r, c) => {
      if ((c.f === 'k_fs_add' || c.f === 'k_fs_add_lazy') && r < 0) throw errno(r, c.a[0].in);
    });
    return n;
  }

  /** A program's image as a page's untarred bundle has it (page/src/tar.js
   *  `untar`; a preset's, page/src/run-program.js), in the order a program's
   *  own loading of it took: empty directories, files with their modes,
   *  directories' modes, lazy files (`[path, size, url, head]`; a bundled
   *  file wins over a lazy one unless it is that file's first `head` bytes),
   *  hard links, symbolic links, file times (seconds) and extended
   *  attributes. The count of files laid. */
  async layImage(l) {
    if (l.stream) return this.#layStream(l);
    const { files = {}, modes = {}, dirs = [] } = l;
    const list = [];
    for (const p of dirs) list.push({ f: 'k_fs_add_dir', a: [{ in: p }, 0o755] });
    let n = 0;
    for (const [p, data] of Object.entries(files)) {
      fileCalls(list, p, data, modes);
      n++;
    }
    return n + await this.#layRest(list, {
      ...l, has: (p) => files[p] !== undefined, size: (p) => files[p].length, bytes: async (p) => files[p],
    });
  }

  /** An image whose files arrive as they are unpacked (`stream`: an async
   *  iterable of tar.js `untar` parts, in the archive's order), laid a part
   *  at a time, each part's bytes free once laid; what needs every file in
   *  first (lazy files, hard and symbolic links, times, the directories'
   *  modes) after the last. `modes`, `mtimes` and `xattrs` beside it are the
   *  lazy files' (the parts' own win, as one untarred tree's would). Nothing
   *  runs in the guest until boot's layers are all in (system.js `#start`). */
  async #layStream(l) {
    const { stream, streamIndex, modes: lazyModes = {}, mtimes: lazyMtimes, xattrs: baseXattrs, lazy = [], onPart } = l;
    const k = this.#k;
    if (streamIndex && k.has('k_fs_fill')) {
      const index = await streamIndex;
      if (index) return this.#layEarly(l, index);
    }
    const sizes = new Map();
    const dirs = [];
    const modes = { ...lazyModes };
    const symlinks = {};
    const hard = {};
    const mtimes = {};
    const xattrs = { ...(baseXattrs ?? {}) };
    let n = 0;
    for await (const t of stream) {
      const list = [];
      for (const p of t.dirs) {
        list.push({ f: 'k_fs_add_dir', a: [{ in: p }, 0o755] });
        dirs.push(p);
      }
      for (const [p, data] of Object.entries(t.files)) {
        fileCalls(list, p, data, t.modes);
        sizes.set(p, data.length);
        n++;
      }
      Object.assign(modes, t.modes);
      Object.assign(symlinks, t.links);
      Object.assign(hard, t.hard);
      Object.assign(mtimes, t.mtimes);
      Object.assign(xattrs, t.xattrs);
      await batched(k, list, (r, c) => {
        if (c.f === 'k_fs_add' && r < 0) throw errno(r, c.a[0].in);
      });
      onPart?.(t);
    }
    // Hard links whose target is a file of the image, as the page's own
    // untarred tree keeps them (app.js).
    for (const [p, target] of Object.entries(hard)) if (!sizes.has(target)) delete hard[p];
    return n + await this.#layRest([], {
      dirs, modes, lazy, hard, symlinks, mtimes: lazyMtimes ? { ...lazyMtimes, ...mtimes } : undefined, xattrs,
      has: (p) => sizes.has(p), size: (p) => sizes.get(p),
      // the bytes of a link the kernel would not make, from the copy laid
      bytes: (p) => this.readFile(p),
    });
  }

  /** A streamed image whose archive starts with its index (app.js
   *  `streamBundle`, kde/bundles.py): every directory, link, mode and time
   *  of the archive, and each file as pending (its size; hosts/kernel
   *  `k_fs_seed`), laid at once with the lazy files, and the guest may start:
   *  a use of a file's bytes waits until they come, a lookup or `stat` never
   *  does. The files' bytes are laid as they arrive (`k_fs_fill`), in
   *  `background`; at the archive's end, or its failure, what has not come
   *  is EIO (`k_fs_pending_end`), never ENOENT. The count of files named. */
  async #layEarly({ stream, modes: lazyModes = {}, mtimes: lazyMtimes, xattrs: baseXattrs, lazy = [], onPart }, index) {
    const k = this.#k;
    const r = await k.r('k_fs_seed', { in: index });
    if (r < 0) throw errno(r, 'the bundle\'s index');
    // The lazy files: the rest bundle's (a head the archive carries is laid
    // over its pending copy at the end, as `#layStream` would).
    const heads = lazy.filter((f) => f[3]);
    const n = r + await this.#layRest([], {
      modes: lazyModes, lazy: lazy.filter((f) => !f[3]), mtimes: lazyMtimes, xattrs: baseXattrs,
      has: () => false, size: () => 0, bytes: null,
    });
    const run = async () => {
      for await (const t of stream) {
        const list = [];
        for (const [p, data] of Object.entries(t.files)) {
          const d = typeof data === 'string' ? enc.encode(data) : data;
          list.push({ f: 'k_fs_fill', a: [{ in: p }, 0n, { in: d.subarray(0, PART) }] });
          for (let off = PART; off < d.length; off += PART) list.push({ f: 'k_fs_fill', a: [{ in: p }, BigInt(off), { in: d.subarray(off, off + PART) }] });
        }
        for (const [p, attrs] of Object.entries(t.xattrs ?? {})) {
          for (const [name, value] of Object.entries(attrs)) {
            const v = value instanceof Uint8Array ? value : enc.encode(String(value));
            list.push({ f: 'k_fs_xattr', a: [-1, { in: p }, XATTR_SET, { in: name }, { in: v }, 0] });
          }
        }
        await batched(k, list, (r, c) => {
          if (c.f === 'k_fs_fill' && r < 0) throw errno(r, c.a[0].in);
        });
        onPart?.(t);
      }
      const left = await k.r('k_fs_pending_end');
      if (left > 0) throw new Error(`the bundle ended with ${left} of the files its index names missing`);
      if (heads.length) await this.#layRest([], { modes: lazyModes, lazy: heads, has: () => true, size: (p) => heads.find((f) => f[0] === p)?.[3], bytes: null });
    };
    this.background = run().catch(async (e) => {
      // the files that did not come: EIO to whoever waits, from now on
      await k.r('k_fs_pending_end').catch(() => {});
      throw e;
    });
    this.background.catch(() => {});
    return n;
  }

  /** An image's calls after its files' (`list`, sent with them): the
   *  directories' modes, the lazy files, hard and symbolic links, times and
   *  extended attributes. `has(p)`/`size(p)`: whether the image has file
   *  `p` and its length; `bytes(p)` its contents. Resolves to the lazy files
   *  laid. */
  async #layRest(list, { dirs = [], modes = {}, lazy = [], hard = {}, symlinks = {}, mtimes, xattrs, has, size, bytes }) {
    const k = this.#k;
    let n = 0;
    const seed = new SeedRecords(list, k.has('k_fs_seed'));
    // Directories' modes once their files are in (a read-only one keeps
    // its mode: Wine reports it read only).
    for (const p of dirs) if (modes[p] !== undefined) seed.chmod(p, modes[p]);
    for (const [p, sz, url, head] of lazy) {
      if (has(p) && !(head && size(p) === head)) continue;
      seed.lazy(p, url, sz, modes[p]);
      n++;
    }
    seed.flush();
    // With no hard links, what follows goes in the same batch: a round trip
    // to the kernel's Worker fewer at every boot.
    const hasHard = Object.keys(hard).length > 0;
    if (hasHard) {
      await batched(k, list.splice(0), (r, c) => {
        if ((c.f === 'k_fs_add' || c.f === 'k_fs_add_lazy') && r < 0) throw errno(r, c.a[0].in);
        if (c.f === 'k_fs_seed' && r < 0) throw errno(r, 'the image\'s seeding records');
      });
    }
    // Hard links after their targets: one inode with two names, as the host
    // had it; a copy where the kernel would not link.
    const copies = [];
    await batched(k, Object.entries(hard).map(([p, target]) => ({ f: 'k_fs_link', a: [{ in: target }, { in: p }], p, target })), (r, c) => {
      if (r < 0 && has(c.target)) copies.push(c);
    });
    for (const { p, target } of copies) fileCalls(list, p, await bytes(target), {});
    for (const [p, target] of Object.entries(hard)) if (modes[target] !== undefined) seed.chmod(p, modes[target]);
    for (const [p, target] of Object.entries(symlinks)) seed.symlink(target, p);
    for (const [p, secs] of Object.entries(mtimes ?? {})) seed.mtime(p, secs);
    seed.flush();
    for (const [p, attrs] of Object.entries(xattrs ?? {})) {
      for (const [name, value] of Object.entries(attrs)) {
        const v = value instanceof Uint8Array ? value : enc.encode(String(value));
        list.push({ f: 'k_fs_xattr', a: [-1, { in: p }, XATTR_SET, { in: name }, { in: v }, 0] });
      }
    }
    await batched(k, list, (r, c) => {
      if ((c.f === 'k_fs_add' || (!hasHard && c.f === 'k_fs_add_lazy')) && r < 0) throw errno(r, c.a[0].in);
      if (c.f === 'k_fs_seed' && r < 0) throw errno(r, 'the image\'s seeding records');
    });
    return n;
  }

  async mount(path, id) {
    const r = await this.#k.r('k_pagefs_mount', { in: path }, id);
    if (r < 0) throw errno(r, `mounting ${path}`);
  }

  async unmount(path) {
    const r = await this.#k.r('k_pagefs_unmount', { in: path });
    if (r < 0) throw errno(r, `unmounting ${path}`);
  }
}

function concat(parts, total) {
  if (parts.length === 1 && parts[0].length === total) return parts[0];
  const out = new Uint8Array(total);
  let at = 0;
  for (const p of parts) {
    out.set(p.subarray(0, Math.min(p.length, total - at)), at);
    at += p.length;
  }
  return out;
}

function meta(b) {
  const v = new DataView(b.buffer, b.byteOffset, META_BYTES);
  const mode = v.getUint32(4, true);
  return {
    type: TYPES[v.getUint32(0, true)] ?? 'file',
    mode: mode & 0o7777,
    size: Number(v.getBigUint64(8, true)),
    ino: Number(v.getBigUint64(16, true)),
    mtime: Number(v.getBigInt64(24, true)) * 1000 + Math.floor(v.getUint32(36, true) / 1e6),
    nlink: v.getUint32(32, true),
  };
}
