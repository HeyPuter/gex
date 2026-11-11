// The page's side of the global kernel: its exports called by name through
// the kernel-call Worker (kernel-worker.js), and its wait queues waited on
// here, with Atomics.waitAsync on the kernel's own memory, which the page
// shares. One wait slot is moved by every queue the page watches; a caller
// takes `seq()` before a call that may find nothing to do and, when it
// does, waits for `changed(seq)` and calls again.

import { SP_CODE, MP_CODE } from '../../../hosts/browser/js/mode.mjs';

const ERRNO = { 1: 'EPERM', 2: 'ENOENT', 9: 'EBADF', 11: 'EAGAIN', 13: 'EACCES', 17: 'EEXIST', 18: 'EXDEV', 20: 'ENOTDIR', 21: 'EISDIR', 22: 'EINVAL', 28: 'ENOSPC', 36: 'ENAMETOOLONG', 39: 'ENOTEMPTY', 40: 'ELOOP', 32: 'EPIPE', 91: 'EPROTOTYPE', 98: 'EADDRINUSE', 104: 'ECONNRESET', 107: 'ENOTCONN', 111: 'ECONNREFUSED' };

/** A negated errno `n` as an Error that says `what`. */
export function errno(n, what) {
  const code = ERRNO[-n] ?? `errno ${-n}`;
  return Object.assign(new Error(`${code}: ${what}`), { code, errno: -n });
}

async function makeWorker() {
  // The single-thread build has no kernel-call Worker (kernel-single-worker.js).
  if (!MP_CODE) throw new Error('the single-thread build has no kernel-call Worker');
  // The URL written out in the `new Worker`, which is how a bundler knows the
  // file is a Worker to build (page/runtime/build.sh), not an asset to copy.
  if (typeof Worker === 'function') return new Worker(new URL('./kernel-worker.js', import.meta.url), { type: 'module', name: 'gex-kernel' });
  // Node runs the source tree. Its URL not written out: a bundler would copy
  // the unbuilt file into the page's build.
  const { Worker: NodeWorker } = await import('node:worker_threads');
  return new NodeWorker(new URL(import.meta.url.replace(/[^/]*$/, 'kernel-worker.js')));
}

export class Kernel {
  #w; #next = 1; #calls = new Map();
  #memory;
  #names = new Set();
  /** Batches asked for in this turn of the page, sent together. */
  #pending = null;
  #dead = false;
  /** This side's wait slot, made at the first `watch`, and its queues. */
  #slot = -1;
  #slotWord = 0;
  #queues = new Set();
  /** Waiting for the slot to move: `[seq, resolve]`. */
  #waiters = [];
  #ticking = false;
  /** The single-thread build: no kernel memory here, and the wait slot's
   *  word as the Worker last said it (every answer carries it). */
  #single = false;
  #seen = 0;

  /** A Worker with the kernel `spec` ({ module, memory, pageFs, restBundle, lazyCache }). */
  static async start(spec) {
    const k = new Kernel(await makeWorker(), spec.memory);
    k.#names = new Set(await k.#rpc('start', { spec }));
    return k;
  }

  /** The single-thread build's system Worker, made before its `start`:
   *  its modules load while the page gets the system's files ready. */
  static singleWorker() {
    if (!SP_CODE) throw new Error('this build has no single-thread mode');
    return new Worker(new URL('./kernel-single-worker.js', import.meta.url), { type: 'module', name: 'gex-system' });
  }

  /** The single-thread build's system Worker (kernel-single-worker.js),
   *  which holds the kernel and the processes: `spec` its `start`'s, `onEvent`
   *  for what it says unasked (`{ ev, ... }`). The page holds no kernel
   *  memory; the wait slot's word is the Worker's to watch (`seq`). */
  static async startSingle(spec, onEvent, w = Kernel.singleWorker()) {
    const k = new Kernel(w, null, onEvent);
    k.#names = new Set(await k.#rpc('start', { spec }));
    return k;
  }

  /** A message to the single-thread system Worker that wants no answer
   *  (kernel-single-worker.js: the compositor's `{ comp }`). */
  post(m, transfer = []) {
    this.#w.postMessage(m, transfer);
  }

  /** One of the single-thread system Worker's own ops (kernel-single-worker.js). */
  ask(op, args = {}) {
    return this.#rpc(op, args);
  }

  /** Whether this kernel build has export `f`. */
  has(f) {
    return this.#names.has(f);
  }

  constructor(worker, memory, onEvent = null) {
    this.#w = worker;
    this.#memory = memory;
    this.#single = SP_CODE && !memory;
    const on = (m) => {
      if (SP_CODE && this.#single) {
        if (m?.ev) return void onEvent?.(m);
        if (m?.seq !== undefined) this.#seen = m.seq;
      }
      const c = this.#calls.get(m?.call);
      if (!c) return;
      this.#calls.delete(m.call);
      if (m.error) c.reject(Object.assign(new Error(m.error.message), { workerStack: m.error.stack }));
      else c.resolve(m.value);
    };
    const fail = (e) => this.#fail(new Error(`the kernel Worker failed: ${e?.message ?? e}`));
    if (worker.addEventListener) {
      worker.addEventListener('message', (e) => on(e.data));
      worker.addEventListener('error', fail);
    } else {
      worker.on('message', on);
      worker.on('error', fail);
    }
  }

