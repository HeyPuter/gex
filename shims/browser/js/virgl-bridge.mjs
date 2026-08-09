// The guest's GPU, reached from a Worker.
//
// virglrenderer runs where the canvas is, and a canvas belongs to the page.
// The guest process that draws is a Worker -- and, in the gears demo, not
// even the first one: pid 1 is a shell, and `glxgears` is two forks and an
// `execve` further on. So the drawing process and the renderer are in
// different agents, and the calls between them have to cross.
//
// They have to cross *synchronously*. `virgl_submit` is a wasm import called
// from inside a syscall: the guest's thread is on the wasm stack, holding it,
// and there is nothing to await with. A message would need the shim to be
// able to suspend mid-syscall, which is exactly what it cannot do.
//
// So: a `SharedArrayBuffer` and `Atomics.wait`. The Worker writes its request
// into shared memory, wakes the page, and blocks; the page runs the renderer
// and wakes it back. The Worker's thread is stopped for the duration, which
// is right -- the guest is inside a syscall and has nothing else to do -- and
// the page's is not, which is also right, because it is the one that has to
// keep drawing.
//
// The page cannot use `Atomics.wait` at all (the main thread is not allowed
// to block), and does not need to: it is woken by an ordinary message and
// replies by storing and notifying.

// Control words, by index.
const STATE = 0; // 0 idle, 1 request waiting, 2 reply ready
const OP = 1; // 0 submit, 1 take, 2 close
const SESSION = 2;
const LEN = 3; // request length, then reply length
const RESULT = 4; // what the call returns

const IDLE = 0;
const REQUEST = 1;
const REPLY = 2;

/** How big a single command stream or reply may be. */
export const DATA_BYTES = 16 << 20;

/** The pair of buffers a bridge is made of. Both ends are given the same two. */
export function makeChannel() {
  return {
    control: new SharedArrayBuffer(64),
    data: new SharedArrayBuffer(DATA_BYTES),
  };
}

/**
 * The page's half: run the renderer when a Worker asks.
 *
 * Call the returned function with each `{kind:'virgl'}` message a Worker
 * posts. Nothing else is needed -- the request is already in shared memory by
 * the time the message arrives, and the reply goes back the same way.
 */
export function serveVirgl(renderer, channel) {
  const ctl = new Int32Array(channel.control);
  const data = new Uint8Array(channel.data);
  // The renderer reads and writes through a `WebAssembly.Memory`, because
  // that is what it is handed by the shim on the direct path. Here the bytes
  // are in a plain buffer instead, so it is handed something that looks the
  // same and is.
  const asMemory = { buffer: channel.data };
  return function serve() {
    if (Atomics.load(ctl, STATE) !== REQUEST) return;
    const op = ctl[OP];
    const session = ctl[SESSION];
    const len = ctl[LEN];
    let result = 0;
    let replyLen = 0;
    try {
      if (op === 0) {
        result = renderer ? renderer.submit(session, asMemory, 0, len) : -1;
      } else if (op === 1) {
        const n = renderer ? renderer.take(session, asMemory, 0, Math.min(len, data.length)) : 0;
        result = n;
        replyLen = n;
      } else {
        renderer?.close(session);
      }
    } catch (e) {
      // A renderer that throws must not leave the Worker blocked forever: it
      // is inside a syscall and nothing else will ever wake it.
      console.error('[virgl] the renderer threw', e);
      result = -1;
    }
    ctl[LEN] = replyLen;
    ctl[RESULT] = result;
    Atomics.store(ctl, STATE, REPLY);
    Atomics.notify(ctl, STATE);
  };
}

/**
 * The Worker's half: something with `submit`, `take` and `close`, which is
 * all `Gex.useVirgl` asks for.
 *
 * `wake` is how the page is told there is a request -- a `postMessage`. It is
 * a callback rather than a port because a Worker started by `host.mjs` talks
 * to its own parent, which may itself not be the page.
 */
export class VirglProxy {
  #ctl; #data; #wake;

  constructor(channel, wake) {
    this.#ctl = new Int32Array(channel.control);
    this.#data = new Uint8Array(channel.data);
    this.#wake = wake;
  }

  /** Send a request and block until the page has answered it. */
  #call(op, session, len) {
    const ctl = this.#ctl;
    ctl[OP] = op;
    ctl[SESSION] = session;
    ctl[LEN] = len;
    Atomics.store(ctl, STATE, REQUEST);
    this.#wake();
    // A timeout, so a page that has gone away or a renderer that has died
    // ends the guest rather than wedging its Worker for the life of the tab.
    // Thirty seconds is far longer than any single command: the slowest are
    // readbacks, which are milliseconds.
    while (Atomics.load(ctl, STATE) === REQUEST) {
      if (Atomics.wait(ctl, STATE, REQUEST, 30000) === 'timed-out') {
        throw new Error('the renderer did not answer in thirty seconds');
      }
    }
    Atomics.store(ctl, STATE, IDLE);
    return ctl[RESULT];
  }

  submit(session, memory, ptr, len) {
    if (len > this.#data.length) return -1;
    this.#data.set(new Uint8Array(memory.buffer, ptr, len), 0);
    return this.#call(0, session, len);
  }

  take(session, memory, ptr, max) {
    const n = this.#call(1, session, Math.min(max, this.#data.length));
    if (n > 0) new Uint8Array(memory.buffer, ptr, n).set(this.#data.subarray(0, n));
    return n;
  }

  close(session) {
    this.#call(2, session, 0);
  }
}
