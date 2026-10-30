// The runtime's Worker: the page's side of a running System.
//
// It holds an instance of the global kernel of its own (web/src/kernel-helper.js)
// and does, from outside every guest process, what the API asks for:
//
// - seeds the root with the boot tree, and holds the kernel's pid 1 when
//   the page names no init (below, "programs");
// - makes each program's process, its terminal or pipes (the page's ends
//   stay here), and the exec record its Worker starts from;
// - moves bytes between those ends and the page, waiting for them on a wait
//   slot of its own registered on each end's queue (shims/kernel wait.rs),
//   with `Atomics.waitAsync`, so a message from the page is never held up;
// - reads and writes the guest's files (`k_fs_*`).
//
// A Worker and not the page: the kernel's locks may wait, and the page's
// thread cannot.
//
// Messages in are calls, `{ call, op, ...args }`, answered `{ call, value }`
// or `{ call, error }`. Messages out without `call` are events:
// `{ ev: 'data', h, fd, bytes }`, `{ ev: 'end', h, fd }`,
// `{ ev: 'exit', pid, code, signal }`.

import { helperKernel } from '../../src/kernel-helper.js';

const port = globalThis.postMessage
  ? globalThis
  : (await import('node:worker_threads')).parentPort;
const post = (m, transfer) => port.postMessage(m, transfer ?? []);

const enc = new TextEncoder();
const dec = new TextDecoder();
const EAGAIN = -11;
const EIO = -5;
const ERANGE = -34;
// The pid the page's own ends are held under: no process has it.
const HOST = 0;
// Linux open flags.
/** `remote::xattr_op::SET`. */
const XATTR_SET = 1;
const O_WRONLY = 1, O_RDWR = 2, O_CREAT = 0o100, O_EXCL = 0o200, O_TRUNC = 0o1000, O_APPEND = 0o2000, O_DIRECTORY = 0o200000, O_NOFOLLOW = 0o400000;
const READ_MAX = 1 << 18;
const META_BYTES = 72;
const TYPES = ['file', 'directory', 'symlink', 'chardev', 'fifo', 'socket'];

let k = null;
let mem = null;
let kernelWords = null;
let slot = -1;

// -- scratch memory --------------------------------------------------------
// One call at a time runs here, so every call lays its arguments out from
// the bottom of the scratch memory and is done with them when it returns.

function ensure(n) {
  const have = mem.buffer.byteLength;
  if (n > have) mem.grow(Math.ceil((n - have) / 65536));
}
const view = () => new Uint8Array(mem.buffer);

/** Lay `parts` (strings or bytes) out one after another, 16-aligned:
 *  [[at, len], ...] and the end. */
function lay(...parts) {
  const bs = parts.map((p) => (typeof p === 'string' ? enc.encode(p) : p));
  let at = 0;
  const out = [];
  for (const b of bs) {
    out.push([at, b.length]);
    at = (at + b.length + 15) & ~15;
  }
  ensure(at + 16);
  const v = view();
  bs.forEach((b, i) => v.set(b, out[i][0]));
  return { at: out, end: at };
}

// -- files -----------------------------------------------------------------

function errno(n, what) {
  const names = { 111: 'ECONNREFUSED', 2: 'ENOENT', 13: 'EACCES', 17: 'EEXIST', 20: 'ENOTDIR', 21: 'EISDIR', 22: 'EINVAL', 39: 'ENOTEMPTY', 9: 'EBADF', 28: 'ENOSPC', 18: 'EXDEV', 40: 'ELOOP', 36: 'ENAMETOOLONG' };
  const code = names[-n] ?? `errno ${-n}`;
  return Object.assign(new Error(`${code}: ${what}`), { code, errno: -n });
}

/** `path` with its symlinks followed (the last too unless `last` is false),
 *  as the guest's own lookups follow them; `path` itself when the walk
 *  fails, for the call that follows to say why. */
function real(path, last = true) {
  const { at: [[p, n], [m, mn]], end } = lay(path, new Uint8Array(0));
  ensure(end + 4096);
  const r = k.k_fs_walk(BigInt(p), n, BigInt(m), mn, last ? 1 : 0, BigInt(end), 4096);
  if (r < 0) return path;
  const len = r & ((1 << 30) - 1);
  const out = new TextDecoder().decode(view().slice(end, end + len));
  return out.startsWith('/') ? out : `/${out}`;
}

function open(path, flags = 0, mode = 0) {
  const { at: [[p, n]] } = lay(real(path, !(flags & O_NOFOLLOW)));
  const h = k.k_fs_open(BigInt(p), n, BigInt(flags), mode);
  if (h < 0) throw errno(h, path);
  return h;
}

function metaAt(at) {
  const v = new DataView(mem.buffer, at, META_BYTES);
  const kind = TYPES[v.getUint32(0, true)] ?? 'file';
  const mode = v.getUint32(4, true);
  return {
    type: kind,
    mode: mode & 0o7777,
    size: Number(v.getBigUint64(8, true)),
    ino: Number(v.getBigUint64(16, true)),
    mtime: Number(v.getBigInt64(24, true)) * 1000 + Math.floor(v.getUint32(36, true) / 1e6),
    nlink: v.getUint32(32, true),
  };
}

/** `lstat` (`follow` false: a symlink is itself) or `stat`. */
function stat(path, follow = true) {
  const { at: [[p, n]], end } = lay(real(path, follow));
  ensure(end + META_BYTES);
  const r = k.k_fs_path_meta(BigInt(p), n, BigInt(end));
  if (r < 0) throw errno(r, path);
  return metaAt(end);
}

function readFile(path) {
  const h = open(path, 0);
  try {
    const parts = [];
    let total = 0;
    ensure(READ_MAX);
    for (;;) {
      const n = k.k_fs_read(h, BigInt(total), 0n, READ_MAX);
      if (n < 0) throw errno(n, path);
      if (n === 0) break;
      parts.push(view().slice(0, n));
      total += n;
    }
    const out = new Uint8Array(total);
    let at = 0;
    for (const p of parts) { out.set(p, at); at += p.length; }
    return out;
  } finally {
    k.k_fs_close(h);
  }
}

