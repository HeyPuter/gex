// The runtime's Worker: the page's side of a running System.
//
// It holds an instance of the global kernel of its own (web/src/kernel-helper.js)
// and does, from outside every guest process, what the API asks for:
//
// - starts programs, through pid 1 (web/runtime/init/gex-init.c), which
//   connects to the unix socket "\0gex-runtime" this Worker binds in the
//   kernel. A program's terminal is a pty made here (the master stays here);
//   its pipes are made here too and their far ends sent with the request;
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
const NAME = '\0gex-runtime';
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

// -- programs --------------------------------------------------------------

/** pid 1's connection: the listener, the connection, what is to be sent,
 *  the line being read. */
const init = { listener: -1, conn: -1, out: [], line: '', ready: null, readyResolve: null };
/** Requests waiting for pid 1's answer, by id. */
const asked = new Map();
/** Each program's ends, by handle: { pty, stdin, stdout, stderr, queue } */
const handles = new Map();

function watch(queue) {
  if (queue) k.k_wq_add(queue, slot);
}
function unwatch(queue) {
  if (queue) k.k_wq_del(queue, slot);
}

/** A frame for pid 1: a u32 length, a letter, NUL-terminated fields. */
function frame(letter, fields) {
  const body = enc.encode(letter + fields.map((f) => `${f}\0`).join(''));
  const out = new Uint8Array(4 + body.length);
  new DataView(out.buffer).setUint32(0, body.length, true);
  out.set(body, 4);
  return out;
}

/** Start a program: its ends made here, the request sent, pid 1's answer
 *  awaited. Requests go one at a time, so the descriptors one carries are
 *  never read with another's. */
let spawnChain = Promise.resolve();
function spawn(m) {
  const run = spawnChain.then(() => spawnNow(m));
  spawnChain = run.catch(() => {});
  return run;
}

