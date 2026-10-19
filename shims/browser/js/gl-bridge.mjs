// The guest's GL device, reached from a Worker. The `MesaGlDevice`
// (web/src/mesa-gl.js) is on the page -- it owns the canvas -- while the drawing
// guest is a Worker. `gl_submit` is a wasm import called synchronously from
// inside an ioctl, so the
// call crosses a `SharedArrayBuffer` with `Atomics.wait`: the Worker copies its
// command ring in, wakes the page, and blocks; the page runs `GlDevice.submit`
// on the shared copy, writes the reply back into it, and wakes the Worker.
//
// Copying just the ring (not the whole guest memory) is enough because the ring
// is self-contained: `guest/gl/gexgl.c` writes every command's data inline with
// `put_blob`, so the page never needs to follow a guest pointer.

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
// Set (1) by a Worker whose request is unwaited (`async`): the page itself
// releases the request and the claim once the batch has run, so the ring is
// free for any process as soon as the host is done with it -- not when the
// client next submits, which for a program that swaps and then sleeps was
// never, and every other GL client waited thirty seconds to steal it.
const ASYNC = 4;
// Whether anything serves the ring: 0 not yet (a lazy device still being
// fetched and started), 1 served (`serveGl`), -1 never will be (the device
// did not start, `failGlChannel`). A client waits for a device that is still
// starting however long its download takes on a slow link, and fails at once
// on one that will not come.
const HOST = 5;
const HOST_WAIT_MS = 600000;
// Per client, what its last unwaited batch left to say: one slot of
// SLOT_WORDS words per `client % SLOTS` (key = the client's number + 1,
// the frames that client's surface has shown as of that batch's PRESENT, and
// whether the batch failed). A slot another client took over says nothing.
const SLOT_BASE = 16;
const SLOT_WORDS = 3;
const SLOTS = 128;
const IDLE = 0;
const REQUEST = 1;
const REPLY = 2;

// Must match guest/gl/gexgl.c and web/src/gl-proto.js.
const HEADER_BYTES = 32;
const CMD_CAPACITY = 32 * 1024 * 1024;
const REPLY_CAPACITY = 32 * 1024 * 1024;
export const GL_RING_BYTES = HEADER_BYTES + CMD_CAPACITY + REPLY_CAPACITY;
const H_CAPACITY = 1; // words: magic(0) capacity(1) tail(2) reply_cap(3) reply_len(4)
const H_TAIL = 2;
const H_REPLY_LEN = 4;

function slotOf(client) {
  return SLOT_BASE + SLOT_WORDS * ((client >>> 0) % SLOTS);
}

/** The frames shown that the reply of a batch ending in PRESENT carries: its
 *  last word (gexgl-mesa.cpp answers PRESENT with the surface's count). 0
 *  when there is no reply. */
export function shownInReply(bytes, at) {
  const words = new Uint32Array(bytes.buffer, bytes.byteOffset + at, HEADER_BYTES / 4);
  const len = words[H_REPLY_LEN];
  if (len < 4) return 0;
  const end = at + HEADER_BYTES + words[H_CAPACITY] + len;
  return new DataView(bytes.buffer, bytes.byteOffset).getUint32(end - 4, true);
}