function writeFile(path, data, { mode = 0o644, append = false, exclusive = false } = {}) {
  const bytes = typeof data === 'string' ? enc.encode(data) : new Uint8Array(data);
  const flags = O_WRONLY | O_CREAT | (append ? O_APPEND : O_TRUNC) | (exclusive ? O_EXCL : 0);
  const h = open(path, flags, mode);
  try {
    let off = append ? stat(path).size : 0;
    for (let done = 0; done < bytes.length || (done === 0 && bytes.length === 0);) {
      if (!bytes.length) break;
      const piece = bytes.subarray(done, done + READ_MAX);
      ensure(piece.length);
      view().set(piece, 0);
      const n = k.k_fs_write(h, BigInt(off), 0n, piece.length);
      if (n < 0) throw errno(n, path);
      done += n;
      off += n;
    }
    k.k_fs_touch?.(h, BigInt(Math.floor(Date.now() / 1000)));
  } finally {
    k.k_fs_close(h);
  }
}

function readdir(path) {
  const h = open(path, O_DIRECTORY);
  try {
    for (let cap = 1 << 16; ; cap *= 4) {
      ensure(cap);
      const n = k.k_fs_read_dir(h, 0n, cap);
      if (n === ERANGE) continue;
      if (n < 0) throw errno(n, path);
      const b = view().slice(0, n);
      const v = new DataView(b.buffer);
      const out = [];
      for (let at = 0; at + 16 <= n;) {
        const len = v.getUint32(at, true);
        const name = dec.decode(b.subarray(at + 16, at + 16 + len));
        if (name !== '.' && name !== '..') out.push({ name, type: TYPES[v.getUint32(at + 4, true)] ?? 'file' });
        at += 16 + len;
      }
      return out;
    }
  } finally {
    k.k_fs_close(h);
  }
}

function mkdir(path, { recursive = false, mode = 0o755 } = {}) {
  const one = (p) => {
    const { at: [[a, n]] } = lay(p);
    return k.k_fs_mkdir(BigInt(a), n, mode);
  };
  if (!recursive) {
    const r = one(path);
    if (r < 0) throw errno(r, path);
    return;
  }
  let p = '';
  for (const part of path.split('/').filter(Boolean)) {
    p += `/${part}`;
    const r = one(p);
    if (r < 0 && r !== -17) throw errno(r, p);
  }
}

function rm(path, { recursive = false, force = false } = {}) {
  let st;
  try {
    st = stat(path, false);
  } catch (e) {
    if (force && e.errno === 2) return;
    throw e;
  }
  if (st.type === 'directory') {
    if (recursive) for (const { name } of readdir(path)) rm(`${path.replace(/\/+$/, '')}/${name}`, { recursive, force });
    const { at: [[a, n]] } = lay(path);
    const r = k.k_fs_rmdir(BigInt(a), n);
    if (r < 0) throw errno(r, path);
    return;
  }
  const { at: [[a, n]] } = lay(path);
  const r = k.k_fs_unlink(BigInt(a), n);
  if (r < 0 && !(force && r === -2)) throw errno(r, path);
}

function rename(from, to) {
  const { at: [[a, an], [b, bn]] } = lay(from, to);
  const r = k.k_fs_rename(BigInt(a), an, BigInt(b), bn, 0);
  if (r < 0) throw errno(r, `${from} -> ${to}`);
}

function symlink(target, path) {
  const { at: [[a, an], [b, bn]] } = lay(target, path);
  const r = k.k_fs_symlink(BigInt(a), an, BigInt(b), bn);
  if (r < 0) throw errno(r, path);
}

function readlink(path) {
  const { at: [[a, an]], end } = lay(path);
  ensure(end + 4096);
  const r = k.k_fs_read_link(BigInt(a), an, BigInt(end), 4096);
  if (r < 0) throw errno(r, path);
  return dec.decode(view().slice(end, end + r));
}

function chmod(path, mode) {
  const { at: [[a, an]] } = lay(real(path));
  const r = k.k_fs_chmod(BigInt(a), an, mode);
  if (r < 0) throw errno(r, path);
}

/** A tree (web/src/tar.js `untar`'s shape) into the live filesystem:
 *  directories, files, modes, hard and symbolic links, lazy files. Over
 *  whatever is there. */
function addTree(t) {
  let files = 0, bytes = 0;
  const parent = (p) => p.slice(0, p.lastIndexOf('/')) || '/';
  const made = new Set();
  const dirOf = (p) => {
    const d = parent(p);
    if (made.has(d)) return;
    mkdir(d, { recursive: true });
    made.add(d);
  };
  for (const d of t.dirs ?? []) {
    mkdir(d, { recursive: true, mode: t.modes?.[d] ?? 0o755 });
    made.add(d);
  }
  for (const [p, data] of Object.entries(t.files ?? {})) {
    dirOf(p);
    const d = typeof data === 'string' ? enc.encode(data) : data;
    // Through `k_fs_add`, the way a bundle is seeded, with what is there
    // first unlinked (a symlink in the way would be written through).
    const { at: [[pp, pl], [dp, dl]] } = lay(p, d);
    k.k_fs_unlink(BigInt(pp), pl);
    const r = k.k_fs_add(BigInt(pp), pl, BigInt(dp), dl);
    if (r < 0) throw errno(r, p);
    files++;
    bytes += d.length;
  }
  for (const [p, m] of Object.entries(t.modes ?? {})) {
    try { chmod(p, m); } catch {}
  }
  for (const [p, target] of Object.entries(t.hard ?? {})) {
    dirOf(p);
    const { at: [[a, an], [b, bn]] } = lay(target, p);
    k.k_fs_unlink(BigInt(b), bn);
    k.k_fs_link(BigInt(a), an, BigInt(b), bn);
  }
  for (const [p, target] of Object.entries(t.links ?? {})) {
    dirOf(p);
    const { at: [[a, an], [b, bn]] } = lay(target, p);
    k.k_fs_unlink(BigInt(b), bn);
    k.k_fs_symlink(BigInt(a), an, BigInt(b), bn);
  }
  for (const [p, size, url] of t.lazy ?? []) {
    dirOf(p);
    const { at: [[a, an], [b, bn]] } = lay(p, url);
    k.k_fs_unlink(BigInt(a), an);
    k.k_fs_add_lazy(BigInt(a), an, BigInt(b), bn, BigInt(size));
    files++;
  }
  for (const [p, secs] of Object.entries(t.mtimes ?? {})) {
    const { at: [[a, an]] } = lay(p);
    k.k_fs_set_mtime?.(BigInt(a), an, BigInt(secs));
  }
  for (const [p, attrs] of Object.entries(t.xattrs ?? {})) {
    for (const [name, value] of Object.entries(attrs)) {
      const v = typeof value === 'string' ? enc.encode(value) : value;
      const { at: [[a, an], [b, bn], [c, cn]] } = lay(p, name, v);
      k.k_fs_xattr(-1, BigInt(a), an, XATTR_SET, BigInt(b), bn, BigInt(c), cn, 0);
    }
  }
  return { files, bytes };
}