async function spawnNow({ h, argv, env = [], cwd = '', uid = -1, gid = -1, stdio = 'pipe', cols = 80, rows = 24 }) {
  await init.ready;
  const ends = { h, pty: -1, ptyReading: false, stdin: -1, stdout: -1, stderr: -1, writes: [], closeIn: false, pid: 0 };
  let flags = 0, pts = 0;
  const descs = [];
  if (stdio === 'pty') {
    const r = k.k_pty_open_master(HOST);
    const id = r >> 16;
    k.k_pty_set_lock(id, 0);
    k.k_pty_set_winsize(id, (BigInt(rows & 0xffff) << 48n) | (BigInt(cols & 0xffff) << 32n));
    ends.pty = id;
    ends.ptyReading = true;
    pts = k.k_pty_ptn(id);
    flags = 1;
    watch(k.k_pty_queue(id));
  } else if (stdio === 'null') {
    flags = 2;
  } else {
    // Three pipes; the far end of each goes with the request (one more
    // holder while it is in flight), and this side lets its own copy of
    // that end go.
    for (const [fd, mine] of [[0, 'stdin'], [1, 'stdout'], [2, 'stderr']]) {
      const id = k.k_pipe_new(HOST);
      const childWrites = fd !== 0;
      k.k_pipe_open_end(id, childWrites ? 1 : 0, -1);
      k.k_pipe_close_end(id, childWrites ? 1 : 0, HOST);
      descs.push(`${childWrites ? O_WRONLY : 0} 0 pipe:[${id}]`);
      ends[mine] = id;
      watch(k.k_pipe_queue(id));
    }
  }
  handles.set(h, ends);
  const fields = [h, flags, uid ?? -1, gid ?? -1, cwd ?? '', pts, argv.length, ...argv, env.length, ...env];
  const answer = new Promise((resolve) => asked.set(h, resolve));
  init.out.push({ bytes: frame('S', fields), descs: descs.join('\n') });
  pump();
  const a = await answer;
  if (a.err) {
    closeEnds(ends);
    handles.delete(h);
    return { h, error: a.err };
  }
  ends.pid = a.pid;
  pump();
  return { h, pid: a.pid };
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
  init.out.push({ bytes: frame('K', [pid, signal]), descs: '' });
  pump();
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

// -- host calls (gex-host NAME JSON: gex-init.c host_call) ---------------------

const hostCalls = { listener: -1, conns: new Map(), next: 1 };

function listenHostCalls() {
  const { at: [[p, n]] } = lay('\0gex-host');
  const id = k.k_sock_bind(BigInt(p), n);
  if (id < 0) throw errno(id, 'the host-call socket');
  hostCalls.listener = id;
  watch(k.k_sock_queue(id));
}

/** New callers, and what they sent: a call to the page once whole. */
function passHostCalls() {
  let moved = false;
  if (hostCalls.listener < 0) return false;
  for (;;) {
    const c = k.k_sock_accept(hostCalls.listener);
    if (c < 0) break;
    hostCalls.conns.set(hostCalls.next++, { sock: c, buf: new Uint8Array(0), asked: false, out: null });
    watch(k.k_sock_queue(c));
    moved = true;
  }
  ensure(READ_MAX);
  for (const [id, c] of hostCalls.conns) {
    if (!c.asked) {
      for (;;) {
        const n = k.k_sock_read(c.sock, 0n, 65536);
        if (n === EAGAIN) break;
        if (n <= 0) { c.out = new Uint8Array(0); break; }
        const b = new Uint8Array(c.buf.length + n);
        b.set(c.buf);
        b.set(view().subarray(0, n), c.buf.length);
        c.buf = b;
        moved = true;
      }
      if (c.buf.length >= 4) {
        const len = new DataView(c.buf.buffer).getUint32(0, true);
        if (c.buf.length >= 4 + len) {
          const body = c.buf.subarray(4, 4 + len);
          const z = body.indexOf(0);
          c.asked = true;
          post({ ev: 'host-call', id, name: dec.decode(body.subarray(0, z < 0 ? body.length : z)), json: z < 0 ? '' : dec.decode(body.subarray(z + 1)) });
          moved = true;
        }
      }
    }
    if (c.out) {
      while (c.out.length) {
        const piece = c.out.subarray(0, READ_MAX);
        view().set(piece, 0);
        const n = k.k_sock_write(c.sock, 0n, piece.length);
        if (n === EAGAIN || n === 0) break;
        if (n < 0) { c.out = c.out.subarray(c.out.length); break; }
        c.out = c.out.subarray(n);
        moved = true;
      }
      if (!c.out.length) {
        unwatch(k.k_sock_queue(c.sock));
        k.k_sock_close(c.sock);
        hostCalls.conns.delete(id);
        moved = true;
      }
    }
  }
  return moved;
}

function hostReply({ id, ok, text }) {
  const c = hostCalls.conns.get(id);
  if (!c) return;
  const body = enc.encode(text);
  const out = new Uint8Array(5 + body.length);
  new DataView(out.buffer).setUint32(0, body.length, true);
  out[4] = ok ? 0 : 1;
  out.set(body, 5);
  c.out = out;
  pump();
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
  let moved = readWatches() | xWanted() | passHostCalls();
  if (init.conn < 0 && init.listener >= 0) {
    const c = k.k_sock_accept(init.listener);
    if (c >= 0) {
      init.conn = c;
      watch(k.k_sock_queue(c));
      moved = true;
    }
  }
  if (init.conn >= 0) {
    // pid 1's lines.
    ensure(READ_MAX);
    for (;;) {
      const n = k.k_sock_read(init.conn, 0n, 65536);
      if (n <= 0) {
        if (n === 0) post({ ev: 'init-gone' });
        break;
      }
      moved = true;
      init.line += dec.decode(view().slice(0, n));
      let nl;
      while ((nl = init.line.indexOf('\n')) >= 0) {
        heard(init.line.slice(0, nl));
        init.line = init.line.slice(nl + 1);
      }
    }
    // What is to go to it.
    while (init.out.length) {
      const o = init.out[0];
      const { at: [[a, an], [d, dn]] } = lay(o.bytes, o.descs);
      const n = o.descs ? k.k_sock_write_fds(init.conn, BigInt(a), an, BigInt(d), dn) : k.k_sock_write(init.conn, BigInt(a), an);
      if (n === EAGAIN || n === 0) break;
      if (n < 0) {
        post({ ev: 'log', text: `gex: the runtime could not write to pid 1 (${n})` });
        init.out.shift();
        continue;
      }
      moved = true;
      if (n < o.bytes.length) init.out[0] = { bytes: o.bytes.subarray(n), descs: '' };
      else init.out.shift();
    }
  }
  for (const e of handles.values()) moved = pump1(e) || moved;
  return moved;
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

function heard(line) {
  const [what, a, b, c] = line.split(' ');
  if (what === 'R') {
    init.readyResolve?.();
  } else if (what === 'P') {
    asked.get(Number(a))?.({ pid: Number(b) });
    asked.delete(Number(a));
  } else if (what === 'E') {
    asked.get(Number(a))?.({ err: Number(b) });
    asked.delete(Number(a));
  } else if (what === 'X') {
    post({ ev: 'exit', pid: Number(a), code: Number(b), signal: Number(c) });
  }
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
// as seeded (`seed`, taken before pid 1's first program) against the tree
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
  async start({ spec, stackTop, mounts = [], persist: keep = null }) {
    ({ k, mem } = await helperKernel(spec, stackTop, 32));
    kernelWords = new Int32Array(spec.memory.buffer);
    // The page's mounts made before boot, before pid 1 can connect (it runs
    // the tree's /etc/gex/boot once it has).
    for (const [path, id] of mounts) mount(path, id);
    // The tree as seeded, before any program of the tree's runs.
    if (keep) {
      while (k.k_fs_populated && !k.k_fs_populated()) await new Promise((r) => setTimeout(r, 5));
      persistStart(keep);
    }
    slot = k.k_ws_alloc();
    if (slot < 0) throw new Error('the runtime found no free wait slot in the kernel');
    listenHostCalls();
    const { at: [[a, an]] } = lay(NAME);
    init.listener = k.k_sock_bind(BigInt(a), an);
    if (init.listener < 0) throw errno(init.listener, 'binding the runtime socket');
    watch(k.k_sock_queue(init.listener));
    init.ready = new Promise((r) => { init.readyResolve = r; });
    loop();
    return true;
  },
  ready: () => init.ready.then(() => true),
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
  hostReply: (m) => hostReply(m),
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
