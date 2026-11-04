// A program `System.spawn` started: its terminal or its three pipes, its
// signals, its end.

import { INTERNAL, signalName } from './internal.js';

const enc = new TextEncoder();

function own(data) {
  if (typeof data === 'string') return enc.encode(data);
  if (data instanceof ArrayBuffer) return new Uint8Array(data.slice(0));
  if (ArrayBuffer.isView(data)) return new Uint8Array(data.buffer, data.byteOffset, data.byteLength).slice();
  throw new TypeError('write: a string or bytes');
}

/** What a program's terminal says: `data`, bytes, or a string when the
 *  terminal was asked for with an `encoding`. */
export class DataEvent extends Event {
  constructor(data) {
    super('data');
    this.data = data;
  }
}

/** A program's terminal, from the outside (the master end). */
export class Pty extends EventTarget {
  #p;
  constructor(p) {
    super();
    this.#p = p;
  }
  /** Keys, or anything the program reads from its terminal. No limit on
   *  the size of one write. */
  write(data) {
    this.#p.write(own(data));
  }
  /** The terminal's new size: the program hears SIGWINCH. */
  resize(cols, rows) {
    this.#p.resize(cols | 0, rows | 0);
  }
}

export class Process extends EventTarget {
  #io;
  #pid = null;
  #status = null;
  #ends = 0;
  #decoder = null;
  /** Why it never started. */
  #failed = null;
  #startedResolve; #startedReject;
  #exitedResolve; #exitedReject;
  #pendingKills = [];
  #drained = null;

  constructor({ argv, stdio, encoding, untilExit = false, read, unread, write, closeStdin, resize, kill, kick, release, windows }) {
    super();
    this.argv = [...argv];
    this.#io = { write, closeStdin, resize, kill, kick, release, windows };
    this.started = new Promise((res, rej) => { this.#startedResolve = res; this.#startedReject = rej; });
    this.exited = new Promise((res, rej) => { this.#exitedResolve = res; this.#exitedReject = rej; });
    // Nobody need wait on both.
    this.started.catch(() => {});
    this.exited.catch(() => {});
    this.pty = null;
    this.stdin = this.stdout = this.stderr = null;
    if (stdio === 'pty') {
      if (encoding) this.#decoder = new TextDecoder(encoding);
      this.pty = new Pty({ write, resize });
      this.#ends = 1;
    } else if (stdio === 'pipe') {
      this.stdin = new WritableStream({
        write: (chunk) => write(own(chunk)),
        close: () => closeStdin(),
        abort: () => closeStdin(),
      });
      // Read as the page reads them: a program whose output nobody reads
      // stops when its pipe is full.
      // `untilExit`: the stream ends once the program has ended and its pipe
      // is empty; what is written after (by a daemon it left) is read and
      // dropped to the pipe's real end, so that writer neither blocks nor
      // gets EPIPE.
      const ended = untilExit ? () => this.#status !== null : null;
      const drop = async (fd) => {
        while (await read(fd));
        this.#ends--;
        this.#settle();
      };
      const output = (fd) => new ReadableStream({
        pull: async (c) => {
          const b = this.#failed ? null : await read(fd, ended);
          if (b) return c.enqueue(b);
          if (this.#failed) return c.error(this.#failed);
          c.close();
          if (b === undefined) return void drop(fd);
          this.#ends--;
          this.#settle();
        },
        cancel: () => {
          unread(fd);
          this.#ends--;
          this.#settle();
        },
      }, { highWaterMark: 0 });
      this.stdout = output(1);
      this.stderr = output(2);
      this.#ends = 2;
    }
    const io = this.#io;
    this[INTERNAL] = {
      started: (pid) => {
        this.#pid = pid;
        this.#startedResolve(pid);
        for (const s of this.#pendingKills.splice(0)) io.kill(pid, s);
      },
      failed: (err) => {
        const e = typeof err === 'number'
          ? Object.assign(new Error(`${this.argv[0]}: ${err === 2 ? 'no such program' : err === 13 ? 'permission denied' : err === 8 ? 'not a program gex can run' : `could not start (errno ${err})`}`), { errno: err })
          : err;
        this.#failed = e;
        this.#startedReject(e);
        this.#exitedReject(e);
        this.dispatchEvent(Object.assign(new Event('error'), { error: e }));
      },
      /** The terminal's output (null at its end). */
      data: (bytes) => {
        if (bytes) this.pty?.dispatchEvent(new DataEvent(this.#decoder ? this.#decoder.decode(bytes, { stream: true }) : bytes));
        else {
          this.#ends--;
          this.#settle();
        }
      },
      /** Resolves once its output has all come (its ends let go). */
      drained: () => (this.#released ? Promise.resolve() : new Promise((r) => {
        const before = this.#drained;
        this.#drained = () => { before?.(); r(); };
      })),
      exit: ({ code, signal, shutdown }) => {
        if (this.#status) return;
        this.#status = { code, signal: signal ? signalName(signal) : null };
        if (shutdown) this.#ends = 0;
        else if (untilExit) io.kick?.();
        this.#settle();
      },
    };
  }

  /** `exited` when the program has ended; its ends let go once its output
   *  has all come too (a daemon it left may hold them longer). */
  #settle() {
    if (!this.#status) return;
    if (!this.#told) {
      this.#told = true;
      this.#exitedResolve(this.#status);
      this.dispatchEvent(Object.assign(new Event('exit'), this.#status));
    }
    if (this.#ends <= 0 && !this.#released) {
      this.#released = true;
      this.#drained?.();
      this.#io.release();
    }
  }
  #told = false;
  #released = false;

  /** Its pid, once it runs (`started`). */
  get pid() {
    return this.#pid;
  }

  /** Send a signal: a name ('SIGTERM', 'TERM') or a number. */
  kill(signal = 'SIGTERM') {
    if (this.#status) return;
    if (this.#pid === null) this.#pendingKills.push(signal);
    else this.#io.kill(this.#pid, signal);
  }

  /** The windows this program has open (HostWindows' GuestWindows). */
  windows() {
    return this.#pid === null ? [] : this.#io.windows?.(this.#pid) ?? [];
  }
}