/** The tree the system boots with (the Filesystem's layers, runtime.js's
 *  `files`, `dirs`, ... options), into the empty root, in gex.mjs `load`'s
 *  order: empty directories, files, directories' modes, lazy files, hard
 *  links, symbolic links, times, extended attributes. Seeding, not a
 *  program's writes (a persist diff does not take it). */
function seed({ files = {}, dirs = [], modes = {}, hard = {}, symlinks = {}, lazy = [], mtimes = {}, xattrs = {} }) {
  const PART = 16 << 20;
  for (const d of dirs) {
    const { at: [[a, an]] } = lay(d);
    k.k_fs_add_dir(BigInt(a), an, 0o755);
  }
  const chmodRaw = (p, m) => {
    const { at: [[a, an]] } = lay(p);
    k.k_fs_chmod(BigInt(a), an, m);
  };
  for (const [p, data] of Object.entries(files)) {
    const d = typeof data === 'string' ? enc.encode(data) : data;
    // A large file in parts, so this Worker's memory never holds a whole one.
    const first = d.subarray(0, PART);
    const { at: [[pp, pl], [dp, dl]] } = lay(p, first);
    k.k_fs_add(BigInt(pp), pl, BigInt(dp), dl);
    for (let off = PART; off < d.length; off += PART) {
      const piece = d.subarray(off, off + PART);
      const { at: [[qp, ql], [ep, el]] } = lay(p, piece);
      k.k_fs_add_part(BigInt(qp), ql, BigInt(off), BigInt(ep), el);
    }
    if (modes[p] !== undefined) chmodRaw(p, modes[p]);
  }
  for (const d of dirs) if (modes[d] !== undefined) chmodRaw(d, modes[d]);
  for (const [p, size, url, head] of lazy) {
    // The bundle's file wins, unless it is the lazy file's head.
    if (files[p] !== undefined && !(head && files[p].length === head)) continue;
    const { at: [[a, an], [b, bn]] } = lay(p, url);
    k.k_fs_add_lazy(BigInt(a), an, BigInt(b), bn, BigInt(size));
    if (modes[p] !== undefined) chmodRaw(p, modes[p]);
  }
  for (const [p, target] of Object.entries(hard)) {
    const { at: [[a, an], [b, bn]] } = lay(target, p);
    k.k_fs_link(BigInt(a), an, BigInt(b), bn);
    if (modes[target] !== undefined) chmodRaw(p, modes[target]);
  }
  for (const [p, target] of Object.entries(symlinks)) {
    const { at: [[a, an], [b, bn]] } = lay(target, p);
    k.k_fs_symlink(BigInt(a), an, BigInt(b), bn);
  }
  for (const [p, secs] of Object.entries(mtimes ?? {})) {
    const { at: [[a, an]] } = lay(p);
    k.k_fs_set_mtime(BigInt(a), an, BigInt(Math.trunc(secs)));
  }
  for (const [p, attrs] of Object.entries(xattrs ?? {})) {
    for (const [name, value] of Object.entries(attrs)) {
      const v = typeof value === 'string' ? enc.encode(value) : value;
      const { at: [[a, an], [b, bn], [c, cn]] } = lay(p, name, v);
      k.k_fs_xattr(-1, BigInt(a), an, XATTR_SET, BigInt(b), bn, BigInt(c), cn, 0);
    }
  }
  k.k_fs_populate_done();
}

// -- programs --------------------------------------------------------------
//
// Pid 1 is the kernel's own when the page names no init (`k_init_virtual`):
// in the process table (proc.rs `INIT_PID`) and nowhere else, no Worker and
// no program. A program the page starts is its child, in a session of its
// own, and this Worker collects it as pid 1 when it ends, for the page; an
// orphan is given to pid 1, as on Linux, and the kernel lets it go as it
// ends. When the page names an init, that program is pid 1, the child of
// nobody (parent 0), and the page starts nothing else.

const INIT = 1;
/** The page names an init: pid 1 is a program. */
let realInit = false;
/** Each program's ends, by handle: { pty, stdin, stdout, stderr, pid } */
const handles = new Map();
/** The page's programs, pid -> handle. */
const programs = new Map();

function watch(queue) {
  if (queue) k.k_wq_add(queue, slot);
}
function unwatch(queue) {
  if (queue) k.k_wq_del(queue, slot);
}

/** The program `file` names: itself with a slash in it (from `cwd` when
 *  relative), else the first executable of that name on `env`'s PATH (or
 *  the usual one). A negative errno when there is none. */
function findProgram(file, env, cwd) {
  const usable = (p) => {
    try {
      const st = stat(p);
      if (st.type !== 'file') return -13;
      return st.mode & 0o111 ? 0 : -13;
    } catch (e) {
      return -(e.errno ?? 2);
    }
  };
  if (file.includes('/')) {
    const p = file.startsWith('/') ? file : `${(cwd || '/').replace(/\/+$/, '')}/${file}`;
    const r = usable(p);
    return r < 0 ? r : p;
  }
  const kv = env.find((e) => e.startsWith('PATH='));
  const path = kv ? kv.slice(5) : '/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin';
  let best = -2;
  for (const d of path.split(':')) {
    const p = d ? `${d.replace(/\/+$/, '')}/${file}` : file;
    const r = usable(p);
    if (r === 0) return p;
    if (r === -13) best = -13;
  }
  return best;
}

/** `len` bytes of the file `path` at `off`, fewer at its end; null when it
 *  cannot be read. */
