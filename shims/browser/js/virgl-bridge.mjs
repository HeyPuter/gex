// The guest's GPU, reached from a Worker. virglrenderer runs on the page
// (where the canvas is) and the drawing guest is a Worker, so the calls
// between them cross a `SharedArrayBuffer` with `Atomics.wait`: the Worker
// writes its request, wakes the page, and blocks; the page runs the renderer
// and wakes it back. It must be synchronous -- `virgl_submit` is a wasm import
// called from inside a syscall, with the guest's thread holding the wasm stack
// and nothing to await. The page cannot `Atomics.wait` (the main thread may
// not block) and need not: it is woken by a message and replies by storing.

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
 * The page's half: run the renderer when a Worker asks. Call the returned
 * function with each `{kind:'virgl'}` message a Worker posts; the request is
 * already in shared memory by the time it arrives.
 */
export function serveVirgl(renderer, channel) {
  const ctl = new Int32Array(channel.control);
  const data = new Uint8Array(channel.data);
  // The renderer expects a `WebAssembly.Memory` (what the shim hands it on the
  // direct path); here the bytes are in a plain buffer, so hand it a lookalike.
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
      // A renderer that throws must still reply, or the Worker blocks forever
      // inside its syscall.
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
 * The Worker's half: `submit`, `take` and `close`, all `Gex.useVirgl` asks
 * for. `wake` (a `postMessage`) tells the page there is a request -- a callback
 * rather than a port, because a Worker's parent may not be the page.
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
    // An OffscreenCanvas renderer waits on this word directly. The page-side
    // fallback still uses `wake`; notifying with no waiter is harmless.
    Atomics.notify(ctl, STATE);
    this.#wake();
    // A timeout, so a dead page or renderer ends the guest rather than wedging
    // its Worker for the life of the tab. Thirty seconds is far longer than any
    // single command (the slowest, readbacks, are milliseconds).
    while (Atomics.load(ctl, STATE) === REQUEST) {
      if (Atomics.wait(ctl, STATE, REQUEST, 30000) === 'timed-out') {
        throw new Error('the renderer did not answer in thirty seconds');
      }
    }
    Atomics.store(ctl, STATE, IDLE);
    Atomics.notify(ctl, STATE);
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
