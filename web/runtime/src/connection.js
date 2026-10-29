// A connection from the page to a program's listening socket
// (`sys.net.connectGuest`): a readable of what the program sends, a writable
// to it.

import { INTERNAL } from './internal.js';

export class GuestConnection extends EventTarget {
  #io;
  #in = null;
  #done = false;

  constructor({ write, closeStdin, release }) {
    super();
    this.#io = { write, closeStdin, release };
    /** What the program sends, as bytes; ends when it closes. */
    this.readable = new ReadableStream({
      start: (c) => { this.#in = c; },
      cancel: () => this.close(),
    });
    /** To the program; closing it is a half close (the program reads the end). */
    this.writable = new WritableStream({
      write: (chunk) => {
        const b = chunk instanceof Uint8Array ? chunk.slice() : typeof chunk === 'string' ? new TextEncoder().encode(chunk) : new Uint8Array(chunk);
        write(b);
      },
      close: () => closeStdin(),
      abort: () => this.close(),
    });
    this[INTERNAL] = {
      stream: (m) => {
        if (m.ev === 'data') this.#in?.enqueue(m.bytes);
        else {
          try { this.#in?.close(); } catch {}
          this.#end();
        }
      },
      exit: () => this.#end(),
    };
  }

  #end() {
    if (this.#done) return;
    this.#done = true;
    this.#io.release();
    this.dispatchEvent(new Event('close'));
  }

  /** Close both ways. */
  close() {
    if (this.#done) return;
    try { this.#in?.close(); } catch {}
    this.#end();
  }
}
