// The page's programs, from outside every guest process, through the kernel's
// exports (kernel.js): each one's process, its terminal or pipes (the page's
// ends held under pid 0, which no process has), the exec record its Worker
// starts from (host.mjs `spawnExec`), the bytes between those ends and the
// page, and its exit. Also what else the page waits on in the kernel: its
// file watches (inotify) and the X display it holds for the server.
//
// Pid 1 is the kernel's own when the page names no init (`k_init_virtual`):
// a program the page starts is its child, in a session of its own, collected
// here as pid 1 when it ends; an orphan is given to pid 1, as on Linux, and
// the kernel lets it go as it ends. With an init, that program is pid 1, the
// child of nobody (parent 0), and the page starts nothing else.
//
// One loop moves everything that can move, a batch of calls a pass, and
// sleeps on the kernel's wait queues (Atomics.waitAsync) when nothing can.

import { errno } from './kernel.js';

const enc = new TextEncoder();
const dec = new TextDecoder();
const EAGAIN = -11;
const INIT = 1;
/** The pid the page's own ends are held under: no process has it. */
const HOST = 0;
const O_WRONLY = 1, O_RDWR = 2;
const READ_MAX = 1 << 18;
const IN = { MODIFY: 0x2, ATTRIB: 0x4, CLOSE_WRITE: 0x8, MOVED_FROM: 0x40, MOVED_TO: 0x80, CREATE: 0x100, DELETE: 0x200, DELETE_SELF: 0x400, MOVE_SELF: 0x800, IGNORED: 0x8000 };
const WATCH_MASK = IN.ATTRIB | IN.CLOSE_WRITE | IN.MOVED_FROM | IN.MOVED_TO | IN.CREATE | IN.DELETE | IN.DELETE_SELF | IN.MOVE_SELF;

export class Programs {
  #k;
  #files;
  #on;
  /** Pid 1 is a program (`boot(argv)`). */
  #realInit = false;
  /** Each program's or connection's ends, by handle. */
  #handles = new Map();
  /** The page's programs, pid -> handle. */
  #programs = new Map();
  /** One inotify instance for every watch the page has. */
  #watches = { id: -1, byWd: new Map(), byH: new Map(), next: 1 };
  /** Display :0's sockets, held for the X server until a client comes. */
  #x = { ids: [], asked: false };
  #kick = null;
  /** Input the page gave a handle before its program's process was made:
   *  h -> { writes, closeIn }, taken up when it is. */
  #early = new Map();
  /** Asked to go round (`pump`) since the pass began. */
  #dirty = false;
  #looping = false;

  /** `on`: { data(h, fd, bytes), end(h, fd), exit({ pid, code, signal }),
   *  watch({ h, type, name, cookie }), xWanted(), log(text) }. */
  constructor(kernel, files, on) {
    this.#k = kernel;
    this.#files = files;
    this.#on = on;
  }

  /** At boot: pid 1 (a program, `init`, or the kernel's own). */
  async start({ init }) {
    this.#realInit = init;
    const list = [];
    if (!init) list.push({ f: 'k_init_virtual', a: [1] }, { f: 'k_proc_set_info', a: [INIT, { in: 'init\ninit\0' }] });
    list.push({ f: 'k_proc_queue', a: [INIT] });
    const res = await this.#k.calls(list);
    // Moved when a child of pid 1 ends, and when pid 1 itself does.
    await this.#k.watch(res[res.length - 1].r);
    this.#loop();
  }