  #fail(err) {
    for (const c of this.#calls.values()) c.reject(err);
    this.#calls.clear();
  }

  #rpc(op, args) {
    if (this.#dead) return Promise.reject(new Error('the kernel is stopped'));
    const call = this.#next++;
    return new Promise((resolve, reject) => {
      this.#calls.set(call, { resolve, reject });
      this.#w.postMessage({ call, op, ...args });
    });
  }

  /** A batch (kernel-worker.js): `[{ r, out }]`, one per call made. Every
   *  batch asked for in one turn of the page goes to the Worker in one
   *  message. */
  calls(list) {
    // A view of part of a larger buffer (a file in a tar) is sent as a copy
    // of that part: posted as it is, the whole buffer would be copied. (The
    // single-thread build's page has no SharedArrayBuffer to ask about.)
    for (const c of list) {
      for (const x of c.a ?? []) {
        const v = x?.in ?? x?.ptr;
        if (v instanceof Uint8Array && v.byteLength !== v.buffer.byteLength && !(!(SP_CODE && this.#single) && v.buffer instanceof SharedArrayBuffer)) {
          if (x.in !== undefined) x.in = v.slice();
          else x.ptr = v.slice();
        }
      }
    }
    if (!this.#pending) {
      const p = this.#pending = { groups: [], done: [] };
      queueMicrotask(() => {
        this.#pending = null;
        this.#rpc('calls', { groups: p.groups }).then(
          (v) => p.done.forEach(([res], i) => res(v[i])),
          (e) => p.done.forEach(([, rej]) => rej(e)),
        );
      });
    }
    const p = this.#pending;
    p.groups.push(list);
    return new Promise((res, rej) => p.done.push([res, rej]));
  }

  /** One call: `{ r, out }`. */
  async call(f, ...a) {
    return (await this.calls([{ f, a }]))[0];
  }

  /** One call's result. */
  async r(f, ...a) {
    return (await this.call(f, ...a)).r;
  }

  /** The kernel's memory, as words, as it is now (it may have grown). */
  words() {
    return new Int32Array(this.#memory.buffer);
  }

  /** Move this side's wait slot when wait queue `queue` moves. */
  async watch(queue) {
    if (!queue || this.#queues.has(queue)) return;
    this.#queues.add(queue);
    await (this.#slotReady ??= this.#allocSlot());
    await this.r('k_wq_add', queue, this.#slot);
  }

  /** Allocate this side's wait slot now, in the batch being made, ahead of
   *  the first `watch`. */
  prepareWatch() {
    return (this.#slotReady ??= this.#allocSlot());
  }

  /** This side's wait slot, once: watches made together wait for the one
   *  allocation, and their queues go to the kernel in one batch. */
  #slotReady = null;
  async #allocSlot() {
    const [{ r: slot }, { r: addr }] = await this.calls([{ f: 'k_ws_alloc' }, { f: 'k_ws_addr', a: [{ ref: 0 }] }]);
    if (slot < 0) throw new Error('the runtime found no free wait slot in the kernel');
    this.#slot = slot;
    this.#slotWord = (addr >>> 0) >>> 2;
    // Not waited for: the Worker takes its messages in order, so the slot is
    // set before any batch sent after it, and its answer sets what was seen.
    if (SP_CODE && this.#single) this.#rpc('slot', { word: this.#slotWord }).then((v) => { this.#seen = v; }, () => {});
  }

  async unwatch(queue) {
    if (!this.#queues.delete(queue) || this.#dead) return;
    await this.r('k_wq_del', queue, this.#slot).catch(() => {});
  }

  /** Where the wait slot is now. Taken before a call that may find nothing
   *  to do, then given to `changed`, so a move in between is not missed. */
  seq() {
    if (SP_CODE && this.#single) return this.#slot < 0 ? 0 : this.#seen;
    return this.#slot < 0 ? 0 : Atomics.load(this.words(), this.#slotWord);
  }

  /** When a watched queue has moved since `seq` (at the latest after 30 s,
   *  or when the kernel stops: the caller looks again). */
  changed(seq) {
    if (this.#dead || this.seq() !== seq) return Promise.resolve();
    return new Promise((res) => {
      this.#waiters.push([seq, res]);
      this.#tick();
    });
  }

  async #tick() {
    if (this.#ticking) return;
    this.#ticking = true;
    while (this.#waiters.length && !this.#dead) {
      const now = this.seq();
      const still = this.#waiters.filter(([s, res]) => s === now || void res());
      this.#waiters = still;
      if (!still.length) break;
      if (SP_CODE && this.#single) {
        // The Worker answers when the word has moved, or after 30 s.
        const v = await this.#rpc('slotWait', { seq: now }).catch(() => now);
        this.#seen = v ?? now;
        if (this.#seen === now) this.#waiters.splice(0).forEach(([, res]) => res());
        continue;
      }
      const w = Atomics.waitAsync(this.words(), this.#slotWord, now, 30000);
      if (w.async && (await w.value) === 'timed-out') this.#waiters.splice(0).forEach(([, res]) => res());
    }
    this.#ticking = false;
  }

  /** Every `changed` now: their callers look again. */
  kick() {
    this.#waiters.splice(0).forEach(([, res]) => res());
  }

  terminate() {
    this.#dead = true;
    this.#w.terminate();
    this.#fail(new Error('the kernel is stopped'));
    this.#waiters.splice(0).forEach(([, res]) => res());
  }
}
