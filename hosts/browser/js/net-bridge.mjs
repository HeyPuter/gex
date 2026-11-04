// The guest's network, reached from a Worker. The page holds the sockets
// (page/src/net.js) over its network; every guest Worker calls it across a `SharedArrayBuffer` with
// `Atomics.wait`, the way gl-bridge.mjs reaches the page's GL device.
//
// Why not a WebSocket in each Worker, as before: its bytes arrive as messages
// on that Worker's event loop, and a native wasm32 program waiting in a
// syscall never gives its Worker's event loop back (it is polled in place;
// hosts/browser/src/lib.rs). Nothing it waited for could ever arrive. The page's
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
const REFILL = 10;
const FRAME = 11;

/** Bytes one send or recv carries. A larger send is cut (it returns the count,
 *  as a socket does); a recv takes at most this much. */
const DATA_BYTES = 1 << 20;

// Receive rings: what a stream receives, the page writes into a ring in
// shared memory as it arrives, and a Worker's status, poll and recv read it
// there, with no call to the page. A slot per open stream (none left: the
// stream goes through the page's calls), its words at `SLOT_WORDS * slot`:
const SLOTS = 64;
const SLOT_WORDS = 8;
const R_ID = 0; // the stream's id, 0 for a free slot
const R_STATE = 1; // its state, as `status` returns it
const R_HEAD = 2; // bytes ever written (wrapping)
const R_TAIL = 3; // bytes ever taken (wrapping)
const R_SPILL = 4; // 1: the page holds bytes past what the ring holds
const R_LOCK = 5; // a reader's (or the page's) claim on the slot
/** Bytes one ring holds; a power of two. */
const RING = 256 << 10;
/** Rings the data buffer grows by: it starts empty, so a page whose guest
 *  opens no stream holds none. */
const GROW_SLOTS = 4;
const S_CONNECTING = 0;
const S_OPEN = 1;
const S_CLOSED = 2;
const S_REFUSED = 3;

/** The control words, the data buffer and the receive rings a bridge is
 *  made of, or null where the page cannot serve one (no `Atomics.waitAsync`).
 *  The rings' data grows as streams open (a growable SharedArrayBuffer;
 *  without one, every stream goes through the page's calls). */
export function makeNetChannel() {
  if (typeof Atomics.waitAsync !== 'function' || typeof SharedArrayBuffer !== 'function') return null;
  let rxData = null;
  try {
    rxData = new SharedArrayBuffer(0, { maxByteLength: SLOTS * RING });
    if (typeof rxData.grow !== 'function' || !rxData.growable) rxData = null;
  } catch {
    rxData = null;
  }
  return {
    control: new SharedArrayBuffer(64),
    data: new SharedArrayBuffer(DATA_BYTES),
    rx: rxData ? new SharedArrayBuffer(4 * SLOTS * SLOT_WORDS) : undefined,
    rxData: rxData ?? undefined,
  };
}

/** The receive rings, as the page (`page`: it writes them, for
 *  `Sockets`'s `rings`, net.js) or a Worker (it reads them) sees them. */
export class Rings {
  #w;
  #d;
  #page;
  /** id -> slot (-1: none), checked against the slot's id at each use: a
   *  stream gets its slot as it opens, and ids are never used again. */
  #at = new Map();
  #data;
  constructor(channel, page = false) {
    this.#w = new Int32Array(channel.rx);
    // Length-tracking: it sees the buffer as it grows.
    this.#d = new Uint8Array(channel.rxData);
    this.#data = channel.rxData;
    this.#page = page;
  }

