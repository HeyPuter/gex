// The page's side of `fs.mount`: the requests guest calls make on a mounted
// directory (shims/kernel/src/pagefs.rs, shims/browser/js/page-fs.mjs),
// answered from the page's provider. They arrive in slots of a
// SharedArrayBuffer; the page waits for them with Atomics.waitAsync and
// answers each as its provider's promise settles, many at once.

import { HEAD, SLOT, SLOTS, DATA, ASKED, TAKEN, DONE, makePageFsControl } from '../../../shims/browser/js/page-fs.mjs';

const OP = { STAT: 1, LSTAT: 2, READDIR: 3, OPEN: 4, READ: 5, WRITE: 6, CLOSE: 7, MKDIR: 8, UNLINK: 9, RMDIR: 10, RENAME: 11, TRUNCATE: 12, CHMOD: 13, UTIMES: 14, READLINK: 15, SYMLINK: 16 };
const O = { ACCMODE: 3, WRONLY: 1, RDWR: 2, CREAT: 0o100, EXCL: 0o200, TRUNC: 0o1000, APPEND: 0o2000 };
const ERRNO = {
  EPERM: 1, ENOENT: 2, EIO: 5, EBADF: 9, EAGAIN: 11, ENOMEM: 12, EACCES: 13, EBUSY: 16, EEXIST: 17, EXDEV: 18,
  ENODEV: 19, ENOTDIR: 20, EISDIR: 21, EINVAL: 22, EFBIG: 27, ENOSPC: 28, EROFS: 30, ENAMETOOLONG: 36,
  ENOSYS: 38, ENOTEMPTY: 39, ELOOP: 40, ENOTSUP: 95, EOPNOTSUPP: 95, ETIMEDOUT: 110,
};
const TYPE = { file: 1, directory: 2, symlink: 3 };
const STAT_BYTES = 32;

/** An error a provider threw, as a negated errno: its `errno` (positive or
 *  negative), its `code` ('ENOENT'), a DOMException's name, else EIO. */
function errnoOf(e) {
  if (Number.isInteger(e?.errno) && e.errno !== 0) return -Math.abs(e.errno);
  const code = typeof e?.code === 'string' ? e.code : null;
  if (code && ERRNO[code]) return -ERRNO[code];
  if (e?.name === 'NotFoundError') return -ERRNO.ENOENT;
  if (e?.name === 'TypeMismatchError') return -ERRNO.ENOTDIR;
  if (e?.name === 'NoModificationAllowedError' || e?.name === 'NotAllowedError') return -ERRNO.EACCES;
  if (e?.name === 'QuotaExceededError') return -ERRNO.ENOSPC;
  if (e?.name === 'InvalidModificationError') return -ERRNO.ENOTEMPTY;
  return -ERRNO.EIO;
}

const fail = (code) => Object.assign(new Error(code), { code });

function typeOf(st) {
  const t = typeof st?.type === 'string' ? st.type
    : st?.isDirectory?.() ? 'directory' : st?.isSymbolicLink?.() ? 'symlink' : 'file';
  return TYPE[t === 'dir' ? 'directory' : t] ?? 1;
}

/** A provider's stat as pagefs.rs reads it: type, mode, size, mtime (s), ino. */
function putStat(dv, at, st) {
  const t = typeOf(st);
  const mode = Number.isInteger(st?.mode) ? st.mode & 0o7777 : t === 2 ? 0o755 : 0o644;
  const mt = st?.mtime instanceof Date ? st.mtime.getTime() : Number(st?.mtimeMs ?? st?.mtime ?? 0);
  dv.setUint32(at, t, true);
  dv.setUint32(at + 4, mode, true);
  dv.setBigUint64(at + 8, BigInt(Math.max(0, Math.floor(Number(st?.size ?? 0)))), true);
  dv.setBigInt64(at + 16, BigInt(Math.floor(mt / 1000)), true);
  const ino = Number(st?.ino ?? 0);
  dv.setBigUint64(at + 24, BigInt(Number.isFinite(ino) && ino > 0 ? Math.floor(ino) : 0), true);
}

/** The page's filesystems and the requests for them. One per system. */
export class PageFsHost {
  #ctl;
  #i32;
  #u8;
  #mounts = new Map(); // id -> provider
  #handles = new Map(); // n -> { id, h }
  #nextHandle = 1;
  #nextId = 1;
  #seen = 0;
  #listings = new Map(); // `${id}:${path}` -> names, for a listing read in pages
  #serving = false;