function readAt(path, off, len) {
  let h;
  try {
    h = open(path, 0);
  } catch {
    return null;
  }
  try {
    ensure(len);
    const n = k.k_fs_read(h, BigInt(off), 0n, len);
    return n < 0 ? null : view().slice(0, n);
  } finally {
    k.k_fs_close(h);
  }
}

/** A native wasm program's image span `[lowest, highest)`, when the file at
 *  `path` is one that runs no translated code, for its memory (the shim's
 *  vfork.rs `exec_wasm_image`, read here from the file): an ELF with
 *  `GEXWASM` 0x40 bytes below its entry, in a loaded segment, and no
 *  Windows address space asked for in the header's flags. Else null. */
function wasmImage(path) {
  const head = readAt(path, 0, 64);
  if (!head || head.length < 64 || head[0] !== 0x7f || head[1] !== 0x45 || (head[4] !== 1 && head[4] !== 2)) return null;
  const e64 = head[4] === 2;
  const word = (dv, o) => (e64 ? Number(dv.getBigUint64(o, true)) : dv.getUint32(o, true));
  const hv = new DataView(head.buffer);
  const at = word(hv, 24) - 0x40;
  const phoff = word(hv, e64 ? 32 : 28), phent = hv.getUint16(e64 ? 54 : 42, true), phnum = hv.getUint16(e64 ? 56 : 44, true);
  if (phent < (e64 ? 56 : 32) || !phnum || phnum > 64) return null;
  const ph = readAt(path, phoff, phent * phnum);
  if (!ph || ph.length < phent * phnum) return null;
  const pv = new DataView(ph.buffer);
  let lo = Infinity, hi = 0, flags = null;
  for (let i = 0; i < phnum; i++) {
    const p = i * phent;
    if (pv.getUint32(p, true) !== 1) continue;
    const off = word(pv, p + (e64 ? 8 : 4)), vaddr = word(pv, p + (e64 ? 16 : 8));
    const filesz = word(pv, p + (e64 ? 32 : 16)), memsz = word(pv, p + (e64 ? 40 : 20));
    lo = Math.min(lo, vaddr);
    hi = Math.max(hi, vaddr + memsz);
    if (flags === null && at >= vaddr && at + 8 <= vaddr + filesz) {
      const h = readAt(path, off + (at - vaddr), 40);
      if (!h || h.length < 8 || dec.decode(h.subarray(0, 7)) !== 'GEXWASM' || h[7] !== 0) return null;
      flags = h.length >= 40 ? new DataView(h.buffer).getUint32(36, true) : 0;
    }
  }
  return flags !== null && (flags & 3) === 0 ? [lo, hi] : null;
}

/** Make a program's process: its pid (the page's child, leading a session
 *  of its own; pid 1 for the page's init), its descriptors held for it in
 *  the kernel, and the exec record the page starts its Worker from (host.mjs
 *  `spawnExec`). */
function spawn({ h, argv, env = [], cwd = '', stdio = 'pipe', cols = 80, rows = 24, init = false }) {
  const prog = findProgram(argv[0], env, cwd);
  if (typeof prog === 'number') return { h, error: -prog };
  const pid = init ? INIT : k.k_spawn(INIT);
  if (!init) k.k_setsid(pid);
  const ends = { h, pty: -1, ptyReading: false, stdin: -1, stdout: -1, stderr: -1, writes: [], closeIn: false, pid };
  const fds = [];
  if (stdio === 'pty') {
    const id = k.k_pty_open_master(HOST) >> 16;
    k.k_pty_set_lock(id, 0);
    k.k_pty_set_winsize(id, (BigInt(rows & 0xffff) << 48n) | (BigInt(cols & 0xffff) << 32n));
    for (const fd of [0, 1, 2]) {
      k.k_pty_open_end(id, 0, pid);
      fds.push(`${fd} 0 ${O_RDWR} 0 pty:[${id}:s]`);
    }
    // Its controlling terminal, with its group in the foreground, as a
    // session leader's open of the slave makes it (syscalls.rs `open_pty`).
    k.k_pty_set_sid(id, pid);
    k.k_pty_set_fg(id, pid);
    ends.pty = id;
    ends.ptyReading = true;
    watch(k.k_pty_queue(id));
  } else if (stdio === 'null') {
    for (const fd of [0, 1, 2]) fds.push(`${fd} 0 ${O_RDWR} 0 /dev/null`);
  } else {
    // Three pipes. The far end's first reference becomes the program's;
    // this side keeps the near one.
    for (const [fd, mine] of [[0, 'stdin'], [1, 'stdout'], [2, 'stderr']]) {
      const id = k.k_pipe_new(HOST);
      const childWrites = fd !== 0 ? 1 : 0;
      k.k_pipe_open_end(id, childWrites, pid);
      k.k_pipe_close_end(id, childWrites, HOST);
      fds.push(`${fd} 0 ${childWrites ? O_WRONLY : 0} 0 pipe:[${id}]`);
      ends[mine] = id;
      watch(k.k_pipe_queue(id));
    }
  }
  handles.set(h, ends);
  programs.set(pid, h);
  // vfork.rs `stage_exec_parts`: the path, then the arguments and the
  // environment, each after its length.
  const exec = enc.encode([prog, String(argv.length), ...argv, String(env.length), ...env].map((x) => `${x}\0`).join(''));
  pump();
  return { h, pid, exec, fds: enc.encode(`${fds.join('\n')}\n`), cwd: cwd || '/', wasmImage: wasmImage(prog) };
}

/** A program whose Worker never started: what was held for it let go,
 *  and its exit (127, as an exec that failed) collected as any. */
function abandon({ pid }) {
  k.k_pipe_forget(pid);
  k.k_pty_forget(pid);
  k.k_exit(pid, 127);
}

/** Every program of the page's that has ended, collected: pid 1's children
 *  under the kernel's init, the init itself under a real one. */
function reap() {
  let moved = false;
  for (;;) {
    const r = realInit ? k.k_wait(0, INIT, 0) : k.k_wait(INIT, -1, 0);
    if (!r) break;
    moved = true;
    const pid = Number(r >> 32n), st = Number(r & 0xffffffffn);
    const h = programs.get(pid);
    programs.delete(pid);
    // What it wrote before it ended goes first.
    const e = h === undefined ? null : handles.get(h);
    if (e) pump1(e);
    const sig = st & 0x7f;
    post({ ev: 'exit', pid, code: sig ? 128 + sig : (st >> 8) & 0xff, signal: sig });
  }
  return moved;
}

