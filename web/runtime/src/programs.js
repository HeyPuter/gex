// The page's programs, from outside every guest process, through the kernel's
// exports (kernel.js): each one's process, its terminal or pipes (the page's
// ends held under pid 0, which no process has), the exec record its Worker
// starts from (host.mjs `spawnExec`), and its exit. Also what else the page
// waits on in the kernel: its file watches (inotify) and the X display it
// holds for the server.
//
// Pid 1 is the kernel's own when the page names no init (`k_init_virtual`):
// a program the page starts is its child, in a session of its own, collected
// here as pid 1 when it ends; an orphan is given to pid 1, as on Linux, and
// the kernel lets it go as it ends. With an init, that program is pid 1, the
// child of nobody (parent 0), and the page starts nothing else.
//
// A pipe or a connection is read when the page asks (`read`) and written as
// the program takes it (`write`): a program whose output nobody reads stops
// when its pipe is full, as on Linux. A terminal is read as it fills, into
// `data`. Each waits on its own queue's moves (kernel.js `changed`).

import { errno } from './kernel.js';

const enc = new TextEncoder();
const dec = new TextDecoder();
const EAGAIN = -11;
const INIT = 1;
/** The pid the page's own ends are held under: no process has it. */
const HOST = 0;
const READ_MAX = 1 << 18;
const IN = { MODIFY: 0x2, ATTRIB: 0x4, CLOSE_WRITE: 0x8, MOVED_FROM: 0x40, MOVED_TO: 0x80, CREATE: 0x100, DELETE: 0x200, DELETE_SELF: 0x400, MOVE_SELF: 0x800, IGNORED: 0x8000 };
const WATCH_MASK = IN.ATTRIB | IN.CLOSE_WRITE | IN.MOVED_FROM | IN.MOVED_TO | IN.CREATE | IN.DELETE | IN.DELETE_SELF | IN.MOVE_SELF;
const winsize = (cols, rows) => (BigInt(rows & 0xffff) << 48n) | (BigInt(cols & 0xffff) << 32n);

export class Programs {
  #k;
  #files;
  #on;
  /** Each program's or connection's ends, by handle: a promise, made when
   *  the page asks for it, of `{ h, pid, pty, stdin, stdout, stderr, sock,
   *  queues }` (ids, -1 for none), or null when it never got any. */
  #ends = new Map();
  /** The page's programs, pid -> handle. */
  #programs = new Map();
  /** The page's watches, by handle: `{ id, queue, open }`, an inotify
   *  instance each. */
  #watches = new Map();
  #nextWatch = 1;

  /** `on`: { data(h, bytes), end(h), exit({ pid, code, signal }),
   *  watch({ h, type, name, cookie }), xWanted() }. */
  constructor(kernel, files, on) {
    this.#k = kernel;
    this.#files = files;
    this.#on = on;
  }

  /** At boot: pid 1 (a program, `init`, or the kernel's own). */
  async start({ init }) {
    const list = [];
    if (!init) list.push({ f: 'k_init_virtual', a: [1] }, { f: 'k_proc_set_info', a: [INIT, { in: 'init\ninit\0' }] });
    list.push({ f: 'k_proc_queue', a: [INIT] });
    const res = await this.#k.calls(list);
    // Moved when a child of pid 1 ends, and when pid 1 itself does.
    const queue = res[res.length - 1].r;
    await this.#k.watch(queue);
    this.#reap(init ? { f: 'k_wait', a: [0, INIT, 0] } : { f: 'k_wait', a: [INIT, -1, 0] });
  }

