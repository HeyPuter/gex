// The guest's network, reached from a Worker. The page holds the one Wisp
// connection; every guest Worker calls it across a `SharedArrayBuffer` with
// `Atomics.wait`, the way gl-bridge.mjs reaches the page's GL device.
//
// Why not a WebSocket in each Worker, as before: its bytes arrive as messages
// on that Worker's event loop, and a native wasm32 program waiting in a
// syscall never gives its Worker's event loop back (it is polled in place;
// shims/browser/src/lib.rs). Nothing it waited for could ever arrive. The page's
// event loop is free, so the bytes land there, and the calls below never
// wait on anything but the page answering.

const STATE = 0; // 0 idle, 1 request, 2 reply
const OP = 1;
const A0 = 2;
const A1 = 3;
const A2 = 4;
const RESULT = 5;
// One channel and every process on it: claimed per call, holding the
// claimant's own number (a stuck claim then says whose it is).
const LOCK = 6;
// Bumped and notified by the page on every network event (a stream opens,
// receives or closes), so a Worker whose guest waits on a socket sleeps on it
// (`NetProxy.activityWait`) rather than polling AVAILABLE in a turn loop.
const ACTIVITY = 8;
const IDLE = 0;
const REQUEST = 1;
const REPLY = 2;

const OPEN = 1;
const CONNECT = 2;
const STATUS = 3;
const SEND = 4;
const RECV = 5;
const AVAILABLE = 6;
const CLOSE = 7;
const DUP = 8;
const PEEK = 9;

/** Bytes one send or recv carries. A larger send is cut (it returns the count,
 *  as a socket does); a recv takes at most this much. */
const DATA_BYTES = 1 << 20;

/** The control words and the data buffer a bridge is made of, or null where
 *  the page cannot serve one (no `Atomics.waitAsync`). */
export function makeNetChannel() {
  if (typeof Atomics.waitAsync !== 'function' || typeof SharedArrayBuffer !== 'function') return null;
  return {
    control: new SharedArrayBuffer(64),
    data: new SharedArrayBuffer(DATA_BYTES),
  };
}

/** The page's half: run `net` (a `WispNetwork`) when a Worker asks. Returns a
 *  function that stops it. */
export function serveNet(net, channel) {
  const ctl = new Int32Array(channel.control);
  const data = new Uint8Array(channel.data);
  const dec = new TextDecoder();
  let active = true;
  const prevActivity = net.onActivity;
  net.onActivity = (...a) => {
    prevActivity?.apply(net, a);
    Atomics.add(ctl, ACTIVITY, 1);
    Atomics.notify(ctl, ACTIVITY);
  };
  const serve = () => {
    const op = ctl[OP];
    const id = ctl[A0];
    let r = 0;
    try {
      switch (op) {
        case OPEN: r = net.open(); break;
        case CONNECT:
          // `slice`: the decoder refuses a view of shared memory.
          net.connect(id, dec.decode(data.slice(0, ctl[A1])), ctl[A2]);
          break;
        case STATUS: r = net.status(id); break;
        case SEND: r = net.send(id, data.subarray(0, ctl[A1])); break;
        case RECV: r = net.recv(id, data.subarray(0, Math.min(ctl[A1], DATA_BYTES))); break;
        case AVAILABLE: r = net.available(id) ? 1 : 0; break;
        case CLOSE: net.close(id); break;
        case DUP: r = net.dup ? net.dup(id) : 0; break;
        case PEEK: r = net.peek ? net.peek(id, data.subarray(0, Math.min(ctl[A1], DATA_BYTES))) : -2; break;
        default: r = -1;
      }
    } catch (e) {
      // It must still answer, or the Worker sits in its syscall until the
      // timeout.
      console.error('[net] the network threw', e);
      r = -2;
    }
    ctl[RESULT] = r;
    Atomics.store(ctl, STATE, REPLY);
    Atomics.notify(ctl, STATE);
  };
  (async () => {
    while (active) {
      const state = Atomics.load(ctl, STATE);
      if (state === REQUEST) {
        serve();
        continue;
      }
      const waiting = Atomics.waitAsync(ctl, STATE, state, 1000);
      if (waiting.async) await waiting.value;
    }
  })();
  return () => {
    active = false;
    net.onActivity = prevActivity;
    Atomics.notify(ctl, STATE);
  };
}