/** A connection of the page's to what a program listens on at
 *  127.0.0.1:`port` (the global kernel's name for the port, net.rs
 *  `tcp_name`): its bytes moved as a program's pipes are. */
function connectGuest({ h, port }) {
  const { at: [[p, n]] } = lay(`\0gex-tcp:${port}`);
  const id = k.k_sock_connect(BigInt(p), n);
  if (id < 0) throw errno(id === -2 ? -111 : id, `127.0.0.1:${port}`);
  const ends = { h, pty: -1, stdin: -1, stdout: -1, stderr: -1, sock: id, writes: [], closeIn: false, pid: 0 };
  handles.set(h, ends);
  watch(k.k_sock_queue(id));
  pump();
  return true;
}

function closeEnds(e) {
  if (e.sock >= 0) {
    unwatch(k.k_sock_queue(e.sock));
    k.k_sock_close(e.sock);
    e.sock = -1;
  }
  if (e.pty >= 0) {
    if (e.ptyReading) unwatch(k.k_pty_queue(e.pty));
    k.k_pty_close_end(e.pty, 1, HOST);
    e.pty = -1;
  }
  if (e.stdin >= 0) {
    unwatch(k.k_pipe_queue(e.stdin));
    k.k_pipe_close_end(e.stdin, 1, HOST);
    e.stdin = -1;
  }
  for (const f of ['stdout', 'stderr']) {
    if (e[f] >= 0) {
      unwatch(k.k_pipe_queue(e[f]));
      k.k_pipe_close_end(e[f], 0, HOST);
      e[f] = -1;
    }
  }
}

function write(h, bytes) {
  const e = handles.get(h);
  if (!e) return;
  e.writes.push(bytes);
  pump();
}

function closeStdin(h) {
  const e = handles.get(h);
  if (!e) return;
  e.closeIn = true;
  pump();
}

function resize(h, cols, rows) {
  const e = handles.get(h);
  if (!e || e.pty < 0) return;
  k.k_pty_set_winsize(e.pty, (BigInt(rows & 0xffff) << 48n) | (BigInt(cols & 0xffff) << 32n));
}

function kill(pid, signal) {
  const r = k.k_kill(INIT, pid, signal);
  if (r < 0 && r !== -3) throw errno(r, `kill ${pid}`);
}

/** Handles done with: ends closed, the page told. */
function release(h) {
  const e = handles.get(h);
  if (!e) return;
  closeEnds(e);
  handles.delete(h);
}

// -- watches ---------------------------------------------------------------

/** One inotify instance for every watch the page has, and each watch's
 *  descriptor by handle and back. */
const watches = { id: -1, byWd: new Map(), byH: new Map(), next: 1 };
const IN = { MODIFY: 0x2, ATTRIB: 0x4, CLOSE_WRITE: 0x8, MOVED_FROM: 0x40, MOVED_TO: 0x80, CREATE: 0x100, DELETE: 0x200, DELETE_SELF: 0x400, MOVE_SELF: 0x800, IGNORED: 0x8000 };
const WATCH_MASK = IN.ATTRIB | IN.CLOSE_WRITE | IN.MOVED_FROM | IN.MOVED_TO | IN.CREATE | IN.DELETE | IN.DELETE_SELF | IN.MOVE_SELF;

function addWatch(path) {
  if (watches.id < 0) {
    const id = k.k_inotify_create();
    if (id < 0) throw errno(id, 'inotify');
    watches.id = id;
    watch(k.k_inotify_queue(id));
  }
  const { at: [[p, n]] } = lay(real(path));
  const wd = k.k_inotify_add_watch(watches.id, BigInt(p), n, WATCH_MASK);
  if (wd < 0) throw errno(wd, path);
  const h = watches.next++;
  // Two watches on one path share a descriptor, as inotify's do.
  const hs = watches.byWd.get(wd) ?? new Set();
  hs.add(h);
  watches.byWd.set(wd, hs);
  watches.byH.set(h, wd);
  return h;
}

function rmWatch(h) {
  const wd = watches.byH.get(h);
  if (wd === undefined) return;
  watches.byH.delete(h);
  const hs = watches.byWd.get(wd);
  hs?.delete(h);
  if (hs && !hs.size) {
    watches.byWd.delete(wd);
    k.k_inotify_rm_watch(watches.id, wd);
  }
}

/** The events queued, to the page. True if there were any. */
function readWatches() {
  if (watches.id < 0 || k.k_inotify_queued(watches.id) <= 0) return false;
  ensure(65536);
  const n = k.k_inotify_read(watches.id, 0n, 65536);
  if (n <= 0) return false;
  const b = view().slice(0, n);
  const d = new DataView(b.buffer);
  for (let at = 0; at + 16 <= n;) {
    const wd = d.getInt32(at, true), mask = d.getUint32(at + 4, true), cookie = d.getUint32(at + 8, true), len = d.getUint32(at + 12, true);
    const raw = b.subarray(at + 16, at + 16 + len);
    const z = raw.indexOf(0);
    const name = new TextDecoder().decode(z < 0 ? raw : raw.subarray(0, z));
    at += 16 + len;
    const type = mask & IN.CREATE ? 'create'
      : mask & IN.CLOSE_WRITE ? 'change'
      : mask & IN.ATTRIB ? 'attrib'
      : mask & IN.DELETE ? 'delete'
      : mask & IN.MOVED_FROM ? 'move-out'
      : mask & IN.MOVED_TO ? 'move-in'
      : mask & (IN.DELETE_SELF | IN.MOVE_SELF | IN.IGNORED) ? 'gone'
      : null;
    if (!type) continue;
    const hs = watches.byWd.get(wd);
    if (!hs) continue;
    for (const h of hs) post({ ev: 'watch', h, type, name, cookie });
    if (type === 'gone') {
      for (const h of hs) watches.byH.delete(h);
      watches.byWd.delete(wd);
    }
  }
  return true;
}

// -- X display :0 -----------------------------------------------------------

/** Display :0's names, held for the X server until a client comes: then the
 *  page is told (`x-wanted`) and starts it on these sockets. */