  /** Collect what ends, for good. */
  async #reap(wait) {
    const k = this.#k;
    for (;;) {
      const seq = k.seq();
      let res;
      try {
        res = await k.calls([wait, wait, wait, wait]);
      } catch {
        return;
      }
      const ended = res.filter(({ r }) => r);
      for (const { r } of ended) await this.#exited(Number(r >> 32n), Number(r & 0xffffffffn));
      if (!ended.length) await k.changed(seq);
    }
  }

  /** `pid` has ended with wait status `st`: what its terminal still holds
   *  first, then its exit, to the page. */
  async #exited(pid, st) {
    const h = this.#programs.get(pid);
    this.#programs.delete(pid);
    const e = h === undefined ? null : await this.#ends.get(h);
    if (e?.reading) {
      const flushed = new Promise((r) => { e.flushed = r; });
      this.#k.kick();
      await flushed;
    }
    const sig = st & 0x7f;
    this.#on.exit({ pid, code: sig ? 128 + sig : (st >> 8) & 0xff, signal: sig });
  }


  /** Make a program's process: its pid (pid 1's child, leading a session of
   *  its own; pid 1 for the init), its descriptors held for it in the
   *  kernel, and the exec record its Worker starts from. `{ pid, exec, fds,
   *  cwd, wasmImage }`, or `{ error }` (an errno). Its ends can be asked for
   *  (`read`, `write`) from now on. */
  spawn(o) {
    const made = this.#spawn(o);
    this.#ends.set(o.h, made.then((m) => m.ends ?? null, () => null));
    return made.then(({ ends, ...m }) => m);
  }

  async #spawn({ h, argv, env = [], cwd = '', stdio = 'pipe', cols = 80, rows = 24, init = false }) {
    const k = this.#k;
    const spec = enc.encode([stdio, cols, rows, init ? 1 : 0, cwd, argv.length, ...argv, env.length, ...env].map((x) => `${x}\0`).join(''));
    const { r, out: [b] } = await k.call('k_page_spawn', { in: spec }, { out: spec.length + (16 << 10) });
    if (r < 0) return { error: -r };
    // pagespawn.rs: twelve words, the image span, the exec record, the
    // descriptors.
    const v = new DataView(b.buffer, b.byteOffset, b.length);
    const w = (i) => v.getInt32(i * 4, true);
    const e = { h, pid: w(0), pty: w(1), stdin: w(2), stdout: w(3), stderr: w(4), sock: -1 };
    e.queues = e.pty >= 0 ? [w(5)] : e.stdin >= 0 ? [w(5), w(6), w(7)] : [];
    for (const q of e.queues) await k.watch(q);
    this.#programs.set(e.pid, h);
    if (e.pty >= 0) this.#readPty(e);
    const wasmImage = w(8) ? [Number(v.getBigUint64(48, true)), Number(v.getBigUint64(56, true))] : null;
    const exec = b.slice(64, 64 + w(9));
    const fds = b.slice(64 + w(9), 64 + w(9) + w(10));
    return { pid: e.pid, exec, fds, cwd: cwd || '/', wasmImage, ends: e };
  }

  /** A program whose Worker never started: what was held for it let go,
   *  and its exit (127, as an exec that failed) collected as any. */
  async abandon(pid) {
    await this.#k.calls([{ f: 'k_pipe_forget', a: [pid] }, { f: 'k_pty_forget', a: [pid] }, { f: 'k_exit', a: [pid, 127] }]);
  }

  /** A connection of the page's to what a program listens on at
   *  127.0.0.1:`port` (the global kernel's name for the port, net.rs
   *  `tcp_name`): read and written as a program's pipes are. */
  async connectGuest(h, port) {
    const made = (async () => {
      const id = await this.#k.r('k_sock_connect', { in: `\0gex-tcp:${port}` });
      if (id < 0) throw errno(id === -2 ? -111 : id, `127.0.0.1:${port}`);
      const q = await this.#k.r('k_sock_queue', id);
      await this.#k.watch(q);
      return { h, pid: 0, pty: -1, stdin: -1, stdout: -1, stderr: -1, sock: id, queues: [q] };
    })();
    this.#ends.set(h, made.catch(() => null));
    await made;
  }

  /** The next bytes the program wrote to `fd` (1 or 2; a connection has one
   *  way in), when it writes them; null at their end. */
  async read(h, fd = 1) {
    const e = await this.#ends.get(h);
    const id = e ? (e.sock >= 0 ? e.sock : fd === 2 ? e.stderr : e.stdout) : -1;
    if (id < 0) return null;
    const f = e.sock >= 0 ? 'k_sock_read' : 'k_pipe_read';
    for (const k = this.#k; ;) {
      const seq = k.seq();
      const { r, out } = await k.call(f, id, { out: READ_MAX }).catch(() => ({ r: -9 }));
      if (r > 0) return out[0];
      if (r === EAGAIN) {
        await k.changed(seq);
        continue;
      }
      // The program closed it (0), or it failed.
      if (e.sock < 0) await this.#close(e, fd === 2 ? 'stderr' : 'stdout');
      return null;
    }
  }

  /** Output `fd` no longer read: the program's writes to it fail (EPIPE). */
  async unread(h, fd) {
    const e = await this.#ends.get(h);
    if (e?.sock < 0) await this.#close(e, fd === 2 ? 'stderr' : 'stdout');
  }

  /** `bytes` to the program, as it takes them; dropped when nobody reads
   *  them any more. */
  async write(h, bytes) {
    const e = await this.#ends.get(h);
    if (!e) return;
    const k = this.#k;
    if (e.pty >= 0) {
      await k.r('k_pty_write', e.pty, 1, { in: bytes }).catch(() => {});
      return;
    }
    const id = e.sock >= 0 ? e.sock : e.stdin;
    const f = e.sock >= 0 ? 'k_sock_write' : 'k_pipe_write';
    for (let b = bytes; b.length && id >= 0;) {
      const seq = k.seq();
      const r = await k.r(f, id, { in: b.subarray(0, READ_MAX) }).catch(() => -9);
      if (r === EAGAIN || r === 0) await k.changed(seq);
      else if (r < 0) return;
      else b = b.subarray(r);
    }
  }

  /** The end of what the page writes: the program reads end of file. */
  async closeStdin(h) {
    const e = await this.#ends.get(h);
    if (e?.sock >= 0) await this.#k.r('k_sock_shutdown', e.sock, 1).catch(() => {});
    else if (e) await this.#close(e, 'stdin');
  }

  /** A terminal is read as it fills, into `data`, to its end (the program
   *  and whatever it left hang up). An exit is told once what came before
   *  it has been (`flushed`). */
  async #readPty(e) {
    const k = this.#k;
    e.reading = true;
    for (;;) {
      const seq = k.seq();
      const flushed = e.flushed;
      const { r, out } = await k.call('k_pty_read', e.pty, 1, { out: READ_MAX }).catch(() => ({ r: -9 }));
      if (r > 0) {
        this.#on.data(e.h, out[0]);
        continue;
      }
      if (flushed) {
        e.flushed = null;
        flushed();
      }
      if (r === EAGAIN) {
        await k.changed(seq);
        continue;
      }
      // The terminal hung up (EIO), or was let go. The master stays held for
      // the page's writes and size until it lets go.
      e.reading = false;
      e.flushed?.();
      await k.unwatch(e.queues[0]);
      this.#on.end(e.h);
      return;
    }
  }

  async resize(h, cols, rows) {
    const e = await this.#ends.get(h);
    if (e?.pty >= 0) await this.#k.r('k_pty_set_winsize', e.pty, winsize(cols, rows));
  }

  async kill(pid, signal) {
    const r = await this.#k.r('k_kill', INIT, pid, signal);
    if (r < 0 && r !== -3) throw errno(r, `kill ${pid}`);
  }

  /** Handles done with: ends closed. */
  async release(h) {
    const e = await this.#ends.get(h);
    this.#ends.delete(h);
    if (!e) return;
    if (e.sock >= 0) {
      await this.#k.unwatch(e.queues[0]);
      await this.#k.r('k_sock_close', e.sock);
      e.sock = -1;
    }
    if (e.pty >= 0) {
      if (e.reading) await this.#k.unwatch(e.queues[0]);
      await this.#k.r('k_pty_close_end', e.pty, 1, HOST);
      e.pty = -1;
    }
    for (const f of ['stdin', 'stdout', 'stderr']) await this.#close(e, f);
  }

  /** The page's end of pipe `f` of `e` closed. */
  async #close(e, f) {
    const id = e[f];
    if (id < 0) return;
    e[f] = -1;
    const i = ['stdin', 'stdout', 'stderr'].indexOf(f);
    await this.#k.unwatch(e.queues[i]);
    await this.#k.r('k_pipe_close_end', id, f === 'stdin' ? 1 : 0, HOST).catch(() => {});
  }

  // -- watches (fs.js `watch`) ---------------------------------------------

  /** A watch on `path`: an inotify instance of its own, read as it fills. */
  async addWatch(path) {
    const k = this.#k;
    const id = await k.r('k_inotify_create');
    if (id < 0) throw errno(id, 'inotify');
    const [{ r: wd }, { r: queue }] = await k.calls([
      { f: 'k_inotify_add_watch', a: [id, { in: await this.#files.real(path) }, WATCH_MASK] },
      { f: 'k_inotify_queue', a: [id] },
    ]);
    if (wd < 0) {
      await k.r('k_inotify_close', id);
      throw errno(wd, path);
    }
    await k.watch(queue);
    const h = this.#nextWatch++;
    const w = { id, queue, open: true };
    this.#watches.set(h, w);
    (async () => {
      while (w.open) {
        const seq = k.seq();
        const { r, out } = await k.call('k_inotify_read', id, { out: 65536 }).catch(() => ({ r: -9 }));
        if (r > 0) {
          if (this.#watchEvents(h, out[0])) await this.rmWatch(h);
        } else if (r === EAGAIN || r === 0) {
          await k.changed(seq);
        } else {
          return;
        }
      }
    })();
    return h;
  }

  async rmWatch(h) {
    const w = this.#watches.get(h);
    if (!w) return;
    this.#watches.delete(h);
    w.open = false;
    await this.#k.unwatch(w.queue);
    await this.#k.r('k_inotify_close', w.id).catch(() => {});
  }

  /** Watch `h`'s events, to the page. True when what it watched is gone. */
  #watchEvents(h, b) {
    const d = new DataView(b.buffer, b.byteOffset, b.length);
    let gone = false;
    for (let at = 0; at + 16 <= b.length;) {
      const mask = d.getUint32(at + 4, true), cookie = d.getUint32(at + 8, true), len = d.getUint32(at + 12, true);
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
      if (!type || gone) continue;
      this.#on.watch({ h, type, name, cookie });
      gone = type === 'gone';
    }
    return gone;
  }

  // -- X display :0 ----------------------------------------------------------

  /** Hold display :0's names for the X server; when a client comes the page
   *  is told (`xWanted`) and starts it on these sockets. */
  async holdX() {
    const k = this.#k;
    const list = ['\0/tmp/.X11-unix/X0', '/tmp/.X11-unix/X0'].map((name) => ({ f: 'k_sock_bind', a: [{ in: name }] }));
    const ids = (await k.calls(list)).map(({ r }, i) => {
      if (r < 0) throw errno(r, `X display :0 (${JSON.stringify(list[i].a[0].in)})`);
      return r;
    });
    const queues = (await k.calls(ids.map((id) => ({ f: 'k_sock_queue', a: [id] })))).map((x) => x.r);
    for (const q of queues) await k.watch(q);
    (async () => {
      for (;;) {
        const seq = k.seq();
        const res = await k.calls(ids.map((id) => ({ f: 'k_sock_pending', a: [id] }))).catch(() => null);
        if (!res) return;
        if (res.some(({ r }) => r > 0)) break;
        await k.changed(seq);
      }
      // The server watches the sockets from now on.
      for (const q of queues) await k.unwatch(q);
      this.#on.xWanted();
    })();
    return ids;
  }
}
