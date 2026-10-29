// `Filesystem`: the guest's root, one per system. Before boot it is the tars
// and files a page adds, laid one over another in the order added; pid 1
// seeds the kernel's root with them (web/src/runtime.js `files`, `lazy`, ...).
// After boot, a tar or files are written into the live tree, and the guest's
// files can be read and written from the page.

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

async function decodeTar(src) {
  let bytes = await bytesOf(src, 'tar');
  if (isZstd(bytes)) {
    const { unzstd } = await import('../../src/zfetch.js');
    bytes = await unzstd(bytes);
  }
  const { untar } = await import('../../src/tar.js');
  return untar(bytes);
}

/** A lazy list: `[[path, size, url], ...]`, or the URL of one as JSON (an
 *  ejected build's lazy.json: `{ base, files: [[path, size, rel], ...] }`
 *  too). A file's URL is a path on the page's origin. */
async function lazyList(lazy) {
  let list = lazy;
  let base = globalThis.location?.href;
  if (typeof lazy === 'string' || lazy instanceof URL) {
    base = new URL(lazy, globalThis.location?.href).href;
    const res = await fetch(base);
    if (!res.ok) throw new Error(`lazy list ${lazy}: ${res.status}`);
    list = await res.json();
  }
  if (!Array.isArray(list)) {
    if (list?.files) {
      const b = new URL(list.base ?? './', base);
      return list.files.map(([p, size, rel]) => [p, size, new URL(rel, b).href]);
    }
    throw new TypeError('lazy: a list of [path, size, url] or its URL');
  }
  return list.map(([p, size, url]) => [p, size, new URL(url, base).href]);
}

/** A same-origin URL as the path the kernel keeps for a lazy file. */
function lazyPath(href) {
  const u = new URL(href, globalThis.location?.href);
  if (globalThis.location && u.origin !== globalThis.location.origin) {
    throw new Error(`lazy file ${href}: only the page's own origin can be read lazily`);
  }
  return u.pathname + u.search + u.hash;
}

export class Filesystem extends EventTarget {
  #sys;
  #sources = [];

  constructor(sys) {
    super();
    const i = internals(sys, 'Filesystem');
    i.attach('filesystem', this, { beforeBoot: true });
    this.#sys = i;
    i.beforeBoot((opts) => this.#seed(opts));
    this[INTERNAL] = {
      sources: () => this.#sources,
    };
  }

  #io(what) {
    if (this.#sys.phase !== 'running') throw new Error(`${what}: the system is ${this.#sys.phase}, not running`);
    return this.#sys.io();
  }

  /**
   * A tar (bytes, a Blob, a stream, or a URL; zstd or not) laid over the
   * root at `at` (default '/'). Before boot it is one more layer; after, it
   * is written into the running tree. `lazy`: files fetched as they are
   * first read, `[[path, size, url], ...]` or the URL of such a list, laid
   * with the tar (`src` may then be null).
   */
  addTar(src, { at = '/', lazy } = {}) {
    const p = (async () => {
      const tree = src == null ? { files: {}, dirs: [], links: {}, hard: {}, modes: {} } : await decodeTar(src);
      const lz = lazy ? await lazyList(lazy) : [];
      return rebase(tree, norm(at), lz);
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
      return { files, dirs: [], links: {}, hard: {}, modes, lazy: [] };
    })();
    return this.#add(p);
  }

  #add(tree) {
    if (this.#sys.phase === 'new' || this.#sys.phase === 'booting') {
      this.#sources.push(tree);
      tree.catch(() => {});
      return tree.then(() => {});
    }
    return tree.then((t) => this.#io('addTar').call('addTree', { tree: t }));
  }

  /** The layers, in order, as runProgram's options: later wins. */
  async #seed(opts) {
    const files = opts.files, links = opts.symlinks, hard = opts.hard, modes = opts.modes;
    const lazy = new Map(opts.lazy.map((l) => [l[0], l]));
    const dirs = new Set(opts.dirs);
    const mtimes = {};
    const xattrs = {};
    let anyTime = false, anyX = false;
    for (const src of this.#sources) {
      const t = await src;
      const take = (p) => {
        delete files[p];
        delete links[p];
        delete hard[p];
        lazy.delete(p);
        delete xattrs[p];
      };
      for (const d of t.dirs ?? []) dirs.add(d);
      for (const [p, d] of Object.entries(t.files ?? {})) { take(p); files[p] = d; }
      for (const [p, l] of Object.entries(t.links ?? {})) { take(p); links[p] = l; }
      for (const [p, l] of Object.entries(t.hard ?? {})) { take(p); hard[p] = l; }
      for (const l of t.lazy ?? []) { take(l[0]); lazy.set(l[0], l); }
      Object.assign(modes, t.modes ?? {});
      if (t.mtimes && Object.keys(t.mtimes).length) { anyTime = true; Object.assign(mtimes, t.mtimes); }
      if (t.xattrs && Object.keys(t.xattrs).length) { anyX = true; Object.assign(xattrs, t.xattrs); }
    }
    // A hard link whose target a later layer replaced by something else
    // than a file links nothing.
    for (const [p, target] of Object.entries(hard)) if (!files[target]) delete hard[p];
    opts.dirs = [...dirs];
    opts.lazy = [...lazy.values()];
    if (anyTime) opts.mtimes = mtimes;
    if (anyX) opts.xattrs = { ...(opts.xattrs ?? {}), ...xattrs };
    // The sources' bytes are pid 1's now.
    this.#sources.length = 0;
  }