const xhold = { ids: [], asked: false };

function holdX() {
  for (const name of ['\0/tmp/.X11-unix/X0', '/tmp/.X11-unix/X0']) {
    const { at: [[p, n]] } = lay(name);
    const id = k.k_sock_bind(BigInt(p), n);
    if (id < 0) throw errno(id, `X display :0 (${JSON.stringify(name)})`);
    xhold.ids.push(id);
    watch(k.k_sock_queue(id));
  }
  return xhold.ids;
}

function xWanted() {
  if (xhold.asked || !xhold.ids.some((id) => k.k_sock_pending(id) > 0)) return false;
  xhold.asked = true;
  // The server watches them from now on.
  for (const id of xhold.ids) unwatch(k.k_sock_queue(id));
  post({ ev: 'x-wanted' });
  return true;
}

// -- the loop --------------------------------------------------------------

/** One pass over everything that can move without waiting. True if
 *  anything did. */
function pass() {
  let moved = readWatches() | xWanted();
  for (const e of handles.values()) moved = pump1(e) || moved;
  // Exits last: what a program wrote before it ended reaches the page
  // before its end does.
  return reap() || moved;
}

/** A connection's turn (`connectGuest`). */
function pumpSock(e) {
  let moved = false;
  for (;;) {
    const n = k.k_sock_read(e.sock, 0n, READ_MAX);
    if (n > 0) {
      const bytes = view().slice(0, n);
      post({ ev: 'data', h: e.h, fd: 1, bytes }, [bytes.buffer]);
      moved = true;
      continue;
    }
    if (n === EAGAIN) break;
    // The program closed it (0), or it failed.
    post({ ev: 'end', h: e.h, fd: 1 });
    closeEnds(e);
    handles.delete(e.h);
    return true;
  }
  while (e.writes.length) {
    const b = e.writes[0];
    const piece = b.subarray(0, READ_MAX);
    view().set(piece, 0);
    const n = k.k_sock_write(e.sock, 0n, piece.length);
    if (n === EAGAIN || n === 0) break;
    if (n < 0) {
      e.writes.length = 0;
      break;
    }
    moved = true;
    if (n < b.length) e.writes[0] = b.subarray(n);
    else e.writes.shift();
  }
  if (e.closeIn && !e.writes.length && !e.shut) {
    e.shut = true;
    k.k_sock_shutdown(e.sock, 1);
    moved = true;
  }
  return moved;
}

function pump1(e) {
  let moved = false;
  ensure(READ_MAX);
  if (e.sock >= 0) return pumpSock(e);
  // Output: the terminal's master, or the two pipes.
  const outs = e.pty >= 0 ? (e.ptyReading ? [['pty', 1]] : []) : [['stdout', 1], ['stderr', 2]];
  for (const [f, fd] of outs) {
    const id = e[f];
    if (id < 0) continue;
    for (;;) {
      const n = f === 'pty' ? k.k_pty_read(id, 1, 0n, READ_MAX) : k.k_pipe_read(id, 0n, READ_MAX);
      if (n > 0) {
        const bytes = view().slice(0, n);
        post({ ev: 'data', h: e.h, fd, bytes }, [bytes.buffer]);
        moved = true;
        continue;
      }
      if (n === EAGAIN) break;
      // End of file (0), or the terminal hung up (EIO). A terminal's master
      // stays held for the page's writes and size until it lets go.
      if (f === 'pty') {
        unwatch(k.k_pty_queue(id));
        e.ptyReading = false;
      } else {
        unwatch(k.k_pipe_queue(id));
        k.k_pipe_close_end(id, 0, HOST);
        e[f] = -1;
      }
      post({ ev: 'end', h: e.h, fd });
      moved = true;
      break;
    }
  }
  // Input.
  while (e.writes.length) {
    const b = e.writes[0];
    if (e.pty >= 0) {
      const { at: [[a]] } = lay(b);
      k.k_pty_write(e.pty, 1, BigInt(a), b.length);
      e.writes.shift();
      moved = true;
      continue;
    }
    if (e.stdin < 0) {
      e.writes.length = 0;
      break;
    }
    const piece = b.subarray(0, READ_MAX);
    ensure(piece.length);
    view().set(piece, 0);
    const n = k.k_pipe_write(e.stdin, 0n, piece.length);
    if (n === EAGAIN) break;
    if (n < 0) {
      // Nobody reads it any more (EPIPE): what is left is dropped.
      e.writes.length = 0;
      break;
    }
    moved = true;
    if (n < b.length) e.writes[0] = b.subarray(n);
    else e.writes.shift();
  }
  if (e.closeIn && !e.writes.length && e.stdin >= 0) {
    unwatch(k.k_pipe_queue(e.stdin));
    k.k_pipe_close_end(e.stdin, 1, HOST);
    e.stdin = -1;
    moved = true;
  }
  return moved;
}

let kick = null;
let looping = false;
/** Something to do from the page: the loop goes round now rather than at
 *  its next wake. */
function pump() {
  if (kick) {
    const f = kick;
    kick = null;
    f();
  }
}

async function loop() {
  if (looping) return;
  looping = true;
  const addr = (k.k_ws_addr(slot) >>> 0) >>> 2;
  for (;;) {
    const seq = k.k_ws_seq(slot);
    let moved;
    try {
      moved = pass();
    } catch (e) {
      post({ ev: 'log', text: `gex: the runtime's Worker: ${e?.stack ?? e}` });
      moved = false;
    }
    if (moved) continue;
    const kicked = new Promise((r) => { kick = r; });
    const w = Atomics.waitAsync(kernelWords, addr, seq, 30000);
    await (w.async ? Promise.race([w.value, kicked]) : Promise.resolve());
    kick = null;
  }
}

// -- persistence (persist.js) ----------------------------------------------
// What the programs changed in the root, as records a store keeps: the tree
// as seeded (`seed`, taken before the first program) against the tree
// now, both from the kernel's node list (`k_fs_tree`, no file's bytes), and
// against what the store holds (`known`: path -> stamp). A file's bytes are
// read only when the program wrote it and it changed since it was last
// stored, so a lazy file nobody wrote is never fetched for a snapshot.