  /** The program `file` names: itself with a slash in it (from `cwd` when
   *  relative), else the first executable of that name on `env`'s PATH
   *  (or the usual one). A negative errno when there is none. */
  async #find(file, env, cwd) {
    const usable = async (p) => {
      try {
        const st = await this.#files.stat(p);
        if (st.type !== 'file') return -13;
        return st.mode & 0o111 ? 0 : -13;
      } catch (e) {
        return -(e.errno ?? 2);
      }
    };
    if (file.includes('/')) {
      const p = file.startsWith('/') ? file : `${(cwd || '/').replace(/\/+$/, '')}/${file}`;
      const r = await usable(p);
      return r < 0 ? r : p;
    }
    const kv = env.find((e) => e.startsWith('PATH='));
    const path = kv ? kv.slice(5) : '/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin';
    let best = -2;
    for (const d of path.split(':')) {
      const p = d ? `${d.replace(/\/+$/, '')}/${file}` : file;
      const r = await usable(p);
      if (r === 0) return p;
      if (r === -13) best = -13;
    }
    return best;
  }

  /** `len` bytes of the file `path` at `off`, fewer at its end; null when
   *  it cannot be read. */
  async #readAt(path, off, len) {
    const [{ r: h }, rd] = await this.#k.calls([
      { f: 'k_fs_open', a: [{ in: path }, 0n, 0], stop: true },
      { f: 'k_fs_read', a: [{ ref: 0 }, BigInt(off), { out: len }] },
      { f: 'k_fs_close', a: [{ ref: 0 }] },
    ]);
    if (h < 0 || rd.r < 0) return null;
    return rd.out[0];
  }

  /** A native wasm program's image span `[lowest, highest)`, when the file
   *  at `path` is one that runs no translated code, for its memory (the
   *  shim's vfork.rs `exec_wasm_image`, read here from the file): an ELF
   *  with `GEXWASM` 0x40 bytes below its entry, in a loaded segment, and no
   *  Windows address space asked for in the header's flags. Else null. */
  async #wasmImage(path) {
    const head = await this.#readAt(path, 0, 64);
    if (!head || head.length < 64 || head[0] !== 0x7f || head[1] !== 0x45 || (head[4] !== 1 && head[4] !== 2)) return null;
    const e64 = head[4] === 2;
    const word = (dv, o) => (e64 ? Number(dv.getBigUint64(o, true)) : dv.getUint32(o, true));
    const hv = new DataView(head.buffer, head.byteOffset);
    const at = word(hv, 24) - 0x40;
    const phoff = word(hv, e64 ? 32 : 28), phent = hv.getUint16(e64 ? 54 : 42, true), phnum = hv.getUint16(e64 ? 56 : 44, true);
    if (phent < (e64 ? 56 : 32) || !phnum || phnum > 64) return null;
    const ph = await this.#readAt(path, phoff, phent * phnum);
    if (!ph || ph.length < phent * phnum) return null;
    const pv = new DataView(ph.buffer, ph.byteOffset);
    let lo = Infinity, hi = 0, flags = null;
    for (let i = 0; i < phnum; i++) {
      const p = i * phent;
      if (pv.getUint32(p, true) !== 1) continue;
      const off = word(pv, p + (e64 ? 8 : 4)), vaddr = word(pv, p + (e64 ? 16 : 8));
      const filesz = word(pv, p + (e64 ? 32 : 16)), memsz = word(pv, p + (e64 ? 40 : 20));
      lo = Math.min(lo, vaddr);
      hi = Math.max(hi, vaddr + memsz);
      if (flags === null && at >= vaddr && at + 8 <= vaddr + filesz) {
        const h = await this.#readAt(path, off + (at - vaddr), 40);
        if (!h || h.length < 8 || dec.decode(h.subarray(0, 7)) !== 'GEXWASM' || h[7] !== 0) return null;
        flags = h.length >= 40 ? new DataView(h.buffer, h.byteOffset).getUint32(36, true) : 0;
      }
    }
    return flags !== null && (flags & 3) === 0 ? [lo, hi] : null;
  }

  /** Make a program's process: its pid (pid 1's child, leading a session of
   *  its own; pid 1 for the init), its descriptors held for it in the
   *  kernel, and the exec record its Worker starts from. `{ pid, exec, fds,
   *  cwd, wasmImage }`, or `{ error }` (an errno). */
  async spawn({ h, argv, env = [], cwd = '', stdio = 'pipe', cols = 80, rows = 24, init = false }) {
    const prog = await this.#find(argv[0], env, cwd);
    if (typeof prog === 'number') {
      this.#early.delete(h);
      return { error: -prog };
    }
    const wasmImage = await this.#wasmImage(prog);
    const k = this.#k;
    const pid = init ? INIT : await k.r('k_spawn', INIT);
    const e = { h, pty: -1, ptyReading: false, stdin: -1, stdout: -1, stderr: -1, sock: -1, writes: [], closeIn: false, pid };
    const fds = [];
    const queues = [];
    if (stdio === 'pty') {
      const id = (await k.r('k_pty_open_master', HOST)) >> 16;
      const list = [{ f: 'k_pty_set_lock', a: [id, 0] }, { f: 'k_pty_set_winsize', a: [id, (BigInt(rows & 0xffff) << 48n) | (BigInt(cols & 0xffff) << 32n)] }];
      for (const fd of [0, 1, 2]) {
        list.push({ f: 'k_pty_open_end', a: [id, 0, pid] });
        fds.push(`${fd} 0 ${O_RDWR} 0 pty:[${id}:s]`);
      }
      // Its controlling terminal, with its group in the foreground, as a
      // session leader's open of the slave makes it (syscalls.rs `open_pty`).
      list.push({ f: 'k_pty_set_sid', a: [id, pid] }, { f: 'k_pty_set_fg', a: [id, pid] }, { f: 'k_pty_queue', a: [id] });
      if (!init) list.unshift({ f: 'k_setsid', a: [pid] });
      const res = await k.calls(list);
      e.pty = id;
      e.ptyReading = true;
      queues.push(res[res.length - 1].r);
    } else if (stdio === 'null') {
      if (!init) await k.r('k_setsid', pid);
      for (const fd of [0, 1, 2]) fds.push(`${fd} 0 ${O_RDWR} 0 /dev/null`);
    } else {
      // Three pipes. The far end's first reference becomes the program's;
      // this side keeps the near one.
      const ids = (await k.calls([{ f: 'k_pipe_new', a: [HOST] }, { f: 'k_pipe_new', a: [HOST] }, { f: 'k_pipe_new', a: [HOST] }])).map((x) => x.r);
      const list = init ? [] : [{ f: 'k_setsid', a: [pid] }];
      [[0, 'stdin'], [1, 'stdout'], [2, 'stderr']].forEach(([fd, mine], i) => {
        const id = ids[i];
        const childWrites = fd !== 0 ? 1 : 0;
        list.push({ f: 'k_pipe_open_end', a: [id, childWrites, pid] }, { f: 'k_pipe_close_end', a: [id, childWrites, HOST] }, { f: 'k_pipe_queue', a: [id] });
        fds.push(`${fd} 0 ${childWrites ? O_WRONLY : 0} 0 pipe:[${id}]`);
        e[mine] = id;
      });
      const res = await k.calls(list);
      for (const r of res.filter((_, i) => list[i].f === 'k_pipe_queue')) queues.push(r.r);
    }
    for (const q of queues) await k.watch(q);
    this.#adopt(e);
    this.#programs.set(pid, h);
    // vfork.rs `stage_exec_parts`: the path, then the arguments and the
    // environment, each after its length.
    const exec = enc.encode([prog, String(argv.length), ...argv, String(env.length), ...env].map((x) => `${x}\0`).join(''));
    this.pump();
    return { pid, exec, fds: enc.encode(`${fds.join('\n')}\n`), cwd: cwd || '/', wasmImage };
  }

  /** A program whose Worker never started: what was held for it let go,
   *  and its exit (127, as an exec that failed) collected as any. */
  async abandon(pid) {
    await this.#k.calls([{ f: 'k_pipe_forget', a: [pid] }, { f: 'k_pty_forget', a: [pid] }, { f: 'k_exit', a: [pid, 127] }]);
    this.pump();
  }

  /** A connection of the page's to what a program listens on at
   *  127.0.0.1:`port` (the global kernel's name for the port, net.rs
   *  `tcp_name`): its bytes moved as a program's pipes are. */
  async connectGuest(h, port) {
    const id = await this.#k.r('k_sock_connect', { in: `\0gex-tcp:${port}` });
    if (id < 0) throw errno(id === -2 ? -111 : id, `127.0.0.1:${port}`);
    this.#adopt({ h, pty: -1, stdin: -1, stdout: -1, stderr: -1, sock: id, writes: [], closeIn: false, pid: 0 });
    await this.#k.watch(await this.#k.r('k_sock_queue', id));
    this.pump();
  }

  /** Handle `e` is ready: what the page gave it before is its now. */
  #adopt(e) {
    const early = this.#early.get(e.h);
    this.#early.delete(e.h);
    if (early) {
      e.writes.push(...early.writes);
      e.closeIn = early.closeIn;
    }
    this.#handles.set(e.h, e);
  }

  /** What `h` is, or, while its process is still being made, where its
   *  input waits. */
  #input(h) {
    const e = this.#handles.get(h);
    if (e) return e;
    if (!this.#early.has(h)) this.#early.set(h, { writes: [], closeIn: false });
    return this.#early.get(h);
  }

  write(h, bytes) {
    this.#input(h).writes.push(bytes);
    this.pump();
  }

  closeStdin(h) {
    this.#input(h).closeIn = true;
    this.pump();
  }

  async resize(h, cols, rows) {
    const e = this.#handles.get(h);
    if (!e || e.pty < 0) return;
    await this.#k.r('k_pty_set_winsize', e.pty, (BigInt(rows & 0xffff) << 48n) | (BigInt(cols & 0xffff) << 32n));
  }

  async kill(pid, signal) {
    const r = await this.#k.r('k_kill', INIT, pid, signal);
    if (r < 0 && r !== -3) throw errno(r, `kill ${pid}`);
  }

  /** Handles done with: ends closed. */
  async release(h) {
    this.#early.delete(h);
    const e = this.#handles.get(h);
    if (!e) return;
    this.#handles.delete(h);
    await this.#closeEnds(e);
  }

  async #closeEnds(e) {
    const list = [];
    const unwatch = [];
    if (e.sock >= 0) {
      unwatch.push({ f: 'k_sock_queue', a: [e.sock] });
      list.push({ f: 'k_sock_close', a: [e.sock] });
      e.sock = -1;
    }
    if (e.pty >= 0) {
      if (e.ptyReading) unwatch.push({ f: 'k_pty_queue', a: [e.pty] });
      list.push({ f: 'k_pty_close_end', a: [e.pty, 1, HOST] });
      e.pty = -1;
    }
    if (e.stdin >= 0) {
      unwatch.push({ f: 'k_pipe_queue', a: [e.stdin] });
      list.push({ f: 'k_pipe_close_end', a: [e.stdin, 1, HOST] });
      e.stdin = -1;
    }
    for (const f of ['stdout', 'stderr']) {
      if (e[f] >= 0) {
        unwatch.push({ f: 'k_pipe_queue', a: [e[f]] });
        list.push({ f: 'k_pipe_close_end', a: [e[f], 0, HOST] });
        e[f] = -1;
      }
    }
    for (const { r } of unwatch.length ? await this.#k.calls(unwatch) : []) await this.#k.unwatch(r);
    if (list.length) await this.#k.calls(list);
  }

  // -- watches (fs.js `watch`) ---------------------------------------------

  async addWatch(path) {
    const w = this.#watches;
    if (w.id < 0) {
      const id = await this.#k.r('k_inotify_create');
      if (id < 0) throw errno(id, 'inotify');
      w.id = id;
      await this.#k.watch(await this.#k.r('k_inotify_queue', id));
    }
    const wd = await this.#k.r('k_inotify_add_watch', w.id, { in: await this.#files.real(path) }, WATCH_MASK);
    if (wd < 0) throw errno(wd, path);
    const h = w.next++;
    // Two watches on one path share a descriptor, as inotify's do.
    const hs = w.byWd.get(wd) ?? new Set();
    hs.add(h);
    w.byWd.set(wd, hs);
    w.byH.set(h, wd);
    return h;
  }

  async rmWatch(h) {
    const w = this.#watches;
    const wd = w.byH.get(h);
    if (wd === undefined) return;
    w.byH.delete(h);
    const hs = w.byWd.get(wd);
    hs?.delete(h);
    if (hs && !hs.size) {
      w.byWd.delete(wd);
      await this.#k.r('k_inotify_rm_watch', w.id, wd);
    }
  }

  #watchEvents(b) {
    const w = this.#watches;
    const d = new DataView(b.buffer, b.byteOffset, b.length);
    for (let at = 0; at + 16 <= b.length;) {
      const wd = d.getInt32(at, true), mask = d.getUint32(at + 4, true), cookie = d.getUint32(at + 8, true), len = d.getUint32(at + 12, true);
      const raw = b.subarray(at + 16, at + 16 + len);
      const z = raw.indexOf(0);
      const name = dec.decode(z < 0 ? raw : raw.subarray(0, z));
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
      const hs = w.byWd.get(wd);
      if (!hs) continue;
      for (const h of hs) this.#on.watch({ h, type, name, cookie });
      if (type === 'gone') {
        for (const h of hs) w.byH.delete(h);
        w.byWd.delete(wd);
      }
    }
  }

  // -- X display :0 ----------------------------------------------------------

  /** Hold display :0's names for the X server; when a client comes the page
   *  is told (`xWanted`) and starts it on these sockets. */
  async holdX() {
    const list = ['\0/tmp/.X11-unix/X0', '/tmp/.X11-unix/X0'].map((name) => ({ f: 'k_sock_bind', a: [{ in: name }] }));
    const ids = (await this.#k.calls(list)).map(({ r }, i) => {
      if (r < 0) throw errno(r, `X display :0 (${JSON.stringify(list[i].a[0].in)})`);
      return r;
    });
    this.#x.ids = ids;
    for (const { r } of await this.#k.calls(ids.map((id) => ({ f: 'k_sock_queue', a: [id] })))) await this.#k.watch(r);
    this.pump();
    return ids;
  }

  // -- the loop ----------------------------------------------------------------

  /** Something to do from the page: the loop goes round now rather than at
   *  its next wake. */
  pump() {
    this.#dirty = true;
    if (this.#kick) {
      const f = this.#kick;
      this.#kick = null;
      f();
    }
  }

  async #loop() {
    if (this.#looping) return;
    this.#looping = true;
    for (;;) {
      const seq = this.#k.seq();
      this.#dirty = false;
      let moved;
      try {
        moved = await this.#pass();
      } catch (e) {
        this.#on.log(`gex: the runtime's loop: ${e?.stack ?? e}`);
        moved = false;
      }
      if (moved || this.#dirty) continue;
      const kicked = new Promise((r) => { this.#kick = r; });
      await this.#k.sleep(seq, 30000, kicked);
      this.#kick = null;
    }
  }

  /** One pass over everything that can move without waiting, in one batch.
   *  True if anything did. */
  async #pass() {
    const list = [];
    const then = [];
    for (const e of this.#handles.values()) this.#ask(e, list, then);
    const w = this.#watches;
    if (w.id >= 0) {
      list.push({ f: 'k_inotify_read', a: [w.id, { out: 65536 }] });
      then.push(({ r, out }) => (r > 0 ? (this.#watchEvents(out[0]), true) : false));
    }
    if (!this.#x.asked) {
      for (const id of this.#x.ids) {
        list.push({ f: 'k_sock_pending', a: [id] });
        then.push(({ r }) => {
          if (r <= 0 || this.#x.asked) return false;
          this.#x.asked = true;
          this.#xWanted();
          return true;
        });
      }
    }
    // Exits last: what a program wrote before it ended reaches the page
    // before its end does.
    const reaps = list.length;
    for (let i = 0; i < 4; i++) list.push(this.#realInit ? { f: 'k_wait', a: [0, INIT, 0] } : { f: 'k_wait', a: [INIT, -1, 0] });
    const res = await this.#k.calls(list);
    let moved = false;
    for (let i = 0; i < reaps; i++) moved = (await then[i](res[i])) || moved;
    for (let i = reaps; i < res.length; i++) {
      const r = res[i].r;
      if (!r) continue;
      moved = true;
      await this.#exited(Number(r >> 32n), Number(r & 0xffffffffn));
    }
    return moved;
  }

  async #xWanted() {
    // The server watches the sockets from now on.
    for (const { r } of await this.#k.calls(this.#x.ids.map((id) => ({ f: 'k_sock_queue', a: [id] })))) await this.#k.unwatch(r);
    this.#on.xWanted();
  }

  /** What `e` can move: its output read, its input written. */
  #ask(e, list, then) {
    if (e.sock >= 0) {
      list.push({ f: 'k_sock_read', a: [e.sock, { out: READ_MAX }] });
      then.push(({ r, out }) => {
        if (r > 0) return this.#on.data(e.h, 1, out[0]), true;
        if (r === EAGAIN) return false;
        // The program closed it (0), or it failed.
        this.#on.end(e.h, 1);
        this.#handles.delete(e.h);
        return this.#closeEnds(e).then(() => true);
      });
    } else {
      const outs = e.pty >= 0 ? (e.ptyReading ? [['pty', 1]] : []) : [['stdout', 1], ['stderr', 2]];
      for (const [f, fd] of outs) {
        const id = e[f];
        if (id < 0) continue;
        list.push(f === 'pty' ? { f: 'k_pty_read', a: [id, 1, { out: READ_MAX }] } : { f: 'k_pipe_read', a: [id, { out: READ_MAX }] });
        then.push(async ({ r, out }) => {
          if (r > 0) return this.#on.data(e.h, fd, out[0]), true;
          if (r === EAGAIN) return false;
          // End of file (0), or the terminal hung up (EIO). A terminal's
          // master stays held for the page's writes and size until it lets go.
          if (f === 'pty') {
            e.ptyReading = false;
            await this.#k.unwatch(await this.#k.r('k_pty_queue', id));
          } else {
            e[f] = -1;
            await this.#k.unwatch(await this.#k.r('k_pipe_queue', id));
            await this.#k.r('k_pipe_close_end', id, 0, HOST);
          }
          this.#on.end(e.h, fd);
          return true;
        });
      }
    }
    // Input: the first piece waiting.
    if (e.writes.length) {
      const b = e.writes[0];
      const piece = b.subarray(0, READ_MAX);
      const call = e.sock >= 0 ? { f: 'k_sock_write', a: [e.sock, { in: piece }] }
        : e.pty >= 0 ? { f: 'k_pty_write', a: [e.pty, 1, { in: b }] }
        : e.stdin >= 0 ? { f: 'k_pipe_write', a: [e.stdin, { in: piece }] } : null;
      if (!call) {
        e.writes.length = 0;
      } else {
        list.push(call);
        then.push(({ r }) => {
          if (call.f === 'k_pty_write') return e.writes.shift(), true;
          if (r === EAGAIN || r === 0) return false;
          // Nobody reads it any more (EPIPE): what is left is dropped.
          if (r < 0) return (e.writes.length = 0), false;
          if (r < b.length) e.writes[0] = b.subarray(r);
          else e.writes.shift();
          return true;
        });
      }
    }
    if (e.closeIn && !e.writes.length) {
      if (e.sock >= 0 && !e.shut) {
        e.shut = true;
        list.push({ f: 'k_sock_shutdown', a: [e.sock, 1] });
        then.push(() => true);
      } else if (e.sock < 0 && e.stdin >= 0) {
        const id = e.stdin;
        e.stdin = -1;
        list.push({ f: 'k_pipe_queue', a: [id] }, { f: 'k_pipe_close_end', a: [id, 1, HOST] });
        then.push(({ r }) => this.#k.unwatch(r).then(() => true), () => true);
      }
    }
  }

  /** `pid` has ended with wait status `st`: the rest of its output first,
   *  then its exit, to the page. */
  async #exited(pid, st) {
    const h = this.#programs.get(pid);
    this.#programs.delete(pid);
    const e = h === undefined ? null : this.#handles.get(h);
    for (let more = !!e; more;) {
      const list = [];
      const then = [];
      this.#ask(e, list, then);
      if (!list.length) break;
      const res = await this.#k.calls(list);
      more = false;
      for (let i = 0; i < res.length; i++) more = (await then[i](res[i])) || more;
    }
    const sig = st & 0x7f;
    this.#on.exit({ pid, code: sig ? 128 + sig : (st >> 8) & 0xff, signal: sig });
  }
}
