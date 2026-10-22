// The single-thread build: every guest process and thread on one JS thread,
// with no SharedArrayBuffer.
//
// The default build gives each guest thread a Worker and shares state through
// SharedArrayBuffers: the process's memory between its threads' Workers, the
// global kernel's memory between every Worker, and `Atomics.wait` for every
// blocking call. A page that is not cross-origin isolated has none of that.
// Here instead:
//
//   - every memory is a plain one, held by reference: a process's threads are
//     instances on one memory, every process's kernel instance is on the one
//     kernel memory, all in this realm;
//   - a guest thread is a fiber: a `FiberWorker` runs the same Worker code
//     (process.mjs) in this realm, and its guest runs on a JSPI stack
//     (`WebAssembly.promising`), so a blocking call suspends that stack
//     (`WebAssembly.Suspending`) where the default build would `Atomics.wait`;
//   - `Sched` stands in for the wait/notify words: a parked fiber is a
//     promise, a wake resolves it.
//
// The binaries are the shared build's (threads need its per-instance stacks
// and thread-locals), with the memory imports' `shared` flag cleared as they
// are compiled: an atomic instruction is valid on a plain memory, and the
// only one that is not (`memory.atomic.wait32`) is never reached, because a
// fiber never waits inside wasm.

export const single = {
  /** Whether this realm runs the single-thread build. */
  on: false,
};

const SUSPENDING = typeof WebAssembly.Suspending === 'function';

/** Whether this engine can run the single-thread build at all (JSPI). */
export function singleSupported() {
  return SUSPENDING && typeof WebAssembly.promising === 'function';
}

// ---------------------------------------------------------------------------
// Binaries: the shared build's modules on plain memories.

function leb(bytes, at) {
  let v = 0, shift = 0, b;
  do {
    b = bytes[at++];
    v += (b & 0x7f) * 2 ** shift;
    shift += 7;
  } while (b & 0x80);
  return [v, at];
}

/** Skip a `limits` (memory type), returning the offset after it. */
function skipLimits(bytes, at) {
  const flags = bytes[at++];
  let x;
  [x, at] = leb(bytes, at);
  if (flags & 1) [x, at] = leb(bytes, at);
  // A custom page size (0x08): one more LEB.
  if (flags & 8) [x, at] = leb(bytes, at);
  return at;
}

function skipTableType(bytes, at) {
  // reftype: a byte (funcref 0x70, externref 0x6f) or a (ref null? ht) form.
  const t = bytes[at++];
  if (t === 0x63 || t === 0x64) [, at] = leb(bytes, at);
  return skipLimits(bytes, at);
}

function skipGlobalType(bytes, at) {
  const t = bytes[at++];
  if (t === 0x63 || t === 0x64) [, at] = leb(bytes, at);
  return at + 1; // mutability
}

/**
 * The offsets of every memory type's flags byte in `bytes` (imported and
 * defined memories), or null when the module cannot be read. Only the import
 * and memory sections are parsed.
 */
function memoryFlagOffsets(bytes) {
  if (bytes.length < 8 || bytes[0] !== 0 || bytes[1] !== 0x61 || bytes[2] !== 0x73 || bytes[3] !== 0x6d) return null;
  const out = [];
  let at = 8;
  while (at < bytes.length) {
    const id = bytes[at++];
    let size;
    [size, at] = leb(bytes, at);
    const end = at + size;
    if (id === 2) {
      let n, p = at;
      [n, p] = leb(bytes, p);
      for (let i = 0; i < n; i++) {
        let len;
        [len, p] = leb(bytes, p); p += len; // module
        [len, p] = leb(bytes, p); p += len; // name
        const kind = bytes[p++];
        if (kind === 0) [, p] = leb(bytes, p);
        else if (kind === 1) p = skipTableType(bytes, p);
        else if (kind === 2) { out.push(p); p = skipLimits(bytes, p); }
        else if (kind === 3) p = skipGlobalType(bytes, p);
        else if (kind === 4) { p++; [, p] = leb(bytes, p); }
        else return null;
      }
    } else if (id === 5) {
      let n, p = at;
      [n, p] = leb(bytes, p);
      for (let i = 0; i < n; i++) { out.push(p); p = skipLimits(bytes, p); }
    }
    // Memories come before code; nothing past the code section matters.
    if (id === 10 || id === 11) break;
    at = end;
  }
  return out;
}

/**
 * `bytes` with every memory made plain: the same module if none was shared,
 * a patched copy otherwise. Accepts any BufferSource.
 */