const TREE_ENTRY = 40;
const KINDS = ['file', 'dir', 'symlink', 'socket'];
const persist = { on: false, seed: null, known: new Map(), changes: new Map(), exclude: [], saves: 0 };

function tree() {
  for (let cap = 1 << 20; ; ) {
    ensure(cap);
    const n = k.k_fs_tree(0n, cap);
    if (n === ERANGE) {
      cap = new DataView(mem.buffer).getUint32(0, true) + (1 << 16);
      continue;
    }
    if (n < 0) throw errno(n, 'the tree');
    const b = view().slice(0, n);
    const v = new DataView(b.buffer);
    const out = new Map();
    for (let at = 0; at + TREE_ENTRY <= n;) {
      const pl = v.getUint32(at + 32, true), tl = v.getUint32(at + 36, true);
      const path = `/${dec.decode(b.subarray(at + TREE_ENTRY, at + TREE_ENTRY + pl))}`;
      out.set(path, {
        kind: KINDS[b[at]] ?? 'file',
        written: b[at + 1] === 1,
        mode: v.getUint32(at + 4, true) & 0o7777,
        size: Number(v.getBigUint64(at + 8, true)),
        changes: v.getBigUint64(at + 16, true),
        mtime: Number(v.getBigInt64(at + 24, true)),
        target: tl ? dec.decode(b.subarray(at + TREE_ENTRY + pl, at + TREE_ENTRY + pl + tl)) : '',
      });
      at += TREE_ENTRY + pl + tl;
    }
    return out;
  }
}

const within = (p, at) => p === at || p.startsWith(at === '/' ? '/' : `${at}/`);
const excluded = (p) => p === '/' || persist.exclude.some((at) => within(p, at));
const parentOf = (p) => p.replace(/\/[^/]*$/, '') || '/';

/** FNV-1a over 32-bit words and the tail: a change detector, not a checksum. */
function hash(d) {
  let h = 0x811c9dc5;
  const words = new Uint32Array(d.buffer, d.byteOffset, d.byteOffset % 4 ? 0 : d.length >>> 2);
  for (let i = 0; i < words.length; i++) h = Math.imul(h ^ words[i], 0x01000193);
  for (let i = words.length * 4; i < d.length; i++) h = Math.imul(h ^ d[i], 0x01000193);
  return (h >>> 0).toString(16);
}

/** A file's bytes by its own name (a tree path: no link on it). */
function readRaw(path, size) {
  const { at: [[p, n]] } = lay(path);
  const h = k.k_fs_open(BigInt(p), n, 0n, 0);
  if (h < 0) return null;
  try {
    const out = new Uint8Array(size);
    ensure(READ_MAX);
    let off = 0;
    while (off < size) {
      const r = k.k_fs_read(h, BigInt(off), 0n, Math.min(READ_MAX, size - off));
      if (r <= 0) break;
      out.set(view().subarray(0, r), off);
      off += r;
    }
    return off === size ? out : out.slice(0, off);
  } finally {
    k.k_fs_close(h);
  }
}

/** Start keeping track: `known`, what the store holds ([path, stamp]);
 *  `exclude`, paths never kept. The seed is the tree now. */
function persistStart({ known = [], exclude = [] }) {
  persist.seed = tree();
  persist.known = new Map(known);
  persist.exclude = exclude;
  persist.on = true;
  if (k.k_fs_watch) {
    const { at: [[p, n]] } = lay('/');
    k.k_fs_watch(BigInt(p), n);
  }
  persist.saves = k.k_fs_saves ? k.k_fs_saves() : 0;
  return true;
}

/** Whether a program finished a change since the last ask (`k_fs_saves`). */
function persistChanged() {
  const n = k.k_fs_saves ? k.k_fs_saves() : 0;
  const moved = n !== persist.saves;
  persist.saves = n;
  return moved;
}

/** What changed since the store's last: `{ put: [record], drop: [path] }`;
 *  a record is `{ path, kind, stamp, mode, mtime, data, target }`, kind
 *  'file', 'dir', 'symlink', 'mode' (a seeded file's mode only) or 'gone'
 *  (a seeded path deleted, and everything under it). */
async function persistDiff() {
  if (!persist.on) throw new Error('persistence is not started');
  const cur = tree();
  const seed = persist.seed;
  const known = persist.known;
  const put = [];
  const keep = new Set();
  const transfer = [];
  let read = 0;
  const has = (p, stamp) => known.get(p) === stamp && keep.add(p);
  for (const [p, n] of cur) {
    if (excluded(p) || n.kind === 'socket') continue;
    const s = seed.get(p);
    if (n.kind === 'file') {
      const was = known.get(p);
      // A file restored from the store and chmod'ed since: stored again.
      const rewrite = was?.startsWith('f:') && !was.startsWith(`f:${n.mode.toString(8)}:`);
      if (n.written || rewrite) {
        if (!rewrite && known.has(p) && persist.changes.get(p) === n.changes) { keep.add(p); continue; }
        const data = readRaw(p, n.size);
        if (!data) continue;
        const stamp = `f:${n.mode.toString(8)}:${data.length}:${hash(data)}`;
        persist.changes.set(p, n.changes);
        if (has(p, stamp)) continue;
        put.push({ path: p, kind: 'file', stamp, mode: n.mode, mtime: n.mtime, data });
        transfer.push(data.buffer);
        // Let the page's messages through between big reads.
        if ((read += data.length) > 8 << 20) { read = 0; await new Promise((r) => setTimeout(r, 0)); }
      } else if (was?.startsWith('f:')) {
        keep.add(p);
      } else {
        const stamp = `m:${n.mode.toString(8)}`;
        if (has(p, stamp)) continue;
        if (s?.kind === 'file' && s.mode !== n.mode) put.push({ path: p, kind: 'mode', stamp, mode: n.mode });
      }
    } else if (n.kind === 'dir') {
      const stamp = `d:${n.mode.toString(8)}`;
      if (has(p, stamp)) continue;
      if (s?.kind === 'dir' && s.mode === n.mode) continue;
      put.push({ path: p, kind: 'dir', stamp, mode: n.mode, mtime: n.mtime });
    } else if (n.kind === 'symlink') {
      const stamp = `l:${n.target}`;
      if (has(p, stamp)) continue;
      if (s?.kind === 'symlink' && s.target === n.target) continue;
      put.push({ path: p, kind: 'symlink', stamp, target: n.target });
    }
  }
  // Deleted: the topmost of what is gone, once.
  for (const p of seed.keys()) {
    if (cur.has(p) || excluded(p)) continue;
    const up = parentOf(p);
    if (up !== '/' && !cur.has(up) && seed.has(up)) continue;
    if (!has(p, 'gone')) put.push({ path: p, kind: 'gone', stamp: 'gone' });
  }
  // Deleted before this load: still gone.
  for (const [p, stamp] of known) if (stamp === 'gone' && !cur.has(p)) keep.add(p);
  const putting = new Set(put.map((r) => r.path));
  const drop = [...known.keys()].filter((p) => !keep.has(p) && !putting.has(p));
  return { value: { put, drop }, transfer };
}

