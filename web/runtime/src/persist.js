// `Persist`: the guest's root kept across page loads by a store. Before boot
// the store's records are laid over the filesystem's sources; while the
// system runs, what the programs change is handed to the store, about a
// second after a change settles, every 30 s, and when the page is hidden or
// left. The diff itself is the runtime's Worker's (io-worker.js
// `persistDiff`): the tree as seeded against the tree now, without reading
// a file nobody wrote.
//
// A store is `{ load(), store({ put, drop }), reset() }`: `load` resolves
// the records it holds; `store` takes new or changed records (`put`) and the
// paths whose records go (`drop`, back to what the sources say); `reset`
// forgets them all. A record is `{ path, kind, stamp, ... }`:
//   'file'    `data` (bytes), `mode`, `mtime` (s)
//   'dir'     `mode`
//   'symlink' `target`
//   'mode'    `mode`: a file from the sources, its mode changed
//   'gone'    a path of the sources deleted, with everything under it
// `stamp` says what the record holds; the store keeps it as it is.

import { INTERNAL } from './internal.js';

/** Never kept: what the kernel makes, and scratch. */
export const ALWAYS_EXCLUDED = Object.freeze(['/tmp', '/proc', '/sys', '/dev', '/run']);

const POLL_MS = 250;

export class Persist extends EventTarget {
  #fs;
  #store;
  #opts;
  #known = [];
  #busy = null;
  #again = false;
  #timers = [];
  #stopped = false;
  #onHide = null;
  #stats = { saves: 0, stored: 0, dropped: 0, bytes: 0, last: null, error: null };

  /**
   * Keep `fs`'s root in `store` (see above). `exclude`: more paths never
   * kept (beside /tmp, /proc, /sys, /dev, /run and the mounts); `every`:
   * the longest between saves (ms, default 30000); `quiet`: how long after
   * a program's last change a save waits (default 1000); made before boot.
   */
  constructor(fs, store, { exclude = [], every = 30000, quiet = 1000 } = {}) {
    super();
    const fi = fs?.[INTERNAL];
    if (!fi?.persist) throw new TypeError('Persist takes a Filesystem as its first argument');
    for (const m of ['load', 'store']) if (typeof store?.[m] !== 'function') throw new TypeError(`Persist: the store has no ${m}()`);
    this.#fs = fs;
    this.#store = store;
    this.#opts = { exclude: exclude.map(String), every, quiet };
    this[INTERNAL] = {
      // The system's shutdown: one last save while the tree is there.
      shutdown: async () => {
        if (this.#io && !this.#stopped) await this.save().catch(() => {});
        this.#stop();
      },
    };
    fi.persist(this, {
      load: () => this.#load(),
      startArgs: () => ({ known: this.#known, exclude: this.#exclude() }),
      running: (sys) => this.#run(sys),
      stop: () => this.#stop(),
    });
  }

  /** `{ saves, stored, dropped, bytes, last, error }`: what it has done. */
  get stats() {
    return { ...this.#stats };
  }

  #exclude() {
    return [...ALWAYS_EXCLUDED, ...this.#opts.exclude, ...this.#fs.mounts().map((m) => m.path)];
  }

  async #load() {
    let records = [];
    try {
      records = (await this.#store.load()) ?? [];
    } catch (e) {
      this.#stats.error = String(e?.message ?? e);
      console.warn(`[gex] persist: nothing restored: ${e?.message ?? e}`);
    }
    this.#known = records.map((r) => [r.path, r.stamp]);
    return records;
  }

  #run(i) {
    this.#io = i.io();
    const io = this.#io;
    let first = 0;
    let last = 0;
    this.#timers.push(setInterval(() => {
      if (this.#stopped) return;
      io.call('persistChanged').then((moved) => {
        const now = performance.now();
        if (moved) {
          last = now;
          first ||= now;
        }
        if (first && (now - last >= this.#opts.quiet || now - first >= 5 * this.#opts.quiet)) {
          first = 0;
          this.save().catch(() => {});
        }
      }, () => {});
    }, POLL_MS));
    this.#timers.push(setInterval(() => this.save().catch(() => {}), this.#opts.every));
    if (globalThis.document) {
      this.#onHide = () => { if (document.hidden) this.save().catch(() => {}); };
      document.addEventListener('visibilitychange', this.#onHide);
      globalThis.addEventListener?.('pagehide', this.#onHide);
    }
  }
  #io = null;

  /** Store what changed now. Resolves `{ stored, dropped, bytes }` once the
   *  store has it; a save asked while one runs runs after it. */
  save() {
    if (!this.#io) return Promise.reject(new Error('persist: the system is not running'));
    if (this.#busy) {
      this.#again = true;
      return this.#busy;
    }
    this.#busy = (async () => {
      try {
        let r;
        do {
          this.#again = false;
          r = await this.#once();
        } while (this.#again && !this.#stopped);
        return r;
      } finally {
        this.#busy = null;
      }
    })();
    return this.#busy;
  }

  async #once() {
    const { put, drop } = await this.#io.call('persistDiff', { exclude: this.#exclude() });
    let bytes = 0;
    for (const r of put) bytes += r.data?.length ?? 0;
    if (put.length || drop.length) {
      try {
        await this.#store.store({ put, drop });
      } catch (e) {
        this.#stats.error = String(e?.message ?? e);
        this.dispatchEvent(Object.assign(new Event('error'), { error: e }));
        throw e;
      }
      await this.#io.call('persistStored', { put: put.map((r) => [r.path, r.stamp]), drop });
    }
    const s = this.#stats;
    s.saves++;
    s.stored += put.length;
    s.dropped += drop.length;
    s.bytes += bytes;
    s.last = Date.now();
    s.error = null;
    const r = { stored: put.length, dropped: drop.length, bytes };
    if (put.length || drop.length) this.dispatchEvent(Object.assign(new Event('stored'), r));
    return r;
  }

  /** Forget what is kept: the next load starts from the sources alone.
   *  Saving stops. */
  async reset() {
    this.#stop();
    await this.#busy?.catch(() => {});
    await this.#store.reset?.();
    this.#known = [];
  }

  #stop() {
    this.#stopped = true;
    for (const t of this.#timers) clearInterval(t);
    this.#timers = [];
    if (this.#onHide) {
      document.removeEventListener('visibilitychange', this.#onHide);
      globalThis.removeEventListener?.('pagehide', this.#onHide);
    }
  }
}