  // -- the running tree ----------------------------------------------------

  /** A file's bytes (`encoding`: its text). */
  async readFile(path, { encoding } = {}) {
    const b = await this.#io('readFile').call('readFile', { path: norm(path) });
    return encoding ? new TextDecoder(encoding).decode(b) : b;
  }

  /** Write a file, made if missing (`mode`, default 0644); `append`. */
  async writeFile(path, data, { mode, append, exclusive } = {}) {
    const bytes = typeof data === 'string' ? new TextEncoder().encode(data) : await bytesOf(data, path);
    return this.#io('writeFile').call('writeFile', { path: norm(path), data: bytes, opts: { mode, append, exclusive } });
  }

  /** `{type, mode, size, ino, mtime (ms), nlink}`; `follow: false` is lstat. */
  stat(path, { follow = true } = {}) {
    return this.#io('stat').call('stat', { path: norm(path), follow });
  }

  /** `[{name, type}]`, without `.` and `..`. */
  readdir(path) {
    return this.#io('readdir').call('readdir', { path: norm(path) });
  }

  mkdir(path, { recursive = false, mode = 0o755 } = {}) {
    return this.#io('mkdir').call('mkdir', { path: norm(path), opts: { recursive, mode } });
  }

  rm(path, { recursive = false, force = false } = {}) {
    return this.#io('rm').call('rm', { path: norm(path), opts: { recursive, force } });
  }

  rename(from, to) {
    return this.#io('rename').call('rename', { from: norm(from), to: norm(to) });
  }

  symlink(target, path) {
    return this.#io('symlink').call('symlink', { target, path: norm(path) });
  }

  readlink(path) {
    return this.#io('readlink').call('readlink', { path: norm(path) });
  }

  chmod(path, mode) {
    return this.#io('chmod').call('chmod', { path: norm(path), mode });
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
    this.#io('watch').call('watch', { path: at }).then((got) => {
      h = got;
      if (stopped) return i.io().call('unwatch', { h }).catch(() => {});
      this.#watching.set(h, each);
    }, (error) => fn({ type: 'error', path: at, error }));
    return () => {
      if (stopped) return;
      stopped = true;
      if (h !== null && this.#watching.delete(h)) i.io()?.call('unwatch', { h }).catch(() => {});
    };
  }
  #watching = null;
}

/** A tree's paths under `at`, its lazy files added. */
function rebase(t, at, lazy) {
  const re = (p) => under(at, p);
  const map = (o) => Object.fromEntries(Object.entries(o ?? {}).map(([p, v]) => [re(p), v]));
  const hard = Object.fromEntries(Object.entries(t.hard ?? {}).map(([p, v]) => [re(p), re(v)]));
  return {
    files: map(t.files),
    dirs: (t.dirs ?? []).map(re),
    links: map(t.links),
    hard,
    modes: map(t.modes),
    mtimes: t.mtimes && Object.keys(t.mtimes).length ? map(t.mtimes) : undefined,
    xattrs: t.xattrs && Object.keys(t.xattrs).length ? map(t.xattrs) : undefined,
    lazy: lazy.map(([p, size, url]) => [re(p), size, lazyPath(url)]),
  };
}