/** The store took a diff: what it now holds. */
function persistStored({ put = [], drop = [] }) {
  for (const p of drop) {
    persist.known.delete(p);
    persist.changes.delete(p);
  }
  for (const [p, stamp] of put) persist.known.set(p, stamp);
  return true;
}

// -- the page's mounts (web/runtime fs.mount, shims/kernel pagefs.rs) -------

function mount(path, id) {
  const { at: [[a, an]] } = lay(path);
  const r = k.k_pagefs_mount(BigInt(a), an, id);
  if (r < 0) throw errno(r, `mounting ${path}`);
  return true;
}

function unmount(path) {
  const { at: [[a, an]] } = lay(path);
  const r = k.k_pagefs_unmount(BigInt(a), an);
  if (r < 0) throw errno(r, `unmounting ${path}`);
  return true;
}

// -- calls -----------------------------------------------------------------

const ops = {
  async start({ spec, tree, mounts = [], persist: keep = null, init = false }) {
    // The first instance of the kernel there is: on its linked stack (the
    // one no process's Worker is given).
    ({ k, mem } = await helperKernel(spec, 0, 32));
    kernelWords = new Int32Array(spec.memory.buffer);
    seed(tree);
    // The page's mounts made before boot, in place before the first program.
    for (const [path, id] of mounts) mount(path, id);
    // The page answers host calls (System `hostCalls`): every process gets a
    // /dev/gex-host.
    k.k_host_calls_serve();
    // The tree as seeded, before any program of the tree's runs.
    if (keep) persistStart(keep);
    slot = k.k_ws_alloc();
    if (slot < 0) throw new Error('the runtime found no free wait slot in the kernel');
    // Pid 1: the page's init, started next, or the kernel's own, which
    // /proc/1 calls init.
    realInit = init;
    if (!init) {
      k.k_init_virtual(1);
      const { at: [[a, an]] } = lay('init\ninit\0');
      k.k_proc_set_info(INIT, BigInt(a), an);
    }
    // Moved when a child of pid 1 ends, and when pid 1 itself does.
    watch(k.k_proc_queue(INIT));
    loop();
    // Where the kernel's stacks are, for the host to hand out (gex.mjs
    // `kernelStacks`).
    return {
      area: k.k_stack_area() >>> 0, size: k.k_stack_size() >>> 0, count: k.k_stack_count() >>> 0,
      ...(k.k_snap_word ? { snapWord: k.k_snap_word() >>> 0, wsAddr: k.k_ws_addr(0) >>> 0, wsCount: k.k_ws_count() >>> 0 } : {}),
      ...(k.k_mono_held_at ? { monoHeld: k.k_mono_held_at() >>> 0 } : {}),
      ...(k.k_snap_extents_at ? { extents: k.k_snap_extents_at() >>> 0 } : {}),
    };
  },
  abandon,
  spawn,
  write: ({ h, bytes }) => write(h, bytes),
  closeStdin: ({ h }) => closeStdin(h),
  resize: ({ h, cols, rows }) => resize(h, cols, rows),
  kill: ({ pid, signal }) => kill(pid, signal),
  release: ({ h }) => release(h),
  readFile: ({ path }) => readFile(path),
  writeFile: ({ path, data, opts }) => writeFile(path, data, opts),
  stat: ({ path, follow }) => stat(path, follow),
  readdir: ({ path }) => readdir(path),
  mkdir: ({ path, opts }) => mkdir(path, opts),
  rm: ({ path, opts }) => rm(path, opts),
  rename: ({ from, to }) => rename(from, to),
  symlink: ({ target, path }) => symlink(target, path),
  readlink: ({ path }) => readlink(path),
  chmod: ({ path, mode }) => chmod(path, mode),
  addTree: ({ tree }) => addTree(tree),
  watch: ({ path }) => addWatch(path),
  holdX: () => holdX(),
  connectGuest: (m) => connectGuest(m),
  // Where `/dev/dsp`'s rings are (shims/kernel/src/audio.rs), made if not yet.
  audioRing: () => (k.k_audio_ring ? k.k_audio_ring() >>> 0 : 0),
  // The game controllers' table (shims/kernel/src/joy.rs), and a change in
  // it, which wakes the processes reading /dev/input.
  joyTable: () => (k.k_joy_table ? k.k_joy_table() >>> 0 : 0),
  joyNotify: () => { k.k_joy_notify?.(); return 0; },
  mount: ({ path, id }) => mount(path, id),
  persistStart: (m) => persistStart(m),
  persistChanged: () => persistChanged(),
  persistDiff: ({ exclude }) => { if (exclude) persist.exclude = exclude; return persistDiff(); },
  persistStored: (m) => persistStored(m),
  unmount: ({ path }) => unmount(path),
  unwatch: ({ h }) => rmWatch(h),
};

const onMessage = async (m) => {
  if (m?.call === undefined) return;
  try {
    let value = await ops[m.op](m);
    let transfer = value instanceof Uint8Array ? [value.buffer] : [];
    // A call whose answer moves buffers says which: { value, transfer }.
    if (m.op === 'persistDiff') ({ value, transfer } = value);
    post({ call: m.call, value }, transfer);
  } catch (e) {
    post({ call: m.call, error: { message: e?.message ?? String(e), code: e?.code, errno: e?.errno, stack: e?.stack } });
  }
};
if (port === globalThis) globalThis.onmessage = (e) => onMessage(e.data);
else port.on('message', onMessage);
