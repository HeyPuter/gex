// Files and folders dropped on the page, copied into the guest: the page side
// of app.js's handle.drop (the copying is drop-worker.js's).
//
// What a drop carries is read in two steps. `dropItems` runs in the drop
// event's handler, before anything awaits: a DataTransfer's items can only
// be asked for their entries (webkitGetAsEntry) and handles
// (getAsFileSystemHandle) while that event is being dispatched, and are
// empty after it. `walk` then reads the folders' trees, which may take a
// while and does not need the event.

/**
 * What a drop holds, taken at once: `data` is a DataTransfer, a
 * DataTransferItemList, a FileList or an array of Files. A list of
 * {handle, entry, file} per dropped thing (each as the browser has it).
 */
export function dropItems(data) {
  const items = data?.items ?? (typeof DataTransferItemList === 'function' && data instanceof DataTransferItemList ? data : null);
  const out = [];
  if (items) {
    for (const item of items) {
      if (item.kind !== 'file') continue;
      // All three, now: each is gone once the event is over.
      const handle = typeof item.getAsFileSystemHandle === 'function' ? item.getAsFileSystemHandle() : null;
      handle?.catch?.(() => {});
      const entry = typeof item.webkitGetAsEntry === 'function' ? item.webkitGetAsEntry() : null;
      const file = item.getAsFile();
      out.push({ handle, entry, file });
    }
    return out;
  }
  for (const file of data?.files ?? data ?? []) out.push({ handle: null, entry: null, file });
  return out;
}

/** Whether `data` is something `dropItems` reads. */
export function isDropData(data) {
  if (!data || typeof data !== 'object') return false;
  if (typeof DataTransfer === 'function' && data instanceof DataTransfer) return true;
  if (typeof DataTransferItemList === 'function' && data instanceof DataTransferItemList) return true;
  if (typeof FileList === 'function' && data instanceof FileList) return true;
  return Array.isArray(data) && data.every((f) => typeof File === 'function' && f instanceof File);
}

/** A name the guest can hold: not empty, `.` or `..`, and without `/` or NUL. */
const goodName = (n) => n && n !== '.' && n !== '..' && !/[/\0]/.test(n);

/**
 * The tree under `items` (from `dropItems`): [{path, file?}], a folder
 * before what is in it, `path` relative to the drop's directory. Throws on
 * `signal`'s abort.
 */
export async function walk(items, signal) {
  const out = [];
  const add = (path, file) => {
    signal?.throwIfAborted();
    out.push(file ? { path, file } : { path });
  };
  async function fromHandle(h, path) {
    if (h.kind === 'file') return add(path, await h.getFile());
    add(path);
    for await (const [name, child] of h.entries()) {
      if (goodName(name)) await fromHandle(child, `${path}/${name}`);
    }
  }
  async function fromEntry(e, path) {
    if (e.isFile) return add(path, await new Promise((ok, no) => e.file(ok, no)));
    add(path);
    const reader = e.createReader();
    // readEntries gives a folder's entries a batch at a time, then nothing.
    for (;;) {
      const batch = await new Promise((ok, no) => reader.readEntries(ok, no));
      if (!batch.length) break;
      for (const c of batch) if (goodName(c.name)) await fromEntry(c, `${path}/${c.name}`);
    }
  }
  for (const it of items) {
    const h = await it.handle?.catch(() => null);
    if (h && goodName(h.name)) await fromHandle(h, h.name);
    else if (it.entry && goodName(it.entry.name)) await fromEntry(it.entry, it.entry.name);
    else if (it.file && goodName(it.file.name)) add(it.file.name, it.file);
  }
  return out;
}

/** Whether `p` is a guest directory a drop may go to: absolute, not `/`, no `..`. */
export const goodDir = (p) =>
  typeof p === 'string' && p.startsWith('/') && /[^/]/.test(p) && !p.split('/').includes('..') && !p.includes('\0');

/** The options handle.drop takes (app.js), and what each must be. */
export const DROP_OPTIONS = ['x', 'y', 'dir', 'fallback', 'signal', 'onProgress'];

/**
 * handle.drop's arguments checked, before it does anything: throws an Error
 * that names the argument and what it must be.
 */
