// `Filesystem`: the guest's root, one per system. Before boot it is the tars
// and files a page adds, laid one over another in the order added into the
// kernel's root at boot (kfs.js `lay`).
// After boot, a tar or files are written into the live tree, and the guest's
// files can be read and written from the page.
//
// `Filesystem` is abstract: the tree, its block cache in the kernel's
// memory and the kernel's wiring. Where the files' contents are kept is a
// subclass, the backing store, and the caller makes one: `MemoryFilesystem`
// (in the kernel's memory, unbounded unless given a maximum),
// `OPFSFilesystem` (a block store in the origin private file system, with a
// bounded cache of it in the kernel's memory), or its own, which implements
// the store's methods (`read`, `write`, `size`, `truncate`, `flush`, and
// `open` if it needs one). The built-ins answer the kernel by their own
// paths (the kernel's memory; a store Worker with a sync access handle); a
// page's own store is asked from the page (blocks.mjs `serveSlots`; the
// default build only).

import { INTERNAL, internals, bytesOf } from './internal.js';

const norm = (p) => {
  if (typeof p !== 'string' || !p.startsWith('/')) throw new TypeError(`not an absolute guest path: ${p}`);
  const out = [];
  for (const part of p.split('/')) {
    if (!part || part === '.') continue;
    if (part === '..') out.pop();
    else out.push(part);
  }
  return `/${out.join('/')}`;
};
const under = (at, p) => norm(`${at}/${p}`);

/** zstd's frame magic. */
const isZstd = (b) => b.length > 4 && b[0] === 0x28 && b[1] === 0xb5 && b[2] === 0x2f && b[3] === 0xfd;

async function tarBytes(src) {
  let bytes = await bytesOf(src, 'tar');
  if (isZstd(bytes)) {
    const { unzstd } = await import('../../src/zfetch.js');
    bytes = await unzstd(bytes);
  }
  // A tar's first header says "ustar" (POSIX and GNU), or the tar is empty
  // (zero blocks). Anything else is not one: a server's page for a path it
  // has no file at (a dev server answers 200 with its index.html), laid as
  // a tar, would be an empty root that fails nothing until a program is
  // missing.
  const magic = new TextDecoder().decode(bytes.subarray(257, 262));
  if (bytes.length < 512 || (magic !== 'ustar' && bytes.subarray(0, 512).some((b) => b !== 0))) {
    const head = new TextDecoder().decode(bytes.subarray(0, 40)).replace(/\s+/g, ' ').trim();
    throw new Error(`tar ${typeof src === 'string' || src instanceof URL ? src : '(bytes)'}: not a tar or a zstd tar (it starts ${JSON.stringify(head)})`);
  }
  return bytes;
}

/** A lazy list, as `[[path, size, url, mode, mtime], ...]`: the page's own
 *  (`[[path, size, url, mode], ...]`), or the URL of one as JSON, which may
 *  also be an ejected build's lazy.json (site/eject-site.mjs: `{ base, files:
 *  [[path, size, rel, mtime, mode], ...] }`, mtime in seconds, mode and
 *  mtime optional; `rel` may be a rest bundle's `BUNDLE#OFFSET`), or such
 *  an object itself (its `base` then resolved against the page). A file's
 *  URL is a path on the page's origin. */
/** A lazy list's `base`, resolved as a link in the list (`at`, its own
 *  URL) is. An ejected site writes a path from the site's root
 *  (`/project/build/web/`, or `.../lazy/` below it) and keeps the list in
 *  that directory or the one above: when the list's directory ends with
 *  the base, or with a leading part of it, the site's root is what comes
 *  before that, wherever the site is served. */
