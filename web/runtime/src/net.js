// The guest's network: where its connections out go. One per system, made
// any time (a program's connection before it fails as unreachable). Ours is
// `Wisp` (net/wisp.js); a page brings its own as a `Network` with a
// `connect(host, port)` resolving a duplex: `{ readable, writable }`, a
// ReadableStream and a WritableStream of bytes.

import { internals } from './internal.js';

/** A connection's two streams as a Wisp stream is (web/src/net.js). */
class DuplexWire {
  open = false;
  onopen = null;
  onmessage = null;
  onclose = null;
  onerror = null;
  #writer = null;
  #reader = null;
  #pending = [];
  #closed = false;

  constructor(made) {
    Promise.resolve(made).then(async ({ readable, writable }) => {
      if (this.#closed) {
        readable?.cancel?.().catch(() => {});
        writable?.abort?.().catch(() => {});
        return;
      }
      this.#writer = writable.getWriter();
      for (const b of this.#pending.splice(0)) this.#writer.write(b).catch(() => {});
      this.open = true;
      this.onopen?.();
      this.#reader = readable.getReader();
      try {
        for (;;) {
          const { value, done } = await this.#reader.read();
          if (done) break;
          if (value?.length) this.onmessage?.(value instanceof Uint8Array ? value : new Uint8Array(value));
        }
      } catch {}
      this.#closed = true;
      this.onclose?.();
    }, (e) => {
      this.#closed = true;
      this.onerror?.(e);
    });
  }

  send(bytes) {
    if (this.#closed) return;
    if (this.#writer) this.#writer.write(bytes).catch(() => {});
    else this.#pending.push(bytes);
  }

  close() {
    if (this.#closed) return;
    this.#closed = true;
    this.#writer?.close().catch(() => {});
    this.#reader?.cancel().catch(() => {});
  }
}

export class Network extends EventTarget {
  #backend = null;

  /**
   * The guest's connections out, each `connect(host, port)`'s duplex
   * (`{ readable, writable }`, or a promise of one; a throw or a rejection
   * is a refused connection). `host` is the name the program asked for, or
   * its dotted address. Events: `connect` (`host`, `port`).
   */
  constructor(sys, options = {}) {
    super();
    const { connect } = options;
    if (new.target === Network && typeof connect !== 'function') throw new TypeError('Network: { connect(host, port) }');
    /** What it was made with. */
    this.options = { ...options };
    const i = internals(sys, 'Network');
    i.attach('network', this);
    this.#ready = (async () => {
      const { WispNetwork } = await import('../../src/net.js');
      const backend = this.makeBackend(WispNetwork, connect);
      this.#backend = backend;
      i.network(backend);
      // A proxy that cannot be reached yet is tried again, less often.
      for (let wait = 2000, said = false; ; wait = Math.min(wait * 2, 30000)) {
        try {
          await backend.start?.();
          return;
        } catch (e) {
          if (!said) i.say(`gex: the network: ${e?.message ?? e}; trying again\n`);
          said = true;
          await new Promise((r) => setTimeout(r, wait));
        }
      }
    })();
  }
  #ready;

  /** Resolves once programs' connections go through it. */
  get ready() {
    return this.#ready;
  }

  /** The shim's network over this one: a subclass's (Wisp) or the page's
   *  `connect`. */
  makeBackend(WispNetwork, connect) {
    return new WispNetwork(null, (e) => this.event(e), {
      dial: (host, port) => new DuplexWire(connect(host, port)),
    });
  }

  /** WispNetwork's events ('connect HOST:PORT', 'open', 'closed'), as ours. */
  event(e) {
    const m = /^connect (.*):(\d+)$/.exec(e);
    if (m) this.dispatchEvent(Object.assign(new Event('connect'), { host: m[1], port: Number(m[2]) }));
    else this.dispatchEvent(new Event(e));
  }
}
