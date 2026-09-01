// The guest's GL device, reached from a Worker. The WebGL `GlDevice`
// (web/src/gl.js) is on the page -- it owns the canvas -- while the drawing
// guest is a Worker. `gl_submit` is a wasm import called synchronously from
// inside an ioctl, so -- exactly as `virgl-bridge.mjs` does for virgl -- the
// call crosses a `SharedArrayBuffer` with `Atomics.wait`: the Worker copies its
// command ring in, wakes the page, and blocks; the page runs `GlDevice.submit`
// on the shared copy, writes the reply back into it, and wakes the Worker.
//
// Copying just the ring (not the whole guest memory) is enough because the ring
// is self-contained: `guest/gl/gexgl.c` writes every command's data inline with
// `put_blob`, so the page never needs to follow a guest pointer.
//
// This mirrors virgl-bridge; the only reason gl needs its own is that
// `GlDevice.submit` reads a `{buffer}` and a ring offset, not a vtest stream.

const STATE = 0; // 0 idle, 1 request, 2 reply
const RESULT = 1;
const IDLE = 0;
const REQUEST = 1;
const REPLY = 2;

// Must match guest/gl/gexgl.c and web/src/gl.js.
const HEADER_BYTES = 32;
const CMD_CAPACITY = 32 * 1024 * 1024;
const REPLY_CAPACITY = 32 * 1024 * 1024;
export const GL_RING_BYTES = HEADER_BYTES + CMD_CAPACITY + REPLY_CAPACITY;
const H_CAPACITY = 1; // words: magic(0) capacity(1) tail(2) reply_cap(3) reply_len(4)
const H_TAIL = 2;
const H_REPLY_LEN = 4;

/** The control words and the ring-sized data buffer a bridge is made of. */
export function makeGlChannel() {
  return {
    control: new SharedArrayBuffer(64),
    data: new SharedArrayBuffer(GL_RING_BYTES),
  };
}

/**
 * The page's half: run the WebGL device when a Worker asks. Call the returned
 * function with each `{kind:'gl'}` message; the ring is already in shared
 * memory by the time it arrives.
 */
export function serveGl(glDevice, channel) {
  const ctl = new Int32Array(channel.control);
  // `GlDevice.submit` wants a `{buffer}` and a ring offset; the ring sits at 0
  // of the shared data buffer, so the buffer *is* the "memory" and 0 the ring.
  const asMemory = { buffer: channel.data };
  let active = true;
  // One submit at a time. Two things drive `serve`: the waitAsync loop below
  // and the function returned to the runtime, which calls it on every
  // wake-up. A JSPI device suspends inside submit, so the request is still
  // marked REQUEST while its promise is pending -- and the other driver, not
  // seeing it, called submit again on the same thread. That re-enters the
  // wasm module and trips musl's own check: "pthread mutex deadlock
  // detected". The throw unwinds to the guest as a failed ioctl, which
  // discards the whole queued batch in silence, and the GL objects created in
  // it are simply gone. It surfaces far away, as an incomplete framebuffer.
  let inFlight = null;
  const serve = function serve() {
    if (inFlight) return inFlight;
    if (Atomics.load(ctl, STATE) !== REQUEST) return;
    const finish = (result) => {
      ctl[RESULT] = result;
      Atomics.store(ctl, STATE, REPLY);
      Atomics.notify(ctl, STATE);
    };
    let result;
    try {
      result = glDevice.submit(asMemory, 0);
    } catch (e) {
      // A device that throws must still reply, or the Worker blocks forever
      // inside its ioctl.
      console.error('[gl] the device threw', e);
      finish(-1);
      return;
    }
    if (result instanceof Promise) {
      inFlight = result.then(finish, (e) => {
        console.error('[gl] the async device threw', e);
        finish(-1);
      }).finally(() => { inFlight = null; });
      return inFlight;
    } else {
      finish(result);
    }
  };
  serve.stop = () => {
    active = false;
    Atomics.notify(ctl, STATE);
  };
  // `waitAsync` is legal on the page thread and avoids a posted-message turn
  // for every synchronous GL query. The renderer still owns the ordinary DOM
  // canvas here, so WebGPU presentation remains visible and flicker-free.
  if (typeof Atomics.waitAsync === 'function') {
    channel.direct = true;
    (async () => {
      while (active) {
        const state = Atomics.load(ctl, STATE);
        if (state === REQUEST) {
          await serve();
          continue;
        }
        const waiting = Atomics.waitAsync(ctl, STATE, state, 1000);
        if (waiting.async) await waiting.value;
      }
    })();
  }
  return serve;
}

/** The Worker's half: `submit`, what `Gex.useGl` calls from `gl_submit`. */
export class GlProxy {
  #ctl;
  #data;
  #words;
  #wake;

  constructor(channel, wake) {
    this.#ctl = new Int32Array(channel.control);
    this.#data = new Uint8Array(channel.data);
    this.#words = new Uint32Array(channel.data);
    this.#wake = wake;
  }

  /** Run the ring at `ring` in the guest `memory`, blocking for the reply. */
  submit(memory, ring) {
    const gbytes = new Uint8Array(memory.buffer);
    const ghead = new Uint32Array(memory.buffer, ring, HEADER_BYTES / 4);
    const capacity = ghead[H_CAPACITY];
    const tail = ghead[H_TAIL];
    if (HEADER_BYTES + tail > this.#data.length) return -1;
    // Forward: the header and the commands recorded so far.
    this.#data.set(gbytes.subarray(ring, ring + HEADER_BYTES + tail), 0);

    const ctl = this.#ctl;
    Atomics.store(ctl, STATE, REQUEST);
    Atomics.notify(ctl, STATE);
    this.#wake();
    // A timeout, so a dead page ends the guest rather than wedging its Worker.
    while (Atomics.load(ctl, STATE) === REQUEST) {
      if (Atomics.wait(ctl, STATE, REQUEST, 30000) === 'timed-out') {
        throw new Error('the GL device did not answer in thirty seconds');
      }
    }
    Atomics.store(ctl, STATE, IDLE);
    Atomics.notify(ctl, STATE);

    // Back: the updated header (tail=0, reply_len, status) then the reply bytes
    // the device wrote into the reply area at HEADER_BYTES + capacity.
    gbytes.set(this.#data.subarray(0, HEADER_BYTES), ring);
    const replyLen = this.#words[H_REPLY_LEN];
    if (replyLen > 0) {
      const from = HEADER_BYTES + capacity;
      gbytes.set(this.#data.subarray(from, from + replyLen), ring + from);
    }
    return ctl[RESULT];
  }
}
