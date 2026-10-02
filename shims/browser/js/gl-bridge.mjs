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
// Which guest process the request belongs to. The host keeps a GL context per
// client: object names are the guest's to allocate, so two processes hand out
// the same ones and a single context would give the second client the first
// one's textures.
const CLIENT = 2;
// The ring is one buffer and every process copies into it, so it has to be
// claimed. Holds the claiming client's own number, which is what makes a
// stuck claim diagnosable rather than just stuck.
const LOCK = 3;
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
      result = glDevice.submit(asMemory, 0, Atomics.load(ctl, CLIENT) >>> 0);
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
  /** An unwaited batch (`async`) is in flight: this client holds LOCK, and
   *  the next submit collects the answer before anything else. */
  #pending = false;

  constructor(channel, wake) {
    this.#ctl = new Int32Array(channel.control);
    this.#data = new Uint8Array(channel.data);
    this.#words = new Uint32Array(channel.data);
    this.#wake = wake;
  }

  /** Run the ring at `ring` in the guest `memory`, blocking for the reply.
   *
   * `client` names this process to the host, which keeps a GL context per
   * client. Zero for a caller that has no identity to give.
   *
   * `async`: copy the batch over and return without waiting for it -- the
   * guest's ring is free again as soon as it is copied, so the guest records
   * its next frame while the host runs this one. The next submit waits for
   * it first (one batch in flight: the channel's buffer is the second copy),
   * reports that wait through `noteWait(ms)`, and fails if it did. Only for
   * a batch with nothing to answer.
   */
  submit(memory, ring, client = 0, async = false, noteWait = null) {
    const gbytes = new Uint8Array(memory.buffer);
    const ghead = new Uint32Array(memory.buffer, ring, HEADER_BYTES / 4);
    const capacity = ghead[H_CAPACITY];
    const tail = ghead[H_TAIL];
    if (HEADER_BYTES + tail > this.#data.length) return -1;

    const ctl = this.#ctl;
    if (this.#pending) {
      const t0 = performance.now();
      const before = this.#collect();
      noteWait?.(performance.now() - t0);
      if (before < 0) return before;
    }
    // One ring, and as many processes as have OpenGL open. Without a claim,
    // two of them copy their commands over each other and the host decodes
    // the mixture -- which is not a corrupt frame, it is a rejected batch and
    // a client that stops dead.
    //
    // Stealing after thirty seconds is deliberate: a holder that has not
    // finished in that time is a process that died inside its ioctl, and
    // waiting for it for ever would take every other GL client with it.
    const mine = client || 1;
    const until = Date.now() + 30000;
    for (;;) {
      const held = Atomics.compareExchange(ctl, LOCK, 0, mine);
      if (held === 0) break;
      // Waiting on the value the claim just saw, not on a fresh read of it: a
      // re-read races with the release, `wait` returns "not-equal" at once,
      // and the loop becomes a spin that eats the core its guest runs on --
      // which with two GL clients starves the very page they are waiting for.
      Atomics.wait(ctl, LOCK, held, 50);
      if (Date.now() > until) {
        console.warn(`[gl] taking the ring from client ${held}`);
        Atomics.store(ctl, LOCK, mine);
        break;
      }
    }
    if (async) {
      // Held until `#collect`: the channel's buffer is in use until then.
      this.#data.set(gbytes.subarray(ring, ring + HEADER_BYTES + tail), 0);
      Atomics.store(ctl, CLIENT, client >>> 0);
      Atomics.store(ctl, STATE, REQUEST);
      Atomics.notify(ctl, STATE);
      this.#wake();
      this.#pending = true;
      // As the host will leave it: the guest refills its ring at once.
      ghead[H_TAIL] = 0;
      return 0;
    }
    try {
      return this.#exchange(gbytes, ring, capacity, tail, client);
    } finally {
      Atomics.store(ctl, LOCK, 0);
      Atomics.notify(ctl, LOCK);
    }
  }

  /** Wait for the unwaited batch, release the ring, and return its result. */
  #collect() {
    const ctl = this.#ctl;
    try {
      while (Atomics.load(ctl, STATE) === REQUEST) {
        if (Atomics.wait(ctl, STATE, REQUEST, 30000) === 'timed-out') {
          throw new Error('the GL device did not answer in thirty seconds');
        }
      }
      Atomics.store(ctl, STATE, IDLE);
      Atomics.notify(ctl, STATE);
      return ctl[RESULT];
    } finally {
      this.#pending = false;
      Atomics.store(ctl, LOCK, 0);
      Atomics.notify(ctl, LOCK);
    }
  }

  #exchange(gbytes, ring, capacity, tail, client) {
    // Forward: the header and the commands recorded so far.
    this.#data.set(gbytes.subarray(ring, ring + HEADER_BYTES + tail), 0);

    const ctl = this.#ctl;
    Atomics.store(ctl, CLIENT, client >>> 0);
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
