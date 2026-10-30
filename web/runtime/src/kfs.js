// The guest's files from the page: the kernel's `k_fs_*` exports through the
// kernel-call Worker (kernel.js), for the Filesystem (fs.js), the boot tree
// and persistence.

import { errno } from './kernel.js';

const enc = new TextEncoder();
const dec = new TextDecoder();
const ERANGE = -34;
/** `remote::xattr_op::SET`. */
const XATTR_SET = 1;
// Linux open flags.
const O_WRONLY = 1, O_CREAT = 0o100, O_EXCL = 0o200, O_TRUNC = 0o1000, O_APPEND = 0o2000, O_DIRECTORY = 0o200000, O_NOFOLLOW = 0o400000;
const READ_MAX = 4 << 20;
const META_BYTES = 72;
const TYPES = ['file', 'directory', 'symlink', 'chardev', 'fifo', 'socket'];
/** The most bytes one batch carries to the Worker. */
const BATCH_BYTES = 32 << 20;

/** Calls in batches of at most BATCH_BYTES of data; each call's result
 *  checked by `check(result, call)` when given. */
async function batched(k, list, check) {
  let part = [];
  let bytes = 0;
  const send = async () => {
    if (!part.length) return;
    const res = await k.calls(part);
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

export class Files {
  #k;
  constructor(k) {
    this.#k = k;
  }

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
    if (!recursive) {
      const r = await this.#k.r('k_fs_mkdir', { in: path }, mode);
      if (r < 0) throw errno(r, path);
      return;
    }
    const list = [];
    let p = '';
    for (const part of path.split('/').filter(Boolean)) {
      p += `/${part}`;
      list.push({ f: 'k_fs_mkdir', a: [{ in: p }, mode] });
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
      const r = await this.#k.r('k_fs_rmdir', { in: path });
      if (r < 0) throw errno(r, path);
      return;
    }
    const r = await this.#k.r('k_fs_unlink', { in: path });
    if (r < 0 && !(force && r === -2)) throw errno(r, path);
  }

  async rename(from, to) {
    const r = await this.#k.r('k_fs_rename', { in: from }, { in: to }, 0);
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

  /** A tree (web/src/tar.js `untar`'s shape) into the live filesystem:
   *  directories, files, modes, hard and symbolic links, lazy files. Over
   *  whatever is there: what a path held is unlinked first (a symlink in
   *  the way would be written through). */
  async addTree(t) {
    let files = 0, bytes = 0;
    const parent = (p) => p.slice(0, p.lastIndexOf('/')) || '/';
    const made = new Set();
    const list = [];
    const mkdirs = (d, mode = 0o755) => {
      let p = '';
      for (const part of d.split('/').filter(Boolean)) {
        p += `/${part}`;
        if (made.has(p)) continue;
        made.add(p);
        list.push({ f: 'k_fs_mkdir', a: [{ in: p }, p === d ? mode : 0o755] });
      }
    };
    const dirOf = (p) => mkdirs(parent(p));
    const fail = new Set(['k_fs_add', 'k_fs_add_lazy']);
    for (const d of t.dirs ?? []) mkdirs(d, t.modes?.[d] ?? 0o755);
    for (const [p, data] of Object.entries(t.files ?? {})) {
      dirOf(p);
      const d = typeof data === 'string' ? enc.encode(data) : data;
      list.push({ f: 'k_fs_unlink', a: [{ in: p }] }, { f: 'k_fs_add', a: [{ in: p }, { in: d }] });
      files++;
      bytes += d.length;
    }
    for (const [p, m] of Object.entries(t.modes ?? {})) list.push({ f: 'k_fs_chmod', a: [{ in: p }, m] });
    for (const [p, target] of Object.entries(t.hard ?? {})) {
      dirOf(p);
      list.push({ f: 'k_fs_unlink', a: [{ in: p }] }, { f: 'k_fs_link', a: [{ in: target }, { in: p }] });
    }
    for (const [p, target] of Object.entries(t.links ?? {})) {
      dirOf(p);
      list.push({ f: 'k_fs_unlink', a: [{ in: p }] }, { f: 'k_fs_symlink', a: [{ in: target }, { in: p }] });
    }
    for (const [p, size, url] of t.lazy ?? []) {
      dirOf(p);
      list.push({ f: 'k_fs_unlink', a: [{ in: p }] }, { f: 'k_fs_add_lazy', a: [{ in: p }, { in: url }, BigInt(size)] });
      files++;
    }
    for (const [p, secs] of Object.entries(t.mtimes ?? {})) list.push({ f: 'k_fs_set_mtime', a: [{ in: p }, BigInt(secs)] });
    xattrCalls(list, t.xattrs);
    await batched(this.#k, list, (r, c) => {
      if (fail.has(c.f) && r < 0) throw errno(r, c.a[0].in);
    });
    return { files, bytes };
  }

  /** The tree the system boots with (the Filesystem's layers), into the
   *  empty root, in gex.mjs `load`'s order: empty directories, files,
   *  directories' modes, lazy files, hard links, symbolic links, times,
   *  extended attributes. Seeding, not a program's writes (a persist diff
   *  does not take it). */
  async seed({ files = {}, dirs = [], modes = {}, hard = {}, symlinks = {}, lazy = [], mtimes = {}, xattrs = {} }) {
    const PART = 16 << 20;
    const list = [];
    for (const d of dirs) list.push({ f: 'k_fs_add_dir', a: [{ in: d }, 0o755] });
    const chmod = (p, m) => list.push({ f: 'k_fs_chmod', a: [{ in: p }, m] });
    for (const [p, data] of Object.entries(files)) {
      const d = typeof data === 'string' ? enc.encode(data) : data;
      // A large file in parts, so the Worker's memory never holds a whole one.
      list.push({ f: 'k_fs_add', a: [{ in: p }, { in: d.subarray(0, PART) }] });
      for (let off = PART; off < d.length; off += PART) list.push({ f: 'k_fs_add_part', a: [{ in: p }, BigInt(off), { in: d.subarray(off, off + PART) }] });
      if (modes[p] !== undefined) chmod(p, modes[p]);
    }
    for (const d of dirs) if (modes[d] !== undefined) chmod(d, modes[d]);
    for (const [p, size, url, head] of lazy) {
      // The bundle's file wins, unless it is the lazy file's head.
      if (files[p] !== undefined && !(head && files[p].length === head)) continue;
      list.push({ f: 'k_fs_add_lazy', a: [{ in: p }, { in: url }, BigInt(size)] });
      if (modes[p] !== undefined) chmod(p, modes[p]);
    }
    for (const [p, target] of Object.entries(hard)) {
      list.push({ f: 'k_fs_link', a: [{ in: target }, { in: p }] });
      if (modes[target] !== undefined) chmod(p, modes[target]);
    }
    for (const [p, target] of Object.entries(symlinks)) list.push({ f: 'k_fs_symlink', a: [{ in: target }, { in: p }] });
    for (const [p, secs] of Object.entries(mtimes ?? {})) list.push({ f: 'k_fs_set_mtime', a: [{ in: p }, BigInt(Math.trunc(secs))] });
    xattrCalls(list, xattrs);
    list.push({ f: 'k_fs_populate_done' });
    await batched(this.#k, list);
  }

  /** A file's bytes by its own name (a tree path: no link on it), `size`
   *  of them; null when it cannot be opened. */
  async readRaw(path, size) {
    const h = await this.#k.r('k_fs_open', { in: path }, 0n, 0);
    if (h < 0) return null;
    try {
      const list = [];
      for (let off = 0; off < size; off += READ_MAX) list.push({ f: 'k_fs_read', a: [h, BigInt(off), { out: Math.min(READ_MAX, size - off) }], stop: true });
      const parts = [];
      let total = 0;
      for (let i = 0; i < list.length; i += 8) {
        for (const { r, out: [b] } of await this.#k.calls(list.slice(i, i + 8))) {
          if (r <= 0) return concat(parts, total);
          parts.push(b);
          total += r;
        }
      }
      return concat(parts, total);
    } finally {
      await this.#k.r('k_fs_close', h);
    }
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

function xattrCalls(list, xattrs) {
  for (const [p, attrs] of Object.entries(xattrs ?? {})) {
    for (const [name, value] of Object.entries(attrs)) {
      const v = typeof value === 'string' ? enc.encode(value) : value;
      list.push({ f: 'k_fs_xattr', a: [-1, { in: p }, XATTR_SET, { in: name }, { in: v }, 0] });
    }
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