/** The control words and the ring-sized data buffer a bridge is made of. */
export function makeGlChannel() {
  return {
    control: new SharedArrayBuffer((SLOT_BASE + SLOT_WORDS * SLOTS) * 4),
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
  Atomics.store(ctl, HOST, 1);
  // `GlDevice.submit` wants a `{buffer}` and a ring offset; the ring sits at 0
  // of the shared data buffer, so the buffer *is* the "memory" and 0 the ring.
  const asMemory = { buffer: channel.data };
  const data = new Uint8Array(channel.data);
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
    const unwaited = Atomics.load(ctl, ASYNC) === 1;
    const client = Atomics.load(ctl, CLIENT) >>> 0;
    const finish = (result) => {
      if (!unwaited) {
        ctl[RESULT] = result;
        Atomics.store(ctl, STATE, REPLY);
        Atomics.notify(ctl, STATE);
        return;
      }
      // Nobody waits for this one: what it has to say goes to the client's
      // slot, and the request and the claim are released here.
      const slot = slotOf(client);
      ctl[slot] = client + 1;
      ctl[slot + 1] = result < 0 ? 0 : shownInReply(data, 0);
      ctl[slot + 2] = result < 0 ? 1 : 0;
      ctl[ASYNC] = 0;
      Atomics.store(ctl, STATE, IDLE);
      Atomics.notify(ctl, STATE);
      Atomics.store(ctl, LOCK, 0);
      Atomics.notify(ctl, LOCK);
    };
    let result;
    try {
      result = glDevice.submit(asMemory, 0, client);
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

/** The page's device for `channel` did not start: its clients' waits fail
 *  now rather than at their timeout. */
export function failGlChannel(channel) {
  if (!channel?.control) return;
  const ctl = new Int32Array(channel.control);
  Atomics.store(ctl, HOST, -1);
  Atomics.notify(ctl, STATE);
  Atomics.notify(ctl, LOCK);
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

  /** Run the ring at `ring` in the guest `memory`, blocking for the reply.
   *
   * `client` names this process to the host, which keeps a GL context per
   * client. Zero for a caller that has no identity to give.
   *
   * `async`: copy the batch over and return without waiting for it -- the
   * guest's ring is free again as soon as it is copied, so the guest records
   * its next frame while the host runs this one. The page releases the ring
   * when it is done, and this client's next submit, of either kind, waits
   * for that first (its claim), reporting the wait through `noteWait(ms)`.
   * Only for a batch with nothing to answer but a PRESENT's frames shown.
   * Returns, for an unwaited batch, the frames shown as of this client's
   * previous unwaited batch (0 when unknown), or -1 when that batch failed.
   */
  submit(memory, ring, client = 0, async = false, noteWait = null) {
    const gbytes = new Uint8Array(memory.buffer);
    const ghead = new Uint32Array(memory.buffer, ring, HEADER_BYTES / 4);
    const capacity = ghead[H_CAPACITY];
    const tail = ghead[H_TAIL];
    if (HEADER_BYTES + tail > this.#data.length) return -1;

    const ctl = this.#ctl;
    // One ring, and as many processes as have OpenGL open. Without a claim,
    // two of them copy their commands over each other and the host decodes
    // the mixture -- which is not a corrupt frame, it is a rejected batch and
    // a client that stops dead.
    //
    // Stealing after thirty seconds is deliberate: a holder that has not
    // finished in that time is a process that died inside its ioctl, and
    // waiting for it for ever would take every other GL client with it.
    const mine = client || 1;
    let until = Date.now() + 30000;
    let t0 = 0;
    for (;;) {
      const held = Atomics.compareExchange(ctl, LOCK, 0, mine);
      if (held === 0) break;
      if (!t0) t0 = performance.now();
      // Waiting on the value the claim just saw, not on a fresh read of it: a
      // re-read races with the release, `wait` returns "not-equal" at once,
      // and the loop becomes a spin that eats the core its guest runs on --
      // which with two GL clients starves the very page they are waiting for.
      Atomics.wait(ctl, LOCK, held, 50);
      // The holder may be waiting for a device that is still starting: not a
      // dead holder, so the thirty seconds count from the device's start.
      if (Atomics.load(ctl, HOST) === 0) until = Date.now() + 30000;
      if (Date.now() > until) {
        console.warn(`[gl] taking the ring from client ${held}`);
        Atomics.store(ctl, LOCK, mine);
        break;
      }
    }
    if (t0) noteWait?.(performance.now() - t0);
    // What this client's last unwaited batch left: the host has run it (the
    // claim was released after it), so this is its final word.
    const slot = slotOf(client);
    let shown = 0;
    if (ctl[slot] === (((client >>> 0) + 1) | 0)) {
      if (ctl[slot + 2]) {
        // That batch failed: this call reports it, as a waited one would
        // have, and is not run.
        ctl[slot] = 0;
        Atomics.store(ctl, LOCK, 0);
        Atomics.notify(ctl, LOCK);
        return -1;
      }
      shown = ctl[slot + 1] >>> 0;
    }
    if (async) {
      // Released by the page: the channel's buffer is in use until then.
      this.#data.set(gbytes.subarray(ring, ring + HEADER_BYTES + tail), 0);
      Atomics.store(ctl, CLIENT, client >>> 0);
      Atomics.store(ctl, ASYNC, 1);
      Atomics.store(ctl, STATE, REQUEST);
      Atomics.notify(ctl, STATE);
      this.#wake();
      // As the host will leave it: the guest refills its ring at once.
      ghead[H_TAIL] = 0;
      return shown & 0x7fffffff;
    }
    try {
      return this.#exchange(gbytes, ring, capacity, tail, client);
    } finally {
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
    // Not while the device is still starting (HOST 0): a lazy Mesa's download
    // on a slow link takes as long as it takes.
    const t0 = Date.now();
    while (Atomics.load(ctl, STATE) === REQUEST) {
      const host = Atomics.load(ctl, HOST);
      if (host < 0) throw new Error('the GL device did not start');
      if (Atomics.wait(ctl, STATE, REQUEST, 30000) !== 'timed-out') continue;
      const starting = Atomics.load(ctl, HOST) === 0;
      if (!starting) throw new Error('the GL device did not answer in thirty seconds');
      if (Date.now() - t0 > HOST_WAIT_MS) throw new Error('the GL device did not start in ten minutes');
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
