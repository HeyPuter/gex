// The page's side of the global kernel: its exports called by name through
// the kernel-call Worker (kernel-worker.js), and its wait queues waited on
// here, with Atomics.waitAsync on the kernel's own memory, which the page
// shares.

const ERRNO = { 1: 'EPERM', 2: 'ENOENT', 9: 'EBADF', 11: 'EAGAIN', 13: 'EACCES', 17: 'EEXIST', 18: 'EXDEV', 20: 'ENOTDIR', 21: 'EISDIR', 22: 'EINVAL', 28: 'ENOSPC', 36: 'ENAMETOOLONG', 39: 'ENOTEMPTY', 40: 'ELOOP', 111: 'ECONNREFUSED' };

/** A negated errno `n` as an Error that says `what`. */
export function errno(n, what) {
  const code = ERRNO[-n] ?? `errno ${-n}`;
  return Object.assign(new Error(`${code}: ${what}`), { code, errno: -n });
}

async function makeWorker() {
  const url = new URL('./kernel-worker.js', import.meta.url);
  if (typeof Worker === 'function') return new Worker(url, { type: 'module', name: 'gex-kernel' });
  const { Worker: NodeWorker } = await import('node:worker_threads');
  return new NodeWorker(url);
}

export class Kernel {
  #w; #next = 1; #calls = new Map();
  #memory;
  /** This side's wait slot, made at the first `watch`. */
  #slot = -1;
  #slotWord = 0;
  #names = new Set();

  /** A Worker with the kernel `spec` ({ module, memory, pageFs }). */
  static async start(spec) {
    const k = new Kernel(await makeWorker(), spec.memory);
    k.#names = new Set(await k.#rpc('start', { spec }));
    return k;
  }

  /** Whether this kernel build has export `f`. */
  has(f) {
    return this.#names.has(f);
  }

  constructor(worker, memory) {
    this.#w = worker;
    this.#memory = memory;
    const on = (m) => {
      const c = this.#calls.get(m?.call);
      if (!c) return;
      this.#calls.delete(m.call);
      if (m.error) c.reject(Object.assign(new Error(m.error.message), { workerStack: m.error.stack }));
      else c.resolve(m.value);
    };
    const fail = (e) => {
      const err = new Error(`the kernel Worker failed: ${e?.message ?? e}`);
      for (const c of this.#calls.values()) c.reject(err);
      this.#calls.clear();
    };
    if (worker.addEventListener) {
      worker.addEventListener('message', (e) => on(e.data));
      worker.addEventListener('error', fail);
    } else {
      worker.on('message', on);
      worker.on('error', fail);
    }
  }

  #rpc(op, args) {
    const call = this.#next++;
    return new Promise((resolve, reject) => {
      this.#calls.set(call, { resolve, reject });
      this.#w.postMessage({ call, op, ...args });
    });
  }

  /** A batch (kernel-worker.js): `[{ r, out }]`, one per call made. */
  calls(list) {
    // A view of part of a larger buffer (a file in a tar) is sent as a copy
    // of that part: posted as it is, the whole buffer would be copied.
    for (const c of list) {
      for (const x of c.a ?? []) {
        const v = x?.in ?? x?.ptr;
        if (v instanceof Uint8Array && v.byteLength !== v.buffer.byteLength && !(v.buffer instanceof SharedArrayBuffer)) {
          if (x.in !== undefined) x.in = v.slice();
          else x.ptr = v.slice();
        }
      }
    }
    return this.#rpc('calls', { list });
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

  /** Wake this side when wait queue `queue` moves. */
  async watch(queue) {
    if (!queue) return;
    if (this.#slot < 0) {
      const [{ r: slot }, { r: addr }] = await this.calls([{ f: 'k_ws_alloc' }, { f: 'k_ws_addr', a: [{ ref: 0 }] }]);
      if (slot < 0) throw new Error('the runtime found no free wait slot in the kernel');
      this.#slot = slot;
      this.#slotWord = (addr >>> 0) >>> 2;
    }
    await this.r('k_wq_add', queue, this.#slot);
  }

  async unwatch(queue) {
    if (queue && this.#slot >= 0) await this.r('k_wq_del', queue, this.#slot);
  }

  /** Where this side's wait slot is now: what `sleep` waits to move. */
  seq() {
    return this.#slot < 0 ? 0 : Atomics.load(this.words(), this.#slotWord);
  }

  /** Until a watched queue moves from `seq`, `ms` pass, or `kicked` settles. */
  async sleep(seq, ms, kicked) {
    if (this.#slot < 0) return kicked;
    const w = Atomics.waitAsync(this.words(), this.#slotWord, seq, ms);
    if (w.async) await Promise.race([w.value, kicked]);
  }

  terminate() {
    this.#w.terminate();
  }
}