  /** The page: a free slot for stream `id`, or -1. */
  claim(id) {
    const w = this.#w;
    for (let k = 0; k < SLOTS; k++) {
      const b = k * SLOT_WORDS;
      if (Atomics.load(w, b + R_ID) !== 0) continue;
      // Its ring's bytes, before any Worker can find the slot.
      const need = Math.min(SLOTS, Math.ceil((k + 1) / GROW_SLOTS) * GROW_SLOTS) * RING;
      if (this.#data.byteLength < need) this.#data.grow(need);
      this.#locked(b, () => {
        w[b + R_STATE] = S_CONNECTING;
        w[b + R_HEAD] = 0;
        w[b + R_TAIL] = 0;
        w[b + R_SPILL] = 0;
        Atomics.store(w, b + R_ID, id);
      });
      return k;
    }
    return -1;
  }

  /** The page: slot `k` free again, its stream closed by its last holder. */
  release(k) {
    const b = k * SLOT_WORDS;
    this.#locked(b, () => Atomics.store(this.#w, b + R_ID, 0));
  }

  /** The slot's lock: a Worker waits for it, the page spins (no waiting on
   *  its thread), and each holds it only for a copy. */
  #locked(b, f) {
    const w = this.#w, me = this.#page ? -1 : 1;
    for (;;) {
      const held = Atomics.compareExchange(w, b + R_LOCK, 0, me);
      if (held === 0) break;
      if (!this.#page) Atomics.wait(w, b + R_LOCK, held, 1);
    }
    try {
      return f();
    } finally {
      Atomics.store(w, b + R_LOCK, 0);
      if (!this.#page) Atomics.notify(w, b + R_LOCK);
    }
  }

  setState(k, st) {
    Atomics.store(this.#w, k * SLOT_WORDS + R_STATE, st);
  }

  setSpill(k, on) {
    Atomics.store(this.#w, k * SLOT_WORDS + R_SPILL, on ? 1 : 0);
  }

  /** The page: as much of `bytes` as ring `k` has room for, written; how much. */
  write(k, bytes) {
    const w = this.#w, b = k * SLOT_WORDS;
    const head = Atomics.load(w, b + R_HEAD) >>> 0;
    const free = RING - ((head - (Atomics.load(w, b + R_TAIL) >>> 0)) >>> 0);
    const n = Math.min(free, bytes.length);
    if (n <= 0) return 0;
    const at = head & (RING - 1), base = k * RING;
    const first = Math.min(n, RING - at);
    this.#d.set(bytes.subarray(0, first), base + at);
    if (n > first) this.#d.set(bytes.subarray(first, n), base);
    Atomics.store(w, b + R_HEAD, (head + n) | 0);
    return n;
  }

  /** Stream `id`'s slot word base, or -1 when it has none. */
  find(id) {
    const w = this.#w;
    const k = this.#at.get(id);
    if (k === -1) return -1;
    if (k !== undefined && Atomics.load(w, k * SLOT_WORDS + R_ID) === id) return k * SLOT_WORDS;
    for (let j = 0; j < SLOTS; j++) {
      if (Atomics.load(w, j * SLOT_WORDS + R_ID) === id) {
        this.#at.set(id, j);
        return j * SLOT_WORDS;
      }
    }
    // Closed (its slot gone to another stream), or it never had one.
    this.#at.set(id, -1);
    return -1;
  }

  /** Bytes in the ring at `b`. */
  queued(b) {
    return ((Atomics.load(this.#w, b + R_HEAD) >>> 0) - (Atomics.load(this.#w, b + R_TAIL) >>> 0)) >>> 0;
  }

  spill(b) {
    return Atomics.load(this.#w, b + R_SPILL) !== 0;
  }

  /** The stream's state; data that has arrived proves it came up. */
  status(b) {
    const st = Atomics.load(this.#w, b + R_STATE);
    return st === S_CONNECTING && (this.queued(b) || this.spill(b)) ? S_OPEN : st;
  }

  ended(b) {
    const st = Atomics.load(this.#w, b + R_STATE);
    return st === S_CLOSED || st === S_REFUSED;
  }

  /** `recv` (`peek`: or `recv(MSG_PEEK)`) of stream `id`, as net.js's: the
   *  bytes, else 0 once it ended and -1 while it may still receive;
   *  undefined for a stream without a ring. `refill()` moves what the page
   *  holds past the ring into it, called as the ring empties. */
  recv(id, out, peek, refill) {
    const b = this.find(id);
    if (b < 0) return undefined;
    if (!out.length) return 0;
    if (!this.queued(b) && this.spill(b)) refill();
    const n = this.copy(b, id, out, peek);
    if (n < 0) return 0;
    if (n && !peek && this.spill(b)) refill();
    if (n) return n;
    return this.ended(b) && !this.spill(b) ? 0 : -1;
  }

  /** The shim's `net_frame` (dgram-frames.mjs `frameOf`) of stream `id`:
   *  its next datagram's frame, whole or nothing, under the slot's lock, so
   *  two processes reading one flow each take whole frames. A frame partly
   *  in the ring, the rest held past it, is moved in first (`refill()`):
   *  a frame is far smaller than a ring. Undefined for a stream without a
   *  ring. */
  frame(id, out, take, refill) {
    const b = this.find(id);
    if (b < 0) return undefined;
    for (let moved = false; ; moved = true) {
      const n = this.#frameCopy(b, id, out, take);
      if (n === -2) return 0;
      if (n > 0) {
        if (take && this.spill(b)) refill();
        return n;
      }
      if (!moved && this.spill(b)) {
        refill();
        continue;
      }
      return this.ended(b) && !this.spill(b) ? 0 : -1;
    }
  }

  /** One whole frame from the ring at `b`, as much of it as `out` holds,
   *  taken unless `!take`: its whole length; 0 when not all of it is there;
   *  -2 when the slot is no longer stream `id`'s. */
  #frameCopy(b, id, out, take) {
    const w = this.#w;
    return this.#locked(b, () => {
      if (Atomics.load(w, b + R_ID) !== id) return -2;
      const tail = Atomics.load(w, b + R_TAIL) >>> 0;
      const q = ((Atomics.load(w, b + R_HEAD) >>> 0) - tail) >>> 0;
      if (q < 2) return 0;
      const base = (b / SLOT_WORDS) * RING, d = this.#d;
      const need = 2 + ((d[base + (tail & (RING - 1))] << 8) | d[base + ((tail + 1) & (RING - 1))]);
      if (q < need) return 0;
      const n = Math.min(need, out.length);
      const at = tail & (RING - 1), first = Math.min(n, RING - at);
      out.set(d.subarray(base + at, base + at + first), 0);
      if (n > first) out.set(d.subarray(base, base + n - first), first);
      if (take) Atomics.store(w, b + R_TAIL, (tail + need) | 0);
      return need;
    });
  }

  /** Whether a read of stream `id` would not wait; undefined without a ring. */
  available(id) {
    const b = this.find(id);
    if (b < 0) return undefined;
    return this.queued(b) > 0 || this.spill(b) || this.ended(b);
  }

  /** Stream `id`'s state; undefined without a ring. */
  statusOf(id) {
    const b = this.find(id);
    return b < 0 ? undefined : this.status(b);
  }

  /** Up to `out.length` bytes from the ring at `b` for stream `id` into
   *  `out`, taken unless `peek`; how many, or -1 when the slot is no longer
   *  the stream's. */
  copy(b, id, out, peek) {
    const w = this.#w;
    return this.#locked(b, () => {
      if (Atomics.load(w, b + R_ID) !== id) return -1;
      const tail = Atomics.load(w, b + R_TAIL) >>> 0;
      const n = Math.min(out.length, ((Atomics.load(w, b + R_HEAD) >>> 0) - tail) >>> 0);
      const at = tail & (RING - 1), base = (b / SLOT_WORDS) * RING;
      const first = Math.min(n, RING - at);
      out.set(this.#d.subarray(base + at, base + at + first), 0);
      if (n > first) out.set(this.#d.subarray(base, base + n - first), first);
      if (!peek) Atomics.store(w, b + R_TAIL, (tail + n) | 0);
      return n;
    });
  }
}

/** The page's half: run `net` (net.js `Sockets`) when a Worker asks. Returns a
 *  function that stops it. */
export function serveNet(net, channel) {
  const ctl = new Int32Array(channel.control);
  const data = new Uint8Array(channel.data);
  const dec = new TextDecoder();
  let active = true;
  if (channel.rx && 'rings' in net) net.rings = new Rings(channel, true);
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
        case REFILL: r = net.refill?.(id) ?? 0; break;
        case FRAME: r = net.frame ? net.frame(id, data.subarray(0, Math.min(ctl[A1], DATA_BYTES)), ctl[A2] !== 0) : -2; break;
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
    if (channel.rx && 'rings' in net) net.rings = null;
    Atomics.notify(ctl, STATE);
  };
}

/** The Worker's half: the calls `Gex.useNetwork` wants. */
export class NetProxy {
  #ctl;
  #data;
  #enc = new TextEncoder();
  #me;
  #rings = null;

  constructor(channel, me = 1) {
    this.#ctl = new Int32Array(channel.control);
    this.#data = new Uint8Array(channel.data);
    this.#me = me || 1;
    if (channel.rx) this.#rings = new Rings(channel);
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

  status(id) {
    return this.#rings?.statusOf(id) ?? this.#run(this.#call(STATUS, id));
  }

  send(id, bytes) {
    const n = Math.min(bytes.length, DATA_BYTES);
    const req = this.#call(SEND, id, n);
    this.#data.set(bytes.subarray(0, n), 0);
    return this.#run(req);
  }

  recv(id, out) {
    const r = this.#rings?.recv(id, out, false, () => this.#refill(id));
    if (r !== undefined) return r;
    return this.#run(this.#call(RECV, id, Math.min(out.length, DATA_BYTES)), (r) => {
      if (r > 0) out.set(this.#data.subarray(0, r), 0);
    });
  }

  peek(id, out) {
    const r = this.#rings?.recv(id, out, true, () => this.#refill(id));
    if (r !== undefined) return r;
    return this.#run(this.#call(PEEK, id, Math.min(out.length, DATA_BYTES)), (r) => {
      if (r > 0) out.set(this.#data.subarray(0, r), 0);
    });
  }

  /** The next datagram's frame of a UDP flow, whole or nothing (net.js
   *  `frame`). */
  frame(id, out, take) {
    const r = this.#rings?.frame(id, out, take, () => this.#refill(id));
    if (r !== undefined) return r;
    return this.#run(this.#call(FRAME, id, Math.min(out.length, DATA_BYTES), take ? 1 : 0), (r) => {
      if (r > 0) out.set(this.#data.subarray(0, Math.min(r, out.length)), 0);
    });
  }

  #refill(id) { this.#run(this.#call(REFILL, id)); }

  available(id) {
    return this.#rings?.available(id) ?? this.#run(this.#call(AVAILABLE, id)) === 1;
  }

  close(id) { this.#run(this.#call(CLOSE, id)); }

  dup(id) { return this.#run(this.#call(DUP, id)); }
}
