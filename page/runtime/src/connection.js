// A connection between the page and a program's socket (`sys.net.connectGuest`,
// `sys.connect`, and each one `sys.listen` takes): a readable of what the
// program sends, a writable to it. And `sys.listen`'s listener.

import { INTERNAL } from './internal.js';
import { errno } from './kernel.js';

export class GuestConnection extends EventTarget {
  #io;
  #in = null;
  #done = false;
  /** Each way's end: the readable read to its end (or cancelled), the
   *  writable closed. The connection is let go when both have ended. */
  #readEnded = false;
  #writeEnded = false;

  constructor({ read, write, closeStdin, release }) {
    super();
    this.#io = { write, closeStdin, release };
    /** What the program sends, as bytes, as the page reads it; ends when it
     *  closes or shuts down its writing. Read only as the page pulls: a page
     *  that does not read leaves the program's writes waiting. */
    this.readable = new ReadableStream({
      start: (c) => { this.#in = c; },
      pull: async (c) => {
        let b;
        try {
          b = this.#done ? null : await read();
        } catch (e) {
          c.error(e);
          return this.close();
        }
        if (b) return c.enqueue(b);
        try { c.close(); } catch {}
        this.#readEnded = true;
        this.#settle();
      },
      cancel: () => {
        this.#readEnded = true;
        this.#settle();
      },
    }, { highWaterMark: 0 });
    /** To the program, each write done once the program's socket has taken
     *  it (a program that does not read holds the page's writes); closing it
     *  is a half close (the program reads the end), aborting it closes both
     *  ways. A write after the program has closed fails (EPIPE). */
    this.writable = new WritableStream({
      // An abort while a write waits on the program: the controller's signal
      // says so at once, where `abort` itself waits for that write.
      start: (c) => c.signal?.addEventListener('abort', () => this.close()),
      write: async (chunk) => {
        const b = chunk instanceof Uint8Array ? chunk.slice() : typeof chunk === 'string' ? new TextEncoder().encode(chunk) : new Uint8Array(chunk);
        const r = this.#done ? -32 : await write(b);
        if (r < 0) throw errno(r, 'write');
      },
      close: async () => {
        if (!this.#done) await closeStdin();
        this.#writeEnded = true;
        this.#settle();
      },
      abort: () => this.close(),
    });
    this[INTERNAL] = {
      exit: () => {
        try { this.#in?.close(); } catch {}
        this.#end();
      },
    };
  }

  #settle() {
    if (this.#readEnded && this.#writeEnded) this.#end();
  }

  #end() {
    if (this.#done) return;
    this.#done = true;
    this.#io.release();
    this.dispatchEvent(new Event('close'));
  }

  /** Close both ways: the program reads the end, and its writes fail. */
  close() {
    if (this.#done) return;
    try { this.#in?.close(); } catch {}
    this.#end();
  }
}

/** `sys.listen`'s: the connections programs make to a unix socket name the
 *  page listens on, in the order they come, as the page takes them. */
export class UnixListener {
  #take;
  #close;
  #closed = false;
  #closing = null;

  /** `take()`: the next connection, null once closed. `close()`: the name
   *  let go. `ready`: when the name is bound. */
  constructor(path, { take, close, ready }) {
    /** The name listened on. */
    this.path = path;
    /** Resolves once the name is bound (at boot, for a listener made before
     *  it), rejects if it could not be (EADDRINUSE, ENOENT, ...). */
    this.ready = ready;
    this.ready.catch(() => {});
    this.#take = take;
    this.#close = close;
  }

  /** The next connection, when a program makes one; null once the listener
   *  is closed. Rejects when the name could not be bound. */
  async accept() {
    await this.ready;
    return this.#closed ? null : this.#take();
  }

  /** Stop listening: the name is let go (a program's connect is refused;
   *  the socket file stays, as on Linux), and connections not yet taken
   *  are closed. Those already taken stay open. */
  close() {
    if (!this.#closed) {
      this.#closed = true;
      this.#closing = this.#close();
    }
    return this.#closing;
  }

  get closed() {
    return this.#closed;
  }

  /** `for await (const conn of listener)`: each connection as it comes;
   *  leaving the loop closes the listener. */
  async *[Symbol.asyncIterator]() {
    try {
      for (let c; (c = await this.accept());) yield c;
    } finally {
      await this.close();
    }
  }
}