export function checkDrop(data, opts) {
  const show = (v) => {
    if (typeof v === 'function') return 'a function';
    try {
      const s = JSON.stringify(v);
      if (s !== undefined) return s.length > 60 ? `${s.slice(0, 57)}...` : s;
    } catch { /* below */ }
    return String(v);
  };
  if (!isDropData(data)) throw new Error(`drop(${show(data)}): a DataTransfer, its items, a FileList or an array of Files`);
  if (opts === undefined || opts === null) return;
  if (typeof opts !== 'object') throw new Error(`drop(data, ${show(opts)}): the options are an object`);
  const bad = (k, want) => new Error(`drop(data, { ${k}: ${show(opts[k])} }): ${want}`);
  for (const k of Object.keys(opts)) {
    if (!DROP_OPTIONS.includes(k)) throw new Error(`drop(data, { ${k} }): no such option (the options: ${DROP_OPTIONS.join(', ')})`);
  }
  const { x, y, dir, fallback, signal, onProgress } = opts;
  if (x !== undefined && !Number.isFinite(x)) throw bad('x', "a number, the drop event's clientX");
  if (y !== undefined && !Number.isFinite(y)) throw bad('y', "a number, the drop event's clientY");
  if ((x === undefined) !== (y === undefined)) {
    const [k, other] = x === undefined ? ['y', 'x'] : ['x', 'y'];
    throw bad(k, `needs ${other} too (the drop event's clientX and clientY)`);
  }
  if (dir !== undefined && dir !== null && !goodDir(dir)) throw bad('dir', 'an absolute guest path without .., not /');
  if (fallback !== undefined && fallback !== null && !goodDir(fallback)) throw bad('fallback', 'an absolute guest path without .., not /');
  if (signal !== undefined && !(typeof AbortSignal === 'function' && signal instanceof AbortSignal)) throw bad('signal', 'an AbortSignal');
  if (onProgress !== undefined && typeof onProgress !== 'function') throw bad('onProgress', 'a function');
}

/**
 * The drop Worker of a running system `sys` (runtime.js's `onSystem`): one
 * per page, on a kernel stack of its own taken from the host when the first
 * drop comes.
 */
/** How long a drop waits for a kernel stack to come free. */
const STACK_WAIT_MS = 30000;

export class Dropper {
  #sys;
  #worker = null;
  #ready = null;
  #next = 1;
  #jobs = new Map();

  constructor(sys) {
    this.#sys = sys;
  }

  #start(signal) {
    this.#ready ??= (async () => {
      const host = this.#sys.host;
      let stackTop;
      // A stack frees when a guest Worker ends; until one is free, wait,
      // a while.
      for (const t0 = performance.now(); ;) {
        stackTop = host.takeKernelStack('drop');
        if (stackTop !== undefined) break;
        if (performance.now() - t0 > STACK_WAIT_MS) throw new Error(`no kernel stack came free in ${STACK_WAIT_MS / 1000} s`);
        await new Promise((r) => setTimeout(r, 100));
      }
      const w = new Worker(new URL('./drop-worker.js', import.meta.url), { type: 'module', name: 'drop' });
      w.onmessage = ({ data: m }) => {
        const job = this.#jobs.get(m.id);
        if (!job) return;
        if (m.progress) {
          job.onProgress?.(m.progress);
          return;
        }
        this.#jobs.delete(m.id);
        if (m.error) job.reject(m.aborted ? new DOMException(m.error, 'AbortError') : new Error(m.error));
        else job.resolve(m.done ?? m.placed);
      };
      w.onerror = (e) => {
        for (const job of this.#jobs.values()) job.reject(new Error(`the drop Worker failed: ${e.message ?? e}`));
        this.#jobs.clear();
      };
      w.postMessage({ spec: this.#sys.kernel, stackTop });
      this.#worker = w;
    })();
    // A failed start is tried again by the next drop.
    this.#ready.catch(() => { this.#ready = null; });
    if (!signal) return this.#ready;
    // An abort while it waits ends this drop's wait, not the start.
    return Promise.race([this.#ready, new Promise((_, no) => {
      if (signal.aborted) no(signal.reason);
      signal.addEventListener('abort', () => no(signal.reason), { once: true });
    })]);
  }

  /** Send `m` to the Worker; resolves with its answer. */
  async #ask(m, { signal, onProgress } = {}) {
    await this.#start(signal);
    signal?.throwIfAborted();
    const id = this.#next++;
    const abort = () => this.#worker.postMessage({ abort: id });
    signal?.addEventListener('abort', abort, { once: true });
    try {
      return await new Promise((resolve, reject) => {
        this.#jobs.set(id, { resolve, reject, onProgress });
        this.#worker.postMessage({ id, ...m });
      });
    } finally {
      signal?.removeEventListener('abort', abort);
    }
  }

  /** Copy `entries` (from `walk`) into a directory: `dir`, or a new
   *  `root`/N. Resolves {path, names, files, bytes, ms}: `names` the
   *  top-level names as they went in (numbered where `dir` had one). */
  copy(entries, { dir = null, root = '/tmp/drops', signal, onProgress } = {}) {
    return this.#ask({ entries, dir, root }, { signal, onProgress });
  }

  /** Move the top-level `names` of directory `from` into `into` (made if
   *  missing), a name taken there getting " (2)", " (3)", ... Resolves
   *  {paths}: where each went. */
  move(from, names, into) {
    return this.#ask({ move: { from, names, into } });
  }
}