export function unshare(bytes) {
  let u8;
  if (bytes instanceof Uint8Array) u8 = bytes;
  else if (ArrayBuffer.isView(bytes)) u8 = new Uint8Array(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  else if (bytes instanceof ArrayBuffer) u8 = new Uint8Array(bytes);
  else return bytes;
  const offs = memoryFlagOffsets(u8);
  if (!offs || !offs.some((o) => u8[o] & 2)) return bytes;
  const copy = u8.slice();
  for (const o of offs) copy[o] &= ~2;
  return copy;
}

/**
 * Turn the single-thread build on for this realm: every module compiled here
 * is unshared on the way in, and every memory made here is plain. Patched at
 * the engine's door rather than at the fifty-odd places that compile, so no
 * path can forget.
 */
export function enableSingle() {
  if (single.on) return;
  if (!singleSupported()) throw new Error('the single-thread build needs JSPI (WebAssembly.Suspending)');
  single.on = true;
  const W = WebAssembly;
  const { compile, instantiate, validate } = W;
  const Module = W.Module;
  const Memory = W.Memory;
  W.compile = (b, ...rest) => compile.call(W, unshare(b), ...rest);
  W.validate = (b, ...rest) => validate.call(W, unshare(b), ...rest);
  W.instantiate = (b, ...rest) =>
    instantiate.call(W, b instanceof Module ? b : unshare(b), ...rest);
  if (W.compileStreaming) {
    W.compileStreaming = async (src, ...rest) =>
      compile.call(W, unshare(new Uint8Array(await (await src).arrayBuffer())), ...rest);
    W.instantiateStreaming = async (src, ...rest) =>
      instantiate.call(W, unshare(new Uint8Array(await (await src).arrayBuffer())), ...rest);
  }
  // `new WebAssembly.Module(bytes)` and `instanceof WebAssembly.Module` both
  // have to keep working: a subclass does both.
  // `Symbol.hasInstance`, because the modules the engine makes (`compile`)
  // are instances of the original class, not of the subclass.
  class PlainModule extends Module {
    constructor(b, ...rest) { super(unshare(b), ...rest); }
    static [Symbol.hasInstance](x) { return x instanceof Module; }
  }
  W.Module = PlainModule;
  class PlainMemory extends Memory {
    constructor(desc) { super(desc?.shared ? { ...desc, shared: false } : desc); }
    static [Symbol.hasInstance](x) { return x instanceof Memory; }
  }
  W.Memory = PlainMemory;
}

// ---------------------------------------------------------------------------
// The scheduler: what `Atomics.wait`/`notify` on the kernel's wake words were.

/** Longest a run of fibers may keep the event loop from turning, in ms. */
const SLICE_MS = 8;

/** Under Node (see `#postTurn`). */
const NODE = typeof process === 'object' && !!process.versions?.node && typeof setImmediate === 'function';

class Waiter {
  constructor(resolve, futex, fiber) {
    this.resolve = resolve;
    this.futex = futex; // a BigInt address, null for the global word, or a slot (below)
    this.fiber = fiber;
    this.timer = 0;
    this.done = false;
    /** A wait slot's word (`k_ws_park`): a function giving the kernel
     *  memory's current words (it can grow), the index, the value parked at. */
    this.words = null;
    this.at = 0;
    this.seq = 0;
  }
}

export class Sched {
  /** Fibers parked on the global wake counter. */
  #global = new Set();
  /** Fibers parked on a futex, by address. */
  #futex = new Map();
  /** Fibers parked on their wait slot (`k_ws_park`). A notify bumps the
   *  slot's word in the kernel's (plain) memory and its
   *  `memory.atomic.notify` wakes nobody, so whoever may have notified asks
   *  `checkSlots` -- and every park and yield does. */
  #slots = new Set();
  /** Resolutions held back for a macrotask (see `#resume`). */
  #held = [];
  /** Woken waiters not yet resumed: they run when the running fiber parks,
   *  or at the next turn of the event loop (see `#settle`). */
  #ready = [];
  #turnPosted = false;
  #mc = null;
  /** When the event loop last had a turn. */
  #lastTurn = performance.now();
  /** Counters, for the doc and for a stuck run. */
  stats = { parks: 0, futexParks: 0, wakes: 0, yields: 0, macroTurns: 0, timeouts: 0, flushes: 0 };

  constructor() {
    if (typeof MessageChannel === 'function') {
      this.#mc = new MessageChannel();
      this.#mc.port1.onmessage = () => this.#onTurn();
      this.#mc.port1.start?.();
      // Under Node, a port keeps the process up only while a turn is posted
      // (see `#postTurn`): never the reason it stays up after the guest.
      this.#mc.port1.unref?.();
    }
  }

  #onTurn() {
    this.#turnPosted = false;
    this.#mc?.port1.unref?.();
    this.#lastTurn = performance.now();
    this.stats.macroTurns++;
    const held = this.#held;
    const ready = this.#ready;
    this.#held = [];
    this.#ready = [];
    for (const f of ready) f();
    for (const f of held) f();
  }

  #postTurn() {
    if (this.#turnPosted) return;
    this.#turnPosted = true;
    // Under Node, `setImmediate`, not the port: Node drains a port's
    // messages in one go, those posted meanwhile included (up to a
    // thousand), so a turn posted from a turn never let the loop round to
    // anything else -- V8's own tasks among them. An async
    // `WebAssembly.compile` of 1,195 bytes resolved after 8.1 s, a
    // thousand 8 ms slices, and retire-during-lift's flipper (which
    // compiles at every flip) took 17 s a round. `setImmediate` runs once
    // per loop iteration, after the poll phase.
    if (NODE) {
      setImmediate(() => this.#onTurn());
      return;
    }
    // Held while posted: an unref'd port lets Node exit with the turn (and
    // every fiber waiting on it) still in the queue.
    this.#mc?.port1.ref?.();
    if (this.#mc) this.#mc.port2.postMessage(0);
    else setTimeout(() => this.#onTurn(), 0);
  }

  /** Whether the fibers have held the event loop for a whole slice. */
  sliceOver() {
    return performance.now() - this.#lastTurn >= SLICE_MS;
  }

  /**
   * Resume a fiber: at once (a microtask, after the running fiber suspends)
   * while the slice lasts, and after a turn of the event loop once it has
   * run out -- or a ping-pong between two processes would never let a key,
   * a WebSocket message or a timer through.
   */
  #resume(f) {
    if (!this.sliceOver()) f();
    else {
      this.#held.push(f);
      this.#postTurn();
    }
  }

  #settle(w, why) {
    if (w.done) return;
    w.done = true;
    if (w.timer) clearTimeout(w.timer);
    if (w.words) this.#slots.delete(w);
    else if (w.futex === null) this.#global.delete(w);
    else {
      const s = this.#futex.get(w.futex);
      s?.delete(w);
      if (s && !s.size) this.#futex.delete(w.futex);
    }
    // Not resumed yet. A wake comes from the running fiber's system call
    // (every one bumps the counter), and resolving here would queue the
    // woken fiber's microtask ahead of the waker's own return from that call:
    // a writer and its reader would trade places on every write instead of
    // the writer filling the pipe first. So the woken wait until the running
    // fiber parks (`#flush`), or until every fiber is blocked on something
    // else and the event loop turns -- the posted turn. A wake from outside
    // any fiber (a timer, the network, a key) gets that turn at once.
    this.#ready.push(() => w.resolve(why));
    this.#postTurn();
  }

  /** The running fiber is about to suspend: the woken go now. */
  #flush() {
    if (!this.#ready.length) return;
    this.stats.flushes++;
    const ready = this.#ready;
    this.#ready = [];
    for (const f of ready) this.#resume(f);
  }

  #park(futex, ns, fiber, slot = null) {
    // Whatever the running fiber notified before it parks.
    this.checkSlots();
    return new Promise((resolve) => {
      const w = new Waiter(resolve, futex, fiber);
      if (slot) {
        w.words = slot.words;
        w.at = slot.at;
        w.seq = slot.seq;
        this.#slots.add(w);
      } else if (futex === null) this.#global.add(w);
      else {
        let s = this.#futex.get(futex);
        if (!s) this.#futex.set(futex, (s = new Set()));
        s.add(w);
      }
      // `ns` bounds a wait, as it bounds `memory.atomic.wait32`; a negative
      // one waits for ever.
      const n = Number(ns);
      const ms = Math.max(0, n / 1e6);
      if (n >= 0 && Number.isFinite(ms) && ms < 2 ** 31) {
        w.timer = setTimeout(() => { this.stats.timeouts++; this.#settle(w, 2); }, ms);
      }
      this.#flush();
    });
  }

  /** Sleep until the wait slot word `words()[at]` leaves `seq`, or `ns` pass
   *  (negative: for ever): 0 woken, 1 it had already moved, 2 timed out. */
  parkSlot(words, at, seq, ns, fiber = null) {
    if (Atomics.load(words(), at) !== seq) return 1;
    this.stats.slotParks = (this.stats.slotParks ?? 0) + 1;
    return this.#park(null, ns, fiber, { words, at, seq });
  }

  /** A wait slot may have been notified (a kernel call, the compositor's
   *  input): every fiber whose slot moved re-checks. */
  checkSlots() {
    if (!this.#slots.size) return;
    for (const w of [...this.#slots]) {
      if (Atomics.load(w.words(), w.at) !== w.seq) this.#settle(w, 0);
    }
  }

  /** Sleep until the global counter is bumped, or `ns` pass: 0 woken, 2 timed out. */
  park(ns, fiber = null) {
    this.stats.parks++;
    return this.#park(null, ns, fiber);
  }


  /** Something of the page's own happened that this realm has no queue for
   *  -- the network's bytes, which the one Wisp connection has no way to say
   *  are whose: every parked fiber re-checks. The single-thread build only;
   *  the default build's network waits turn their own Worker's loop. */
  wake() {
    if (!this.#global.size && !this.#slots.size) return;
    this.stats.wakes++;
    for (const w of [...this.#global]) this.#settle(w, 0);
    for (const w of [...this.#slots]) this.#settle(w, 0);
  }

  /** Forget a fiber's waits (its Worker was terminated). */
  drop(fiber) {
    for (const w of [...this.#slots]) if (w.fiber === fiber) { w.done = true; clearTimeout(w.timer); this.#slots.delete(w); }
    for (const w of [...this.#global]) if (w.fiber === fiber) { w.done = true; clearTimeout(w.timer); this.#global.delete(w); }
    for (const [k, s] of this.#futex) {
      for (const w of [...s]) if (w.fiber === fiber) { w.done = true; clearTimeout(w.timer); s.delete(w); }
      if (!s.size) this.#futex.delete(k);
    }
  }

  /**
   * A preemption point (a guest's safepoint, a wasm32 program's system call):
   * nothing when the slice lasts, a promise for the next turn of the event
   * loop when it has run out.
   */
  maybeYield() {
    this.checkSlots();
    if (!this.sliceOver()) return 0;
    this.stats.yields++;
    return new Promise((resolve) => {
      this.#held.push(() => resolve(0));
      this.#postTurn();
    });
  }

  /**
   * The running fiber asked to give way (`sched_yield`): the woken go first,
   * then it. Without this a guest spinning on `sched_yield` for a fiber it
   * just woke would wait out the slice.
   */
  yieldNow() {
    this.checkSlots();
    if (!this.#ready.length) return this.maybeYield();
    this.stats.yields++;
    this.#flush();
    return Promise.resolve(0);
  }

  /** How many fibers are parked, for a census. */
  get parked() {
    let n = this.#global.size + this.#slots.size;
    for (const s of this.#futex.values()) n += s.size;
    return n;
  }
}

/** The one scheduler of this realm. */
export const sched = new Sched();

// ---------------------------------------------------------------------------
// Fibers: a Worker's code, run in this realm.

/**
 * Every message between a host and its fibers, in one queue: a child's
 * output and its parent's must reach the host in the order they were sent,
 * which separate Worker ports never promised (and `drainOutput` existed for).
 */
const queue = [];
let draining = false;

function deliver(f) {
  queue.push(f);
  if (draining) return;
  draining = true;
  queueMicrotask(drain);
}

function drain() {
  // Bounded per turn: a guest that writes without end must not starve the
  // event loop of its own keyboard.
  const t0 = performance.now();
  while (queue.length) {
    const f = queue.shift();
    try {
      f();
    } catch (e) {
      console.error('[gex] a fiber message handler threw:', e);
    }
    if (performance.now() - t0 > SLICE_MS && queue.length) {
      setTimeout(drain, 0);
      return;
    }
  }
  draining = false;
}

/** What `workerMain` (process.mjs) runs a fiber's side through. */
let workerMain = null;

/**
 * A Worker that is a fiber of this realm: the same `postMessage` /
 * `addEventListener` / `terminate` as a Web Worker, so `ProcessHost` drives
 * it unchanged. Messages are not cloned: everything (a memory, a module, a
 * fork's image) is passed by reference, which is the point.
 */
export class FiberWorker {
  #onMessage = [];
  #onError = [];
  #toFiber = null;
  #early = [];
  dead = false;
  static count = 0;

  constructor() {
    FiberWorker.count++;
    const port = {
      post: (m) => deliver(() => {
        if (this.dead) return;
        for (const f of this.#onMessage) f({ data: m });
      }),
      on: (f) => {
        this.#toFiber = f;
        const early = this.#early;
        this.#early = [];
        for (const m of early) deliver(() => f(m));
      },
      fiber: this,
    };
    if (!workerMain) throw new Error('fiber.mjs: process.mjs has not registered workerMain');
    Promise.resolve().then(() => workerMain(port)).catch((e) => {
      for (const f of this.#onError) f(e);
    });
  }

  postMessage(m) {
    if (this.dead) return;
    if (!this.#toFiber) { this.#early.push(m); return; }
    const f = this.#toFiber;
    deliver(() => f(m));
  }

  addEventListener(type, f) {
    if (type === 'message') this.#onMessage.push(f);
    else if (type === 'error') this.#onError.push(f);
  }

  removeEventListener(type, f) {
    const l = type === 'message' ? this.#onMessage : this.#onError;
    const i = l.indexOf(f);
    if (i >= 0) l.splice(i, 1);
  }

  terminate() {
    if (this.dead) return;
    this.dead = true;
    sched.drop(this);
  }
}

/** process.mjs hands itself over here, so this file does not import it. */
export function registerWorkerMain(f) {
  workerMain = f;
}