function listBase(rel, at) {
  const r = String(rel);
  // (a full URL, or no list's URL to go by: the page's own object)
  if (!r.startsWith('/') || r.startsWith('//') || !at) return new URL(r, at);
  const u = new URL(at);
  const dir = u.pathname.replace(/[^/]*$/, '');
  const parts = r.replace(/\/*$/, '').split('/').slice(1);
  for (let n = parts.length; n > 0; n--) {
    const head = `/${parts.slice(0, n).join('/')}/`;
    if (dir.endsWith(head)) return new URL(`${dir.slice(0, dir.length - head.length)}${r.replace(/\/*$/, '/')}`, u);
  }
  return new URL(r, u);
}

async function lazyList(lazy) {
  let list = lazy;
  let base = globalThis.location?.href;
  if (typeof lazy === 'string' || lazy instanceof URL) {
    base = new URL(lazy, globalThis.location?.href).href;
    list = JSON.parse(new TextDecoder().decode(await bytesOf(base, 'lazy list')));
  }
  if (!Array.isArray(list)) {
    if (Array.isArray(list?.files)) {
      const b = listBase(list.base ?? './', base);
      return list.files.map(([p, size, rel, mtime, mode]) => [p, size, new URL(rel, b).href, mode ?? undefined, mtime ?? undefined]);
    }
    throw new TypeError('lazy: a list of [path, size, url] or its URL');
  }
  return list.map(([p, size, url, mode, mtime]) => [p, size, new URL(url, base).href, mode, mtime]);
}

/** A same-origin URL as the path the kernel keeps for a lazy file. */
function lazyPath(href) {
  const u = new URL(href, globalThis.location?.href);
  // Under Node (the runtime's own tests), a file URL is a host path.
  if (u.protocol === 'file:') return decodeURIComponent(u.pathname);
  if (globalThis.location && u.origin !== globalThis.location.origin) {
    throw new Error(`lazy file ${href}: only the page's own origin can be read lazily`);
  }
  return u.pathname + u.search + u.hash;
}

/** A directory the page serves (`Filesystem.mount`). Events: `unmounted`. */
export class Mount extends EventTarget {
  #fs;

  constructor(fs, path, provider) {
    super();
    this.#fs = fs;
    /** Where it is in the guest's tree. */
    this.path = path;
    this.provider = provider;
  }

  /** Take it out of the tree ([`Filesystem.unmount`]). */
  unmount() {
    return this.#fs.unmount(this.path);
  }
}

/** Blocks of the store, and how large a file stays in the kernel's memory. */
const BLOCK = 65536;
const INLINE = 16384;

/** A built-in's own way to the kernel, instead of the store's methods
 *  (`MemoryFilesystem`, `OPFSFilesystem`): module-private. */
const FAST = Symbol('fast store');

/** The store's methods a subclass implements. */
const STORE_METHODS = ['read', 'write', 'size', 'truncate', 'flush'];

/** The block cache's size option: MiB, 16 to 4096, default 256. */
function cacheOf(cache, who) {
  cache ??= 256;
  if (!Number.isInteger(cache) || cache < 16 || cache > 4096) throw new TypeError(`${who}: cache is MiB, 16 to 4096`);
  return cache;
}

/** The write-back's age option: ms a written block may stay only in the
 *  cache, 100 to 600000, default 5000. */
function writebackOf(writeback, who) {
  writeback ??= 5000;
  if (!Number.isInteger(writeback) || writeback < 100 || writeback > 600000) throw new TypeError(`${who}: writeback is ms, 100 to 600000`);
  return writeback;
}

/** Never kept across loads: what the kernel makes, and scratch. */
const ALWAYS_EXCLUDED = Object.freeze(['/tmp', '/run', '/proc', '/sys', '/dev']);

export class Filesystem extends EventTarget {
  #sys;
  #cache;
  #writeback;
  #sources = [];
  #mounts = new Map(); // path -> { mount, id }
  #exclude;

  /** Abstract: a page makes a `MemoryFilesystem`, an `OPFSFilesystem`, or
   *  a subclass of its own that implements the store's methods. `cache`:
   *  MiB of the store held in the kernel's memory (default 256);
   *  `writeback`: ms a written block may stay only there before it goes to
   *  the store (default 5000); `exclude`: more paths of the root its store
   *  does not keep across loads (beside /tmp, /run, /proc, /sys, /dev and
   *  the mounts). */
  constructor(sys, { cache, writeback, exclude = [] } = {}) {
    if (new.target === Filesystem) {
      throw new TypeError('Filesystem is abstract: make a MemoryFilesystem(sys), an OPFSFilesystem(sys, { name }), or a subclass that implements its store');
    }
    super();
    if (!this[FAST]) {
      const missing = STORE_METHODS.filter((m) => this[m] === Filesystem.prototype[m]);
      if (missing.length) throw new TypeError(`${new.target.name}: a Filesystem's store implements ${missing.join(', ')}`);
    }
    this.#cache = cacheOf(cache, new.target.name);
    this.#writeback = writebackOf(writeback, new.target.name);
    if (!Array.isArray(exclude) || exclude.some((p) => typeof p !== 'string' || !p.startsWith('/'))) throw new TypeError(`${new.target.name}: exclude is a list of absolute paths`);
    this.#exclude = exclude;
    const i = internals(sys, 'Filesystem');
    i.attach('filesystem', this, { beforeBoot: true });
    this.#sys = i;
    i.beforeBoot((opts) => this.#seed(opts));
    this[INTERNAL] = {
      sources: () => this.#sources,
      cache: () => this.#cache,
      writeback: () => this.#writeback,
      /** Where the contents are kept, as the system wires it
       *  (system.js `#store`): a built-in's own, or this store's methods. */
      store: () => this[FAST]?.() ?? { kind: 'page', cache: this.#cache, writeback: this.#writeback, block: BLOCK, inline: INLINE, fs: this },
      /** For the system's start: the mounts made before boot,
       *  in place before the first program. */
      mounts: () => [...this.#mounts].map(([path, m]) => [path, m.id]),
      phase: () => this.#sys.phase,
      /** The paths the store does not keep (system.js `#start`). */
      exclude: () => [...ALWAYS_EXCLUDED, ...this.#exclude, ...[...this.#mounts.keys()]],
      /** A program's image as an untarred bundle has it (kfs.js
       *  `layImage`): a preset's (page/src/run-program.js), not the API's. */
      addImage: (layer) => this.#add(Promise.resolve({ image: true, ...layer })),
    };
  }

  // The store: the files' contents, one array of bytes the kernel addresses
  // by offset in blocks of 64 KiB. A subclass implements these; the kernel
  // calls them as its cache fills and writes back, one call at a time.

  /** Make the store ready, before boot (optional). What it holds is the
   *  root a load before kept (empty: none); a store that is to start over
   *  empties itself here. */
  async open() {}

  /** `length` bytes at `offset` (a Uint8Array; shorter past the end, which
   *  reads as zeros). */
  async read(_offset, _length) { throw abstract('read'); }

  /** Write `bytes` (a Uint8Array) at `offset`, growing the store. */
  async write(_offset, _bytes) { throw abstract('write'); }

  /** The store's length in bytes. */
  async size() { throw abstract('size'); }

  /** Set the store's length to `length` bytes (grows with zeros). A store
   *  that is full throws an error with `code: 'ENOSPC'`. */
  async truncate(_length) { throw abstract('truncate'); }

  /** Make what was written durable. */
  async flush() { throw abstract('flush'); }

  /** The guest's files (kfs.js), once the system runs. */
  #files(what) {
    if (this.#sys.phase !== 'running') throw new Error(`${what}: the system is ${this.#sys.phase}, not running`);
    return this.#sys.files();
  }

  /**
   * A tar (bytes, a Blob, a stream, or a URL; zstd or not) laid over the
   * root at `at` (default '/'). Before boot it is one more layer; after, it
   * is written into the running tree. `lazy`: files fetched as they are
   * first read, `[[path, size, url, mode], ...]` (mode optional, 0644 without
   * one) or the URL of such a list, laid with the tar (`src` may then be
   * null). A list at a URL may be `{ base, files }`, each url relative to
   * `base`, itself relative to the list.
   */
  addTar(src, { at = '/', lazy } = {}) {
    const p = (async () => {
      const lz = lazy ? await lazyList(lazy) : [];
      return {
        tar: src == null ? null : await tarBytes(src),
        at: norm(at),
        lazy: lz.map(([p, size, url, mode, mtime]) => [under(at, p), size, lazyPath(url), mode, mtime]),
      };
    })();
    return this.#add(p);
  }

  /** Single files: `{ '/path': bytes | Blob | URL | string }` (a string is
   *  the file's text), each of `mode` (default 0644). */
  addFiles(map, { mode } = {}) {
    const p = (async () => {
      const files = {};
      const modes = {};
      for (const [path, v] of Object.entries(map)) {
        files[norm(path)] = typeof v === 'string' ? new TextEncoder().encode(v) : await bytesOf(v, path);
        if (mode !== undefined) modes[norm(path)] = mode;
      }
      return { files, modes };
    })();
    return this.#add(p);
  }

  #add(layer) {
    if (this.#sys.phase === 'new' || this.#sys.phase === 'booting') {
      this.#sources.push(layer);
      layer.catch(() => {});
      return layer.then(() => {});
    }
    return layer.then((l) => {
      // Its Workers started without the rest bundle's decoder (boot's
      // layers had no such file).
      if (l.lazy?.some(([, , url]) => url.includes('#')) && !this.#sys.restBundle()) {
        throw new Error('addTar: a rest bundle\'s lazy files (BUNDLE#OFFSET) after boot, in a system that booted with none: add the bundle\'s list before boot');
      }
      return l.image ? this.#files('addTar').layImage(l) : this.#files('addTar').lay(l);
    }).then(() => {});
  }

  /** For boot: the layers, in order (later wins). */
  async #seed(opts) {
    opts.layers = await Promise.all(this.#sources);
    this.#sources.length = 0;
  }

  #host() {
    const h = this.#sys.pageFs();
    if (!h) throw new Error('mount: this system serves no page directories');
    return h;
  }

  /**
   * Serve `path` from `provider`, the page's own filesystem, for every
   * program, before boot or while the system runs. Over whatever the tree
   * has there, which comes back at `unmount`. A guest call on it waits, in
   * its own thread only, for the provider's promise. The provider's methods
   * take paths inside it (`/`, `/a/b`) and throw errors with a `code`
   * ('ENOENT', ...): `stat(path)` -> `{ type: 'file' | 'directory' |
   * 'symlink', size, mode, mtime (ms or a Date), ino }`; `readdir(path)` ->
   * `[{ name, type }]` or names; `open(path, { read, write, create,
   * truncate, exclusive, append, mode })` -> a handle (anything);
   * `read(handle, offset, length)` -> bytes; `write(handle, offset, bytes)`
   * -> count; `close(handle)`; `mkdir(path, mode)`, `unlink(path)`,
   * `rename(from, to)`; and when it has them `lstat`, `rmdir`,
   * `truncate(path, length, handle)`, `chmod(path, mode)`, `utimes(path,
   * mtime)`, `readlink(path)`, `symlink(target, path)`. Returns a `Mount`.
   */
  mount(path, provider) {
    const at = norm(path);
    if (at === '/') throw new Error('mount: the root is the system\'s own');
    for (const m of ['stat', 'readdir', 'open', 'read', 'close']) {
      if (typeof provider?.[m] !== 'function') throw new TypeError(`mount: the provider has no ${m}()`);
    }
    if (this.#mounts.has(at)) throw new Error(`mount: ${at} is mounted already`);
    if (!['new', 'booting', 'running'].includes(this.#sys.phase)) throw new Error(`mount: the system is ${this.#sys.phase}`);
    const id = this.#host().add(provider);
    const mount = new Mount(this, at, provider);
    this.#mounts.set(at, { mount, id });
    if (this.#sys.phase === 'running') {
      mount.ready = this.#sys.files().mount(at, id).then(() => mount);
    } else {
      mount.ready = Promise.resolve(mount);
    }
    return mount;
  }

  /** Take the mount at `path` out of the tree: what was there before shows
   *  again, and its files still open fail. */
  async unmount(path) {
    const at = norm(path);
    const m = this.#mounts.get(at);
    if (!m) throw Object.assign(new Error(`unmount: nothing mounted at ${at}`), { code: 'EINVAL' });
    this.#mounts.delete(at);
    if (this.#sys.phase === 'running') await this.#sys.files().unmount(at);
    this.#host().remove(m.id);
    m.mount.dispatchEvent(new Event('unmounted'));
  }

  /** Every mount, as `[Mount]`. */
  mounts() {
    return [...this.#mounts.values()].map((m) => m.mount);
  }

  // -- the running tree ----------------------------------------------------

  /** A file's bytes (`encoding`: its text). */
  async readFile(path, { encoding } = {}) {
    const b = await this.#files('readFile').readFile(norm(path));
    return encoding ? new TextDecoder(encoding).decode(b) : b;
  }

  /** Write a file, made if missing (`mode`, default 0644); `append`. */
  async writeFile(path, data, { mode, append, exclusive } = {}) {
    const bytes = typeof data === 'string' ? new TextEncoder().encode(data) : await bytesOf(data, path);
    return this.#files('writeFile').writeFile(norm(path), bytes, { mode, append, exclusive });
  }

  /** `{type, mode, size, ino, mtime (ms), nlink}`; `follow: false` is lstat. */
  stat(path, { follow = true } = {}) {
    return this.#files('stat').stat(norm(path), follow);
  }

  /** `[{name, type}]`, without `.` and `..`. */
  readdir(path) {
    return this.#files('readdir').readdir(norm(path));
  }

  mkdir(path, { recursive = false, mode = 0o755 } = {}) {
    return this.#files('mkdir').mkdir(norm(path), { recursive, mode });
  }

  rm(path, { recursive = false, force = false } = {}) {
    return this.#files('rm').rm(norm(path), { recursive, force });
  }

  rename(from, to) {
    return this.#files('rename').rename(norm(from), norm(to));
  }

  symlink(target, path) {
    return this.#files('symlink').symlink(target, norm(path));
  }

  readlink(path) {
    return this.#files('readlink').readlink(norm(path));
  }

  chmod(path, mode) {
    return this.#files('chmod').chmod(norm(path), mode);
  }

  /**
   * Changes to `path` while the system runs, made by anyone (a program or
   * the page), through the guest's inotify: for a directory, its entries
   * (not their subdirectories' entries); for a file, the file.
   * `fn({type, path})` for each, `type` one of 'create', 'change' (a write
   * finished), 'attrib', 'delete', 'move-out', 'move-in' (a rename's two
   * halves share a `cookie`), and 'gone' when `path` itself was deleted or
   * moved, after which nothing more comes. Returns a function that stops it.
   */
  watch(path, fn) {
    const at = norm(path);
    const i = this.#sys;
    if (!this.#watching) {
      this.#watching = new Map();
      i.on('watch', (m) => {
        const f = this.#watching.get(m.h);
        if (!f) return;
        if (m.type === 'gone') this.#watching.delete(m.h);
        f({ type: m.type, path: m.name ? norm(`${f.at}/${m.name}`) : f.at, ...(m.cookie ? { cookie: m.cookie } : {}) });
      });
    }
    let h = null;
    let stopped = false;
    const each = Object.assign((e) => fn(e), { at });
    this.#files('watch');
    i.programs().addWatch(at).then((got) => {
      h = got;
      if (stopped) return i.programs().rmWatch(h).catch(() => {});
      this.#watching.set(h, each);
    }, (error) => fn({ type: 'error', path: at, error }));
    return () => {
      if (stopped) return;
      stopped = true;
      if (h !== null && this.#watching.delete(h)) i.programs()?.rmWatch(h).catch(() => {});
    };
  }
  #watching = null;
}


const abstract = (m) => new TypeError(`Filesystem.${m}: a subclass's store implements it`);

/** The root's file contents in the kernel's memory, as many bytes as the
 *  page can hold, or at most `max` (a write past it is ENOSPC). Gone with
 *  the page. The kernel keeps them itself; the store's methods are a plain
 *  array of bytes for a caller that uses them directly. */
export class MemoryFilesystem extends Filesystem {
  #max;
  #chunks = []; // 1 MiB each
  #size = 0;

  constructor(sys, { max } = {}) {
    max ??= 0;
    if (!Number.isSafeInteger(max) || max < 0) throw new TypeError('MemoryFilesystem: max is a number of bytes');
    super(sys);
    this.#max = max;
  }

  [FAST]() { return { kind: 'memory', max: this.#max }; }

  async read(offset, length) {
    const n = Math.max(0, Math.min(length, this.#size - offset));
    const out = new Uint8Array(n);
    for (let at = 0; at < n;) {
      const p = offset + at, c = this.#chunks[Math.floor(p / CHUNK)], o = p % CHUNK, k = Math.min(n - at, CHUNK - o);
      if (c) out.set(c.subarray(o, o + k), at);
      at += k;
    }
    return out;
  }

  async write(offset, bytes) {
    const end = offset + bytes.length;
    if (end > this.#size) await this.truncate(end);
    for (let at = 0; at < bytes.length;) {
      const p = offset + at, i = Math.floor(p / CHUNK), o = p % CHUNK, k = Math.min(bytes.length - at, CHUNK - o);
      (this.#chunks[i] ??= new Uint8Array(CHUNK)).set(bytes.subarray(at, at + k), o);
      at += k;
    }
  }

  async size() { return this.#size; }

  async truncate(length) {
    if (this.#max && length > this.#max) throw Object.assign(new Error('MemoryFilesystem: past its max'), { code: 'ENOSPC' });
    if (length < this.#size) {
      this.#chunks.length = Math.ceil(length / CHUNK);
      const last = this.#chunks[this.#chunks.length - 1];
      if (last && length % CHUNK) last.fill(0, length % CHUNK);
    }
    this.#size = length;
  }

  async flush() {}
}
const CHUNK = 1 << 20;

/** The root's file contents in a block store in the origin private file
 *  system, named `name`, with `cache` MiB of it (default 256) held in the
 *  kernel's memory: a root larger than the page's memory. A written block
 *  reaches the store within `writeback` ms (default 5000), or at a sync. The kernel reaches
 *  the store through a Worker of its own with a sync access handle; the
 *  store's methods reach the same file from the page, for a caller that
 *  uses them while no system holds it. */
export class OPFSFilesystem extends Filesystem {
  #name;

  constructor(sys, { name, cache, writeback } = {}) {
    name = String(name ?? '');
    if (!/^[A-Za-z0-9._-]{1,64}$/.test(name)) throw new TypeError('OPFSFilesystem: name the store ({ name }: letters, digits, . _ -)');
    super(sys, { cache, writeback });
    this.#name = name;
  }

  [FAST]() { return { kind: 'opfs', name: this.#name, cache: this[INTERNAL].cache(), writeback: this[INTERNAL].writeback(), block: BLOCK, inline: INLINE }; }

  async #file() {
    const root = await navigator.storage.getDirectory();
    const dir = await root.getDirectoryHandle(`gex-fs-${this.#name}`, { create: true });
    return dir.getFileHandle('blocks', { create: true });
  }

  async read(offset, length) {
    const f = await (await this.#file()).getFile();
    return new Uint8Array(await f.slice(offset, offset + length).arrayBuffer());
  }

  async write(offset, bytes) {
    const w = await (await this.#file()).createWritable({ keepExistingData: true });
    try {
      await w.write({ type: 'write', position: offset, data: bytes });
      await w.close();
    } catch (e) {
      await w.abort().catch(() => {});
      throw quota(e);
    }
  }

  async size() { return (await (await this.#file()).getFile()).size; }

  async truncate(length) {
    const w = await (await this.#file()).createWritable({ keepExistingData: true });
    try {
      await w.truncate(length);
      await w.close();
    } catch (e) {
      await w.abort().catch(() => {});
      throw quota(e);
    }
  }

  async flush() {}

  /** Forget what the store `name` keeps: the next boot on it starts from
   *  the sources alone. Not while a system holds it. */
  static async erase(name) {
    const root = await navigator.storage.getDirectory();
    // A system just stopped: its store Worker lets the file go a moment
    // after it was terminated.
    for (let i = 0; ; i++) {
      try {
        return await root.removeEntry(`gex-fs-${name}`, { recursive: true });
      } catch (e) {
        if (e?.name === 'NotFoundError') return;
        if (e?.name !== 'NoModificationAllowedError' || i >= 100) throw e;
        await new Promise((r) => setTimeout(r, 100));
      }
    }
  }

  /** {@link OPFSFilesystem.erase} of this filesystem's store: before boot,
   *  or once its system has stopped. */
  async erase() {
    const phase = this[INTERNAL].phase();
    if (phase !== 'new' && phase !== 'stopped' && phase !== 'failed') throw new Error(`OPFSFilesystem: erase() while the system is ${phase}`);
    await OPFSFilesystem.erase(this.#name);
  }
}

/** An OPFS failure with a code: the origin's quota is ENOSPC. */
const quota = (e) => Object.assign(e instanceof Error ? e : new Error(String(e)), { code: e?.name === 'QuotaExceededError' ? 'ENOSPC' : 'EIO' });