/** The Worker's half: the calls `Gex.useNetwork` wants. */
export class NetProxy {
  #ctl;
  #data;
  #enc = new TextEncoder();
  #me;

  constructor(channel, me = 1) {
    this.#ctl = new Int32Array(channel.control);
    this.#data = new Uint8Array(channel.data);
    this.#me = me || 1;
  }

  /** The page's network event count (`ACTIVITY`), sampled before a wait. */
  activitySeq() {
    return Atomics.load(this.#ctl, ACTIVITY);
  }

  /** A promise that settles at the next network event after `seq`, or in
   *  `ms`. */
  activityWait(seq, ms) {
    const w = Atomics.waitAsync(this.#ctl, ACTIVITY, seq, ms);
    return w.async ? w.value : Promise.resolve(w.value);
  }

  #call(op, a0 = 0, a1 = 0, a2 = 0) {
    const ctl = this.#ctl;
    // A holder that has not let go in five seconds died inside its call.
    const until = Date.now() + 5000;
    for (;;) {
      const held = Atomics.compareExchange(ctl, LOCK, 0, this.#me);
      if (held === 0) break;
      Atomics.wait(ctl, LOCK, held, 50);
      if (Date.now() > until) {
        console.warn(`[net] taking the channel from ${held}`);
        Atomics.store(ctl, LOCK, this.#me);
        break;
      }
    }
    return { a0, a1, a2, op };
  }

  #run({ op, a0, a1, a2 }, after) {
    const ctl = this.#ctl;
    try {
      ctl[OP] = op;
      ctl[A0] = a0;
      ctl[A1] = a1;
      ctl[A2] = a2;
      Atomics.store(ctl, STATE, REQUEST);
      Atomics.notify(ctl, STATE);
      while (Atomics.load(ctl, STATE) === REQUEST) {
        if (Atomics.wait(ctl, STATE, REQUEST, 30000) === 'timed-out') {
          throw new Error('the page did not answer a network call in thirty seconds');
        }
      }
      const r = ctl[RESULT];
      after?.(r);
      Atomics.store(ctl, STATE, IDLE);
      return r;
    } finally {
      Atomics.store(ctl, LOCK, 0);
      Atomics.notify(ctl, LOCK);
    }
  }

  open() { return this.#run(this.#call(OPEN)); }

  connect(id, host, port) {
    const req = this.#call(CONNECT, id, 0, port);
    const b = this.#enc.encode(host);
    this.#data.set(b.subarray(0, DATA_BYTES), 0);
    req.a1 = Math.min(b.length, DATA_BYTES);
    this.#run(req);
  }

  status(id) { return this.#run(this.#call(STATUS, id)); }

  send(id, bytes) {
    const n = Math.min(bytes.length, DATA_BYTES);
    const req = this.#call(SEND, id, n);
    this.#data.set(bytes.subarray(0, n), 0);
    return this.#run(req);
  }

  recv(id, out) {
    return this.#run(this.#call(RECV, id, Math.min(out.length, DATA_BYTES)), (r) => {
      if (r > 0) out.set(this.#data.subarray(0, r), 0);
    });
  }

  peek(id, out) {
    return this.#run(this.#call(PEEK, id, Math.min(out.length, DATA_BYTES)), (r) => {
      if (r > 0) out.set(this.#data.subarray(0, r), 0);
    });
  }

  available(id) { return this.#run(this.#call(AVAILABLE, id)) === 1; }

  close(id) { this.#run(this.#call(CLOSE, id)); }

  dup(id) { return this.#run(this.#call(DUP, id)); }
}