  constructor() {
    this.#ctl = makePageFsControl();
    this.#i32 = new Int32Array(this.#ctl);
    this.#u8 = new Uint8Array(this.#ctl);
  }

  /** What every Worker is handed (runtime.js `pageFs`). */
  get control() {
    return { ctl: this.#ctl };
  }

  add(provider) {
    const id = this.#nextId++;
    this.#mounts.set(id, provider);
    this.#serve();
    return id;
  }

  remove(id) {
    this.#mounts.delete(id);
    for (const [n, x] of this.#handles) if (x.id === id) this.#handles.delete(n);
  }

  #serve() {
    if (this.#serving) return;
    this.#serving = true;
    const loop = () => {
      this.#scan();
      const w = Atomics.waitAsync(this.#i32, 0, this.#seen);
      if (w.async) w.value.then(loop);
      else queueMicrotask(loop);
    };
    loop();
  }

  #scan() {
    this.#seen = Atomics.load(this.#i32, 0);
    for (let s = 0; s < SLOTS; s++) {
      const base = (HEAD + s * SLOT) / 4;
      if (Atomics.compareExchange(this.#i32, base, ASKED, TAKEN) !== ASKED) continue;
      this.#answer(s).catch(() => {});
    }
  }

  async #answer(s) {
    const base = (HEAD + s * SLOT) / 4;
    const at = HEAD + s * SLOT + DATA;
    const len = this.#i32[base + 2];
    // The request, copied out: the slot is where the answer goes.
    const req = this.#u8.slice(at, at + len);
    let result;
    let out = null;
    try {
      [result, out] = await this.#call(req);
    } catch (e) {
      result = errnoOf(e);
      out = null;
    }
    const n = out ? Math.min(out.length, SLOT - DATA) : 0;
    if (n) this.#u8.set(out.subarray(0, n), at);
    this.#i32[base + 1] = result;
    this.#i32[base + 3] = n;
    Atomics.store(this.#i32, base, DONE);
    Atomics.notify(this.#i32, base);
  }

  async #call(req) {
    const dv = new DataView(req.buffer, req.byteOffset, req.byteLength);
    const op = dv.getUint32(0, true);
    const id = dv.getUint32(4, true);
    const handle = dv.getUint32(8, true);
    const flags = dv.getUint32(12, true);
    const mode = dv.getUint32(16, true);
    const len = dv.getUint32(20, true);
    const offset = Number(dv.getBigUint64(24, true));
    let p = 32;
    const text = () => {
      const n = dv.getUint32(p, true);
      const s = new TextDecoder().decode(req.subarray(p + 4, p + 4 + n));
      p += 4 + n;
      return s;
    };
    const path = req.length > p ? text() : '';
    const path2 = req.length > p ? text() : '';
    const data = req.subarray(p);
    const fs = op === OP.READ || op === OP.WRITE || op === OP.CLOSE ? this.#mounts.get(this.#handles.get(handle)?.id) : this.#mounts.get(id);
    if (!fs) throw fail('ENODEV');
    const statBytes = (st) => {
      const b = new Uint8Array(STAT_BYTES);
      putStat(new DataView(b.buffer), 0, st);
      return b;
    };
    switch (op) {
      case OP.STAT:
        return [0, statBytes(await fs.stat(path))];
      case OP.LSTAT:
        return [0, statBytes(await (fs.lstat ?? fs.stat).call(fs, path))];
      case OP.READDIR:
        return this.#readdir(fs, id, path, offset);
      case OP.OPEN:
        return this.#open(fs, id, path, flags, mode);
      case OP.READ: {
        const h = this.#handles.get(handle);
        const b = await fs.read(h.h, offset, len);
        const bytes = b instanceof Uint8Array ? b : new Uint8Array(b ?? []);
        return [Math.min(bytes.length, len), bytes];
      }
      case OP.WRITE: {
        const h = this.#handles.get(handle);
        const n = await fs.write(h.h, offset, data.slice());
        return [Number.isInteger(n) ? n : data.length, null];
      }
      case OP.CLOSE: {
        const h = this.#handles.get(handle);
        this.#handles.delete(handle);
        await fs.close?.(h.h);
        return [0, null];
      }
      case OP.MKDIR:
        await fs.mkdir(path, mode & 0o7777);
        return [0, null];
      case OP.UNLINK:
        await fs.unlink(path);
        return [0, null];
      case OP.RMDIR:
        await (fs.rmdir ?? fs.unlink).call(fs, path);
        return [0, null];
      case OP.RENAME:
        await fs.rename(path, path2);
        return [0, null];
      case OP.TRUNCATE: {
        if (!fs.truncate) throw fail('EPERM');
        const h = handle === 0xffffffff ? undefined : this.#handles.get(handle)?.h;
        await fs.truncate(path, offset, h);
        return [0, null];
      }
      case OP.CHMOD:
        // A provider with no modes takes a chmod as done: cp, tar and
        // installers chmod what they make.
        await fs.chmod?.(path, mode & 0o7777);
        return [0, null];
      case OP.UTIMES:
        await fs.utimes?.(path, new Date(offset * 1000));
        return [0, null];
      case OP.READLINK: {
        if (!fs.readlink) throw fail('EINVAL');
        return this.#bytes(new TextEncoder().encode(await fs.readlink(path)));
      }
      case OP.SYMLINK:
        if (!fs.symlink) throw fail('EPERM');
        await fs.symlink(path, path2);
        return [0, null];
      default:
        throw fail('ENOSYS');
    }
  }

  #bytes(b) {
    return [b.length, b];
  }

  async #open(fs, id, path, flags, mode) {
    let st = null;
    try {
      st = await fs.stat(path);
    } catch (e) {
      if (errnoOf(e) !== -ERRNO.ENOENT || !(flags & O.CREAT)) throw e;
    }
    if (st && flags & O.CREAT && flags & O.EXCL) throw fail('EEXIST');
    const out = new Uint8Array(STAT_BYTES + 4);
    const dv = new DataView(out.buffer);
    if (st && typeOf(st) === TYPE.directory) {
      putStat(dv, 0, st);
      dv.setInt32(STAT_BYTES, -1, true);
      return [0, out];
    }
    const acc = flags & O.ACCMODE;
    const h = await fs.open(path, {
      read: acc !== O.WRONLY,
      write: acc === O.WRONLY || acc === O.RDWR,
      create: !!(flags & O.CREAT),
      truncate: !!(flags & O.TRUNC),
      exclusive: !!(flags & O.EXCL),
      append: !!(flags & O.APPEND),
      mode: mode & 0o7777,
    });
    if (!st || flags & O.TRUNC) {
      try { st = await fs.stat(path); } catch { st = { type: 'file', size: 0, mode: mode & 0o7777, mtime: Date.now() }; }
    }
    const n = this.#nextHandle++;
    this.#handles.set(n, { id, h });
    putStat(dv, 0, st);
    dv.setInt32(STAT_BYTES, n, true);
    return [0, out];
  }

  async #readdir(fs, id, path, from) {
    const key = `${id}:${path}`;
    let list = from > 0 ? this.#listings.get(key) : null;
    if (!list) {
      const got = await fs.readdir(path);
      list = [];
      for (const e of got) {
        if (typeof e === 'string') {
          // Names alone: their types from a stat each (find and ls -l trust
          // a listing's types).
          let t = TYPE.file;
          try { t = typeOf(await fs.lstat?.(`${path.replace(/\/$/, '')}/${e}`) ?? await fs.stat(`${path.replace(/\/$/, '')}/${e}`)); } catch {}
          list.push([e, t]);
        } else {
          list.push([e.name, typeOf(e)]);
        }
      }
      list = list.filter(([n]) => n && n !== '.' && n !== '..');
      this.#listings.set(key, list);
    }
    const out = new Uint8Array(SLOT - DATA);
    let at = 0;
    let n = 0;
    const enc = new TextEncoder();
    for (let i = from; i < list.length; i++) {
      const name = enc.encode(list[i][0]);
      if (at + 3 + name.length > out.length) break;
      out[at] = list[i][1];
      out[at + 1] = name.length & 0xff;
      out[at + 2] = name.length >> 8;
      out.set(name, at + 3);
      at += 3 + name.length;
      n++;
    }
    if (n === 0) this.#listings.delete(key);
    return [n, out.subarray(0, at)];
  }
}
