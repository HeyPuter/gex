// The page's half of a guest system: one Worker per guest process.
//
// Each Worker has its own `WebAssembly.Memory`, so nothing collides and
// nothing is snapshotted. Shared state -- the process table, filesystem, pipes
// -- lives in the global kernel, a second wasm module every Worker
// instantiates against one shared memory, so a syscall stays a call rather
// than a message back through here. This file starts pid 1, makes a Worker per
// fork, relays I/O, and holds the run open until pid 1 exits.

// Narration under node goes to stderr: node's console.debug is stdout, the
// guest's own output there. In a page it stays on the debug console.
function narrate(text) {
  // Debug output, off unless GEX_LOG (`?log=1`) asks for it (log.mjs).
  if (!logOn('debug')) return;
  const stderr = typeof process !== 'undefined' ? process.stderr : null;
  if (stderr?.write) stderr.write(`${text}\n`);
  else console.debug(text);
}

/**
 * Announce a crash to an automated watcher. These failures are caught (the
 * spawn promise is rejected and handled), so no `error` event fires and a test
 * that already matched its output has stopped reading. A hook, not prose
 * scraping, which fails open. See task 146.
 */
import { single } from './single.mjs';
import { HELPER_STACK_COUNT } from './lazy-fetch.mjs';
import { logOn, logQuiet } from './log.mjs';

/** A line gex says by default (log.mjs): to stderr under Node, else the
 *  console's info level. */
function inform(text) {
  if (logQuiet()) return;
  const fs = typeof process !== 'undefined' ? process.getBuiltinModule?.('fs') : null;
  if (fs?.writeSync) {
    try { fs.writeSync(2, `${text}\n`); return; } catch {}
  }
  console.info(text);
}

// `?memories=1` (or GEX_MEMORIES), read once at load: a test harness's view
// of each threaded process's memory by pid, `globalThis.__gexMemories`, what
// it reads a guest word from while the process's own Workers are busy in
// wasm (a game's frame counter). Held until the process ends; off, nothing
// is registered and nothing is kept.
const MEMORIES = !!(globalThis.GEX_MEMORIES || globalThis.process?.env?.GEX_MEMORIES || (typeof location !== 'undefined' && new URLSearchParams(location.search).get('memories') === '1'));
function forgetMemory(pid) {
  if (MEMORIES) globalThis.__gexMemories?.delete(Number(String(pid).split(':')[0]));
}

// A library or image loaded for the first time this session, said once
// rather than once per process (each Wine process maps the same dozen DLLs).
// Every Worker says what it installs on the 'gex-fetch' channel
// (fetch-note.mjs `noteStatus`); this side keeps the set.
const libsSeen = new Set();
let libChannel = null;
function watchLibraries() {
  if (libChannel || typeof BroadcastChannel !== 'function') return;
  libChannel = new BroadcastChannel('gex-fetch');
  // Under Node an open channel keeps the event loop alive.
  libChannel.unref?.();
  libChannel.onmessage = (ev) => {
    const st = ev.data?.status;
    if (!st || (st.kind !== 'module' && st.kind !== 'unit') || !st.path) return;
    const path = String(st.path).replace(/^wasm(dll|prog):/, '');
    if (libsSeen.has(path)) return;
    libsSeen.add(path);
    inform(`[gex] loaded ${path}${st.kind === 'unit' ? (st.hit ? ' (with its unit)' : ' (no unit)') : ''}`);
  };
}

function crashed(what) {
  try {
    globalThis.GEX_ON_CRASH?.(what);
  } catch {
    // A reporter that throws must not add a failure to the one it reports.
  }
}

/**
 * The buffers in a fork record, so `postMessage` transfers them instead of
 * copying. Views on a `SharedArrayBuffer` are skipped -- shared buffers cannot
 * be transferred, and the guest's memory is shared when threads are on; the
 * regions are `slice`d, so each owns its buffer and transferring one cannot
 * detach another.
 */
function forkTransfer(child) {
  const out = [];
  const add = (v) => {
    if (!ArrayBuffer.isView(v)) return;
    const b = v.buffer;
    if (!(b instanceof ArrayBuffer) || out.includes(b)) return;
    out.push(b);
  };
  for (const r of child?.image ?? []) add(r?.bytes);
  add(child?.vmaBlob);
  add(child?.fds);
  add(child?.pages);
  add(child?.shared);
  return out;
}

/// Where a pooled Worker's early messages are kept until somebody owns it.
const EARLY = Symbol('gex.early');
/// The keys of the page's modules a pooled Worker was sent while it waited
/// (`#poolModules`), so its start message carries only the rest.
const POOLED = Symbol('gex.pooled');
/// A pooled Worker's global-kernel stack slice, taken when it was sent
/// `preattach` (keyed `pool:N` until a process or thread takes the Worker).
const STACK_KEY = Symbol('gex.stackKey');
/// A pooled Worker has said `ready` (its handler is on): it can be sent
/// `preattach`.
const READY = Symbol('gex.ready');
/// A process Worker's listeners (`#spawn`), taken off when it is given back.
const LISTENING = Symbol('gex.listening');

/**
 * Which processes and threads (`pid` or `pid:tid`) a named Worker has run,
 * in order, as `globalThis.__gexWorkers` (Worker name -> keys): a pooled
 * Worker is named before it has a process, and a profiler or trace lists its
 * thread only by that name. A Worker without a name is not recorded.
 */
function workerRan(w, key) {
  const name = w?.gexName;
  if (!name) return;
  const m = (globalThis.__gexWorkers ??= new Map());
  const ran = m.get(name);
  if (ran) ran.push(String(key));
  else m.set(name, [String(key)]);
}
/// How many processes a Worker has run after its first (`#giveBack`).
const REUSES = Symbol('gex.reuses');
/** How many processes one Worker runs before it is ended anyway, so a leak
 *  in a realm that is given back cannot grow without end; and how many
 *  given-back Workers the pool keeps beyond the new ones it is topped up to. */
const REUSE_MAX = 256;
const GIVEN_BACK_MAX = 2;
/** How many exited processes' memories are kept for fork children, and how
 *  many bytes they may hold together: memory the page held anyway while
 *  those processes ran, not more. */
const RECYCLE_COUNT = 4;
/** A test switch (tests/spawn-release): whether a running fork child's
 *  record, its parent's modules with it, can be collected once the child's
 *  Worker has it. Node only, with `--expose-gc`. */
const TEST_FORK_RECORDS = !!globalThis.process?.env?.GEX_TEST_FORK_RECORDS;
const RECYCLE_BYTES = 768 << 20;

/** How far a fork record's image reaches: what its child's memory grows to. */
function imageExtent(child) {
  let n = 0;
  for (const r of child?.image ?? []) {
    n = Math.max(n, r.end ?? 0, r.at + (r.len ?? r.bytes?.byteLength ?? 0));
  }
  return n;
}
/** How many content-keyed modules (`wasm:`) the page keeps for every
 *  process, and how many bytes of wasm they may add up to (their compiled
 *  code is larger still; python3.13 alone is 24 MiB of wasm). */
const PAGE_MODULES = 64;
const PAGE_MODULE_BYTES = 192 << 20;
/** The most wasm the page-wide unit cache (`#unitModules`) keeps compiled. */
const UNIT_MODULE_BYTES = 256 << 20;
/** The most wasm one process's relay cache (`#moduleCache`) keeps: a pc
 *  translated again and again (a dispatch that does not converge; Paint's
 *  startup stall, scratch/P/stall85) made a fresh key each time, and an
 *  unbounded cache held every one, 10 GiB of renderer within minutes. Past
 *  it the oldest are dropped from the cache (Workers that installed them
 *  keep theirs; a thread started later compiles what it needs). */
const PROCESS_MODULE_BYTES = 256 << 20;
/** And the most modules it, or the page's unit cache, keeps: each module the
 *  main thread holds lengthens every collection there, and those hold up
 *  compiles on every Worker of the page. Not with `?relayall=1`. */
const RELAY_MODULES = 512;
/** A registry key's module size: `LENGTH:HASH:HASH` (or `u:LENGTH:...`). */
const regKeyBytes = (k) => Number(String(k).replace(/^u:/, '').split(':')[0]) || 0;
/** A `wasm:` key's module size: the key is `wasm:LENGTH:HASH:HASH`. */
const keyBytes = (k) => Number(String(k).split(':')[1]) || 0;

export class ProcessHost {
  #makeWorker; #shim; #guest; #kernel; #extra;
  /// The other memory flavour's runtime, `{ shim, guest, kernel }` (a
  /// kernel module, on the same kernel memory), for a page whose processes
  /// do not all have one kind of memory: in a 64-bit memory's page
  /// (`?mem64=1`), a program that is a wasm32 module (W64's wineserver,
  /// docs/wine-native-x64.md §4) or a 32-bit ELF runs in a 32-bit memory.
  /// What a process gets: the exec record says (`mem64`, vfork.rs
  /// `exec_memory`), a fork child has its parent's, and anything else the
  /// page's. As the X server's (runtime.js), which takes the wasm32 kernel.
  #other; #otherShim = null; #otherLoading = null;
  /// Each process's memory flavour, by pid, where it is not the page's.
  #flavourOf = new Map();
  /// Per Worker, a shared counter of the output messages this host has
  /// delivered. A Worker about to publish its process's exit waits until the
  /// count reaches what it sent (see `drainOutput` in worker.mjs): output from
  /// different Workers crosses different ports, which nothing orders, so a
  /// parent that waited for its child could otherwise print first.
  #outputAcks = new WeakMap();
  #outputAck(w) {
    // Fibers (the single-thread build) deliver in order through one queue,
    // and could not wait for an ack on the thread that delivers it anyway.
    if (single.on || typeof SharedArrayBuffer === 'undefined') return undefined;
    const a = new Int32Array(new SharedArrayBuffer(4));
    this.#outputAcks.set(w, a);
    return a.buffer;
  }
  #onOutput; #onFork; #onExit; #readStdin; #onOther;
  #workers = new Map();
  /** The wineserver's door, once published (`doorPublish`). */
  #door = null;
  #pending = new Map();
  #first = null;
  #forked = 0;
  // Per-process compiled-module cache: `procPid -> Map<registry index,
  // WebAssembly.Module>`. A process's threads share one guest memory and so one
  // registry, so a module a sibling compiled is reusable by the rest. Relayed
  // here between the Workers of a process, and handed to a thread when it starts
  // so it need not recompile what its siblings already did. Keyed by the process
  // (the first segment of a `pid:tid` worker key); forks copy the registry into
  // a separate memory, so they do NOT share this -- the fork path carries the
  // parent's compiled modules itself. See `Gex#sharedModules`.
  #moduleCache = new Map();
  // Modules whose compiled code depends on their bytes alone (`wasm:` keys:
  // native wasm32 programs, by content), for every process of the page: an
  // exec'd program another process already ran is not compiled again. Kept
  // to the most recent PAGE_MODULES and PAGE_MODULE_BYTES. A hit is by
  // content (length plus two 32-bit hashes of every byte), never by path, so
  // a file apt replaced in place is compiled afresh.
  #pageModules = new Map();
  // Compiled translation units (content keys `len:a:b`) for every process of
  // the page, with `extra.unitCache`: a process served units another one
  // built instantiates the other's compiled code instead of compiling the
  // same bytes again. Most recent first out, to UNIT_MODULE_BYTES of wasm.
  #unitModules = new Map();
  #unitBytes = 0;
  /** `?filltime=1`: at most once a minute, how many modules the relay
   *  holds on this thread, and how many it has been handed. */
  #relayCensus() {
    this.#relayed++;
    const now = performance.now();
    if (now < this.#relayCensusAt) return;
    this.#relayCensusAt = now + 60000;
    let n = 0, bytes = 0;
    for (const c of this.#moduleCache.values()) { n += c.size; bytes += c.bytes ?? 0; }
    console.warn(`[host] relay holds ${n} process modules (${(bytes / 1048576).toFixed(0)} MiB), ${this.#pageModules.size} programs, `
      + `${this.#unitModules.size} units (${(this.#unitBytes / 1048576).toFixed(0)} MiB); ${this.#relayed} handed to it so far`);
  }
  #relayed = 0;
  #relayCensusAt = 0;
  get #relayModules() { return this.#extra?.relayAll ? Infinity : RELAY_MODULES; }

  /** What a starting Worker gets: its process's modules and the page's. */
  #sharedFor(pid, withProcess, w = null) {
    const own = withProcess ? [...(this.#moduleCache.get(String(pid).split(':')[0])?.entries() ?? [])] : [];
    // And the page's translation units (empty unless `extra.unitCache`),
    // less those the Worker was given while it waited in the pool.
    const page = [...this.#pageModules.entries(), ...this.#unitModules.entries()];
    const had = w?.[POOLED];
    return [...own, ...(had ? page.filter(([k]) => !had.has(k)) : page)];
  }

  /** Give a pooled Worker the page's modules now, while it waits: in the
   *  start message they were most of a fork's hand-off (about 860 on the
   *  demo after a few commands; 2.5 ms to clone in, 1.1 ms more in transit
   *  and 0.7 ms to register, of every child's start). */
  #poolModules(w) {
    const entries = [...this.#pageModules.entries(), ...this.#unitModules.entries()];
    w[POOLED] = new Set(entries.map(([k]) => k));
    if (entries.length) w.postMessage({ kind: 'pooledModules', entries });
  }

  /**
   * Have a pooled Worker make its runtime's instances (kernel, shim, guest)
   * on a fresh memory while it waits (worker.mjs `preattach`), for an exec'd
   * posix_spawn child to start on. On the kernel stack slice its start
   * message will name, taken now. Only with a global kernel whose stacks are
   * known, and shared memory (the build every Worker of a page is); nothing
   * of a process is in it, so a Worker taken for anything else drops it.
   * About what a fresh process's memory holds before it runs: the shim's
   * data and the Worker's shim stack.
   *
   * One pooled Worker at a time has it (`#preattachOne`), and only an exec'd
   * child is given that one (`#takeWorker`): its memory is grown to 177 MiB
   * at once, and a Worker taken for a thread or a fork dropped it unfreed --
   * the Worker then sits in wasm or a wait, its isolate never collects, and
   * the dropped memory stays in the renderer's VmData (one program: 24 of them
   * alive at its window, 4.15 GiB; a desktop shell: 58, 10 GiB).
   */
  #preattach(w) {
    if (!this.#kernel || !this.#stacks || !this.#extra?.shared || w[STACK_KEY]) return;
    const key = `pool:${++this.#poolSeq}`;
    let stack;
    try {
      stack = this.#nextKernelStack(key);
    } catch {
      return;
    }
    w[STACK_KEY] = key;
    this.#shimModule().then((shim) => w.postMessage({
      kind: 'preattach',
      shim,
      guest: this.#guest,
      kernel: this.#kernel,
      kernelStack: stack,
      shared: true,
      // The process memory's index type: the page's (`?mem64=1`, run-node's
      // GEX_MEM64), which every pooled Worker has (`#other`).
      mem64: !!this.#extra?.mem64,
      pageFlavours: this.#pageFlavours(),
    }));
  }
  #poolSeq = 0;

  /** A runtime made ahead in one pooled Worker, unless one already has it. */
  #preattachOne() {
    if (this.#warm.some((w) => w[STACK_KEY] !== undefined)) return;
    const w = this.#warm.find((x) => x[READY]);
    if (w) this.#preattach(w);
  }

  /** The kernel stack slice for the Worker `w` starting `key`: the one it
   *  was given in the pool (`#preattach`), now `key`'s, or a new one. */
  #stackFor(w, key) {
    const pooled = w[STACK_KEY];
    if (pooled === undefined) return this.#nextKernelStack(key);
    delete w[STACK_KEY];
    const slice = this.#stackOf.get(pooled);
    this.#stackOf.delete(pooled);
    this.#stackOf.set(key, slice);
    return this.#stacks.area + slice * this.#stacks.size;
  }

  /** A module the page just kept, to the Workers waiting in the pool. */
  #poolModule(key, mod) {
    for (const w of this.#warm) {
      if (!w[POOLED] || w[POOLED].has(key)) continue;
      w[POOLED].add(key);
      w.postMessage({ kind: 'pooledModules', entries: [[key, mod]] });
    }
  }
  /// When each process was asked for. See `#spawn`.
  #asked = new Map();
  /** A process whose first Worker's thread went with the old program when
   *  another thread called `execve` (Linux's de_thread): the process runs on,
   *  and ends, in that thread's Worker. Per pid, whichever of the two exits
   *  arrives first waits here for the other: `{ resolve }` from the first
   *  Worker, `{ status }` from the thread Worker that carried it. */
  #carried = new Map();
  #threads = 0;
  // The global kernel's per-Worker stacks, as the first Worker reported them.
  // Each Worker's kernel instance has its own `__stack_pointer` global, all
  // pointing into the shared memory, so each needs its own slice or two Workers
  // in the kernel overwrite each other's locals.
  #stacks = null;
  #stacksTaken = 0;
  // Slices handed back by ended Workers, and which slice each live Worker
  // holds: a run of many short-lived threads uses as many slices as run at
  // once, not as many as it ever made.
  #stacksFree = [];
  /// The shim compiling, once, so no Worker waits on a second compile.
  #shimCompiling = null;
  /// Workers made before anybody asks, so a fork's ~170ms of Worker startup is
  /// off the path. One spare suits a shell; a pipeline wants more, and an empty
  /// pool falls back to making a Worker inline.
  #warm = [];
  #warmWanted = 3;
  /// Exited processes' memories, zeroed (`Gex#recycleMemory`), for fork
  /// children to adopt into: their pages are touched, where a new memory's
  /// first touch was most of the cost of copying a fork's image in. Kept
  /// newest last, up to RECYCLE_COUNT and RECYCLE_BYTES.
  #recycled = [];
  /** TEST_FORK_RECORDS: each fork child's record, weakly, by pid. */
  #forkRecords = new Map();
  /** TEST_FORK_RECORDS: said a moment after the child's Worker has its
   *  record. A scope of its own, so the timer holds nothing of the caller's. */
  #checkForkRecord(pid) {
    const record = this.#forkRecords.get(pid);
    if (!record) return;
    this.#forkRecords.delete(pid);
    setTimeout(() => {
      globalThis.gc?.();
      console.error(`[test] pid ${pid}: fork record ${record.deref() ? 'kept' : 'released'} while it runs`);
    }, 500);
  }
  #warmMade = 0;
  #warmUsed = 0;
  #stackOf = new Map();

  constructor({
    makeWorker,
    shim,
    guest,
    kernel = null,
    // Merged into every Worker's start message: everything a process needs
    // that is not the program -- GPU channel, proxy address, terminal size.
    extra = {},
    onOutput = () => {},
    onFork = () => {},
    // Every process's exit message, whole. `start` resolves with pid 1's
    // status; this hands a host the rest of what a Worker reports on exit.
    onExit = () => {},
    readStdin = null,
    // Anything a Worker posts that this file has no meaning for.
    onMessage = () => {},
    // The other memory flavour's `{ shim, guest, kernel }` (`#other`), or
    // an async function that fetches it.
    other = null,
  }) {
    this.#other = other;
    this.#makeWorker = makeWorker;
    this.#shim = shim;
    this.#guest = guest;
    this.#kernel = kernel;
    this.#onOutput = onOutput;
    this.#onFork = onFork;
    this.#onExit = onExit;
    this.#readStdin = readStdin;
    this.#extra = extra;
    this.#onOther = onMessage;
  }

  /**
   * The shim, compiled once and shared to every Worker. `postMessage` clones a
   * `Uint8Array` (8.7 MB each) but clones a `WebAssembly.Module` by reference.
   */
  #shimModule() {
    if (this.#shim instanceof WebAssembly.Module) return Promise.resolve(this.#shim);
    // Async, not a preference: Chromium refuses a synchronous compile over 8 MB
    // on the main thread and the shim is 8.7. Node allows it, so the sync form
    // passed every test here and killed the page at boot.
    this.#shimCompiling ??= WebAssembly.compile(this.#shim).then((m) => (this.#shim = m));
    return this.#shimCompiling;
  }

  /** Whether process `pid` (or a thread's `pid:tid`) has a 64-bit memory. */
  #mem64(pid) {
    const f = this.#flavourOf.get(Number(String(pid).split(':')[0]));
    return f ?? !!this.#extra?.mem64;
  }

  /** The page's memory flavours, for the shim (vfork.rs
   *  `gex_set_page_flavours`): bit 0 its own is a 64-bit memory, bit 1 it
   *  runs the other too (`#other`), so that an exec may ask for that one
   *  (docs/mem64.md §5). */
  #pageFlavours() {
    return (this.#extra?.mem64 ? 1 : 0) | (this.#other && this.#kernel ? 2 : 0);
  }

  /** Whether `pid` runs on the other flavour's runtime (`#other`). */
  #foreign(pid) {
    return this.#mem64(pid) !== !!this.#extra?.mem64;
  }

  /** What a Worker of process `pid` starts on: `{ shim, guest, kernel,
   *  mem64 }`, the shim compiled. */
  async #runtimeFor(pid) {
    const mem64 = this.#mem64(pid);
    if (!this.#foreign(pid)) {
      return { shim: await this.#shimModule(), guest: this.#guest, kernel: this.#kernel ?? undefined, mem64 };
    }
    // Fetched on first use when given as a function: few pages run one.
    const o = typeof this.#other === 'function' ? await (this.#otherLoading ??= this.#other()) : this.#other;
    if (!o) throw new Error(`process ${pid} needs a ${mem64 ? '64' : '32'}-bit memory, which this page does not run`);
    this.#otherShim ??= o.shim instanceof WebAssembly.Module ? Promise.resolve(o.shim) : WebAssembly.compile(o.shim);
    return {
      shim: await this.#otherShim,
      guest: o.guest,
      kernel: this.#kernel ? { ...this.#kernel, module: o.kernel } : undefined,
      mem64,
    };
  }

  /** How many children the guest has forked. */
  get forked() { return this.#forked; }

  /** How many threads its processes have started in Workers of their own. */
  get threads() { return this.#threads; }

  /** Every live process, by pid. Pid 1 is the one `start` was given. */
  get pids() { return [...this.#workers.keys()]; }

  /**
   * Run `elf` as pid 1 and resolve with its exit status. Children are stopped
   * when it exits -- a page done with a program is done with its daemons.
   */
  async start({ elf, files = {}, args = [], opts = {}, transfer = [] }) {
    watchLibraries();
    // One of the few default lines: what pid 1 runs. Its execs say their own
    // (the shim's `do_exec`).
    const what = [opts.argv0, ...args].filter((a) => typeof a === 'string' && a).join(' ');
    inform(`[gex] pid 1: start ${what.length > 200 ? `${what.slice(0, 200)}...` : what || '(program)'}`);
    const status = await this.#spawn(1, {
      kind: 'run',
      elf,
      files,
      args,
      opts,
      transfer,
    });
    this.stop();
    return status;
  }

  /** Deliver a line of standard input to pid 1. */
  pushStdin(bytes) {
    this.#first?.postMessage({ kind: 'stdin', bytes });
  }

  /** Say there will be no more input. */
  endStdin() {
    this.#first?.postMessage({ kind: 'eof' });
  }

  /** Tell every process the terminal changed size. */
  resize(rows, cols) {
    for (const w of this.#workers.values()) w.postMessage({ kind: 'winsize', rows, cols });
  }

  /** Stop every Worker, whatever it is doing. */
  stop() {
    for (const w of this.#workers.values()) w.terminate();
    this.#workers.clear();
    // Including the warm ones nobody claimed.
    for (const w of this.#warm) w.terminate();
    this.#warm = [];
  }

  /**
   * One process in one Worker. `msg` is `run` (load a program) or `adopt` (a
   * forked child, from its parent's fork record).
   */
  #spawn(pid, msg) {
    // When this process was asked for, to measure the gap before it runs.
    this.#asked.set(pid, performance.now());
    return new Promise((resolve, reject) => {
      // A pooled Worker holds the page flavour's runtime and modules: one of
      // the other flavour starts in a Worker of its own.
      const w = this.#foreign(pid) ? this.#makeWorker() : this.#takeWorker(msg.kind === 'adopt' && !!msg.child?.exec);
      this.#workers.set(pid, w);
      workerRan(w, pid);
      if (pid === 1) this.#first = w;
      const onEvent = (e) => this.#onMessage(pid, w, e.data, resolve, reject);
      const onMsg = (m) => this.#onMessage(pid, w, m, resolve, reject);
      w.addEventListener?.('message', onEvent);
      w.on?.('message', onMsg);
      const fail = (e) => {
        this.#workers.delete(pid);
        reject(e instanceof Error ? e : new Error(e?.message ?? String(e)));
      };
      w.addEventListener?.('error', fail);
      w.on?.('error', fail);
      w[LISTENING] = { onEvent, onMsg, fail };
      this.#pending.set(pid, { ...msg, pid });
      const kind = msg.kind;
      // The listeners above live as long as the process and share this
      // scope: holding `msg` here kept a fork's whole record (every module
      // its parent had, with their metadata) on this thread for the child's
      // life, and a child's record names its parent's modules too -- some
      // twenty thousand modules on a desktop's page. `#pending` has it until
      // the Worker is ready.
      msg = null;
      // `#pending` pins the whole fork record (image bytes included) until the
      // child reports ready. A child that never starts leaves ~19 MB pinned.
      // See task 145.
      if (this.#extra?.debugExit) {
        let held = 0;
        for (const r of this.#pending.values()) {
          for (const g of r.image ?? []) held += g.bytes?.byteLength ?? 0;
        }
        console.error(
          `[pending] ${this.#pending.size} records holding ${held} bytes`,
        );
      }
      // One line per process, to `debug`: a process that never starts leaves
      // no other trace -- the fork is logged by the parent, the exit by the
      // child. #137.
      narrate(`[host] pid ${pid}: worker taken, ${kind} held for ready`);
      // And now whatever it said while it was still in the pool.
      const held = w[EARLY];
      if (held) {
        delete w[EARLY];
        w.removeEventListener?.('message', held.buf);
        w.off?.('message', held.buf);
        for (const m of held.early) this.#onMessage(pid, w, m, resolve, reject);
      }
    });
  }

  /**
   * A Worker, from the pool if one is warm. Tops up after the take, so the
   * Worker started now serves the next process, not this one.
   */
  #takeWorker(exec = false) {
    // An exec'd child takes the Worker with a runtime made ahead, anything
    // else one without (`#preattach`), whichever comes first otherwise.
    let i = this.#warm.findIndex((x) => (x[STACK_KEY] !== undefined) === exec);
    if (i < 0 && this.#warm.length) i = 0;
    const w = i < 0 ? null : this.#warm.splice(i, 1)[0];
    if (w) this.#warmUsed++;
    const got = w ?? this.#makeWorker();
    this.#topUp();
    this.#preattachOne();
    return got;
  }

  /// Start Workers up to the pool size, one per call so a burst of forks does
  /// not spend its first moments making Workers it will not reach. Called on
  /// a take and again when a pooled Worker is ready, so the pool fills one
  /// Worker after another: called on a take alone, a take removed one and
  /// added one, and the pool never held more than one Worker. The second
  /// and third processes of a pipeline (`echo a | cat | cat`) then each
  /// waited about 20 ms for a Worker made at their fork (demo page).
  #topUp() {
    if (this.#warm.length >= this.#warmWanted) return;
    try {
      const w = this.#makeWorker();
      this.#pool(w, false);
      this.#warmMade++;
    } catch {
      // Cannot make another Worker: the next `#takeWorker` makes one itself.
    }
  }

  /** Into the pool: a new Worker at the back, a given-back one at the front
   *  (its JS is compiled and warm, so the next process takes it first). */
  #pool(w, givenBack) {
    // Buffer whatever it says before `#spawn` attaches its listener: a
    // pooled Worker announces itself while still sitting in the array, and
    // that message would otherwise be delivered to nobody and dropped. A
    // given-back Worker's last process may still say something on its way
    // out: nothing before its next `ready` is kept.
    const early = [];
    let readied = !givenBack;
    // A given-back Worker is ready again once it has reset.
    w[READY] = false;
    const buf = (e) => {
      const m = e && e.data !== undefined ? e.data : e;
      if (m?.kind === 'ready') readied = true;
      if (!readied) return;
      early.push(m);
      // Listening now (`ready` is posted once its handler is on).
      if (m?.kind === 'ready') {
        w[READY] = true;
        if (!w[POOLED]) this.#poolModules(w);
        if (this.#warm.includes(w)) this.#preattachOne();
      }
      // Still pooled (not taken, the host not stopped): start the next.
      if (m?.kind === 'ready' && !givenBack && this.#warm.includes(w)) this.#topUp();
    };
    w.addEventListener?.('message', buf);
    w.on?.('message', buf);
    w[EARLY] = { early, buf };
    if (givenBack) this.#warm.unshift(w);
    else this.#warm.push(w);
  }

  /**
   * A Worker whose process exited cleanly, back into the pool for the next
   * process rather than ended: a new Worker compiles and warms the runtime's
   * JS on every process's way in and out. The Worker resets its realm
   * (worker.mjs `reset`) and makes a new runtime on a new memory, as a new
   * one does. False when it is not wanted (the pool is full, the Worker has
   * run `REUSE_MAX` processes), and then it is ended as before.
   */
  #giveBack(w) {
    const n = (w[REUSES] ?? 0) + 1;
    const l = w[LISTENING];
    if (!l || n > REUSE_MAX || this.#warm.length >= this.#warmWanted + GIVEN_BACK_MAX) return false;
    delete w[LISTENING];
    w.removeEventListener?.('message', l.onEvent);
    w.off?.('message', l.onMsg);
    w.removeEventListener?.('error', l.fail);
    w.off?.('error', l.fail);
    w[REUSES] = n;
    this.#reused++;
    this.#pool(w, true);
    w.postMessage({ kind: 'reset' });
    return true;
  }
  #reused = 0;

  /// How the pool is doing, for the per-run line.
  get warmStats() {
    return { made: this.#warmMade, used: this.#warmUsed, idle: this.#warm.length, reused: this.#reused };
  }

  /// Send a Worker the closure metadata only -- which library owns which
  /// pages. Sending all compiled modules up front cost 549ms per fork; each
  /// module is fetched later, when a branch lands in it (`#sendLibrary`).
  #streamClosure(w, closure) {
    const metas = (closure ?? []).map((e) => (e instanceof Uint8Array ? e : e.meta));
    // Which program each library images, where it is one: every closure root
    // claims `PIE_BASE`, so this tells `ls`'s pages from `bash`'s.
    const programs = (closure ?? []).map((e) => (e instanceof Uint8Array ? '' : (e.program ?? '')));
    try {
      w.postMessage({ kind: 'closureMeta', metas, programs });
    } catch (e) {
      this.#onOutput(2, `gex: the closure metadata did not reach a worker: ${e.message}\n`, 0);
    }
  }

  /// Hand one compiled closure library to the Worker that asked, fetching and
  /// compiling it now on first ask. A closure entry may arrive with only
  /// metadata and a `url`; the compile is kept and reused by reference.
  async #sendLibrary(w, i) {
    const entry = (this.#extra?.closure ?? [])[i];
    if (!entry) return;
    try {
      if (!(entry instanceof Uint8Array) && !entry.mod && entry.url) {
        entry.fetching ??= (async () => {
          const t0 = performance.now();
          const res = await fetch(entry.url);
          if (!res.ok) throw new Error(`${entry.url}: ${res.status} ${res.statusText}`);
          const bytes = new Uint8Array(await res.arrayBuffer());
          const mod = await WebAssembly.compile(bytes);
          narrate(
            `[gex] closure library ${i}: ${(bytes.length / 1048576).toFixed(1)} MiB `
            + `fetched and compiled in ${Math.round(performance.now() - t0)}ms`,
          );
          return mod;
        })();
        entry.mod = await entry.fetching;
      }
      w.postMessage({
        kind: 'closureModule',
        i,
        mod: entry instanceof Uint8Array ? entry : entry.mod,
      });
    } catch (e) {
      // Loud, not fatal: a missing library costs the guest the translation it
      // would have saved, nothing else.
      console.error(`[gex] closure library ${i} did not reach a worker:`, e);
      this.#onOutput(2, `gex: closure library ${i} did not reach a worker: ${e.message}\n`, 0);
    }
  }

  #nextKernelStack(key) {
    if (!this.#stacks) return undefined;
    const reuse = this.#stacksFree.pop();
    if (reuse !== undefined) {
      this.#stackOf.set(key, reuse);
      return this.#stacks.area + reuse * this.#stacks.size;
    }
    // Slice 0 belongs to nobody: the first Worker uses the linked-in stack.
    const n = ++this.#stacksTaken;
    this.#stackOf.set(key, n);
    // The top slices are the page helpers' that take a stack of their own,
    // one each (lazy-fetch.mjs `HELPER_STACKS`: tty.mjs, hostx.js,
    // persist-worker.js, fs-opened-worker.js). Slice n's stack is the one
    // below the top `area + n * size`, so the highest a Worker may have is
    // the one under the lowest helper's.
    if (n > this.#stacks.count - HELPER_STACK_COUNT) {
      throw new Error(
        `the global kernel has ${this.#stacks.count} stacks and this is Worker ${n + 1}. ` +
          'Raise STACK_COUNT in shims/kernel.',
      );
    }
    return this.#stacks.area + n * this.#stacks.size;
  }

  /**
   * A kernel stack slice for something that is not a guest Worker but calls
   * into the global kernel from a realm of its own (the demo's overlay
   * Worker, `web/src/overlay-worker.js`). Undefined until the first Worker has said
   * where the stacks are. Give it back with `releaseKernelStack(key)`.
   */
  takeKernelStack(key) {
    return this.#nextKernelStack(`ext:${key}`);
  }

  releaseKernelStack(key) {
    const slice = this.#stackOf.get(`ext:${key}`);
    if (slice === undefined) return;
    this.#stackOf.delete(`ext:${key}`);
    this.#stacksFree.push(slice);
  }

  /** Whether the Worker `key` is inside a wineserver door call: it holds
   *  the door lock (docs/wineserver-kernel.md §4.2). */
  #inDoor(key) {
    const d = this.#door;
    if (!d) return false;
    const i32 = new Int32Array(d.memory.buffer);
    const a = d.area >> 2;
    const parts = String(key).split(':');
    return Atomics.load(i32, a) !== 0 && i32[a + 30] === Number(parts[0]) && i32[a + 29] === Number(parts[parts.length - 1]);
  }

  /** A Worker has ended: stop it, and put its kernel stack back. `reuse`:
   *  its process exited cleanly, and it may go back to the pool instead. */
  #release(key, w, reuse = false) {
    // Not inside a door call: a Worker stopped there would leave the door
    // lock held and the server's pump waiting for it forever (a process the
    // server kills from inside its own request, as `terminate_process`
    // does). It finishes the call first; the guest it then goes back to
    // runs a moment longer, as a signal's target does.
    if (this.#inDoor(key)) {
      const t0 = Date.now();
      if (globalThis.GEX_DOOR_STATS || globalThis.process?.env?.GEX_DOOR_STATS) console.error(`[host] ${key}: ended inside a wineserver door call; stopped when it leaves`);
      const again = () => {
        if (this.#inDoor(key) && Date.now() - t0 < 10000) { setTimeout(again, 1); return; }
        if (this.#inDoor(key)) {
          // Stopped inside a door call: its handler is cut off midway, so
          // the server is poisoned (area +124) and the lock given up; the
          // pump takes it, sees the poison and ends the server, as when
          // wineserver dies.
          const i32 = new Int32Array(this.#door.memory.buffer);
          const a = this.#door.area >> 2;
          console.error(`[host] ${key}: still inside a wineserver door call after 10 s; stopped, and the server is poisoned`);
          Atomics.store(i32, a + 31, i32[a + 29] || -1);
          Atomics.store(i32, a, 0);
          Atomics.notify(i32, a);
        }
        this.#releaseNow(key, w);
      };
      setTimeout(again, 1);
      return;
    }
    this.#releaseNow(key, w, reuse);
  }

  #releaseNow(key, w, reuse = false) {
    this.#workers.delete(key);
    // The last Worker of a process gone: drop its shared-module cache so a long
    // page does not accumulate every process's compiled modules forever.
    const proc = String(key).split(':')[0];
    if (![...this.#workers.keys()].some((k) => String(k).split(':')[0] === proc)) {
      this.#moduleCache.delete(proc);
    }
    const slice = this.#stackOf.get(key);
    if (slice !== undefined) {
      this.#stackOf.delete(key);
      this.#stacksFree.push(slice);
    }
    // Not the other flavour's: the pool is the page flavour's (`#other`).
    if (reuse && !this.#foreign(key) && this.#giveBack(w)) return;
    w.terminate();
  }

  /** Keep an exited process's zeroed memory for a fork child, dropping the
   *  oldest past RECYCLE_COUNT or RECYCLE_BYTES. */
  #keepMemory(mem) {
    this.#recycled.push(mem);
    let bytes = this.#recycled.reduce((n, x) => n + x.buffer.byteLength, 0);
    while (this.#recycled.length > RECYCLE_COUNT || (this.#recycled.length > 1 && bytes > RECYCLE_BYTES)) {
      bytes -= this.#recycled.shift().buffer.byteLength;
    }
    if (bytes > RECYCLE_BYTES) this.#recycled.length = 0;
  }

  /** A kept memory for a child whose image ends at `need` bytes, taken: the
   *  smallest that already covers it, up to twice its size (the slack is
   *  zeroed, already-touched pages the shim's arena grows into without a
   *  `memory.grow`); otherwise the largest smaller one, which `growTo`
   *  extends. A memory never shrinks, so one more than twice the child's is
   *  left for a bigger child. */
  #takeMemory(need) {
    let best = -1;
    let bestN = 0;
    for (let i = 0; i < this.#recycled.length; i++) {
      const n = this.#recycled[i].buffer.byteLength;
      const fits = n >= need && n <= 2 * need;
      const better = best < 0
        || (fits && (bestN < need || n < bestN))
        || (!fits && n < need && bestN < need && n > bestN);
      if ((fits || n < need) && better) {
        best = i;
        bestN = n;
      }
    }
    return best < 0 ? undefined : this.#recycled.splice(best, 1)[0];
  }

  #onMessage(pid, w, m, resolve, reject) {
    switch (m?.kind) {
      case 'ready': {
        // How long the Worker took to exist and load its bundle.
        if (this.#extra?.debugExit && this.#asked.has(pid)) {
          const t = performance.now() - this.#asked.get(pid);
          this.#onOutput(2, `[gex] pid ${pid}: worker ready after ${Math.round(t)}ms\n`, pid);
        }
        const msg = this.#pending.get(pid);
        this.#pending.delete(pid);
        narrate(`[host] pid ${pid}: ready, ${msg ? `sending ${msg.kind}` : 'NOTHING HELD'}`);
        if (msg) {
          // Awaited: compiling the shim cannot be synchronous in a browser.
          this.#runtimeFor(pid).then((rt) => {
          // The closure goes after, one library per message: all at once does
          // not cross (Chromium `DataCloneError, out of memory`). The guest
          // runs while they arrive -- the miss path forgives a module not yet
          // landed and asks again when it shows up.
          const { closure, ...extra } = this.#extra;
          w.postMessage({
            ...extra,
            // How many to wait for. The Worker installs them before it runs
            // the guest, so it has to know when it has them all.
            closureCount: (closure ?? []).length,
            ...msg,
            outputAck: this.#outputAck(w),
            kernelStack: this.#stackFor(w, pid),
            // Compiled once here, not from bytes in every Worker.
            shim: rt.shim,
            guest: rt.guest,
            kernel: rt.kernel,
            mem64: rt.mem64,
            pageFlavours: this.#pageFlavours(),
            // A thread joins its process: every module the process's Workers
            // have compiled so far, so it does not fetch and compile them all
            // again (each thread of a game spent ~3 s doing that, one after the
            // other, at start-up). The `start` closure above meant to send
            // these, but this is the path that runs. A fork has its own memory
            // and registry, so only threads (pid `proc:tid`).
            sharedModules: this.#sharedFor(pid, msg.kind === 'thread', w),
            // the wineserver's door, if one is up (`doorPublish`)
            door: this.#door ?? undefined,
            // A fork child's memory: an exited process's, zeroed, if one fits.
            recycledMemory: msg.kind === 'adopt' && msg.child && !msg.child.exec && !this.#foreign(pid)
              ? this.#takeMemory(imageExtent(msg.child))
              : undefined,
          // `transfer`: buffers the caller gives up (the page's bundle), moved
          // rather than copied, and freed by the Worker once loaded.
          }, [...forkTransfer(msg.child), ...(msg.transfer ?? [])]);
          // Timed separately: the fork-to-adopt gap is 549ms and unaccounted --
          // this `postMessage` (the 15.4 MiB image copy) or the closure below.
          const tSent = performance.now();
          if (TEST_FORK_RECORDS) this.#checkForkRecord(pid);
          this.#streamClosure(w, closure);
          if (this.#extra?.debugExit) {
            this.#onOutput(
              2,
              `[gex] pid ${pid}: sent ${(closure ?? []).length} closure modules in `
                + `${Math.round(performance.now() - tSent)}ms\n`,
              pid,
            );
          }
          },
          );
        }
        break;
      }
      case 'doorPublish': {
        // The wineserver's door (docs/wineserver-kernel.md): every Worker of
        // the session, now and later, gets the server's memory and module.
        this.#door = m.door;
        narrate(`[host] wineserver pid ${m.door?.pid} published its door`);
        for (const [, other] of this.#workers) other.postMessage({ kind: 'door', door: m.door });
        break;
      }
      case 'wantLibrary':
        this.#sendLibrary(w, m.i);
        break;
      case 'kernelStacks':
        this.#stacks = m;
        // A pooled Worker that was ready before the stacks were known.
        this.#preattachOne();
        break;
      case 'output': {
        this.#onOutput(m.fd, m.text, pid, m);
        // Delivered: the Worker may now let its exit be seen.
        const a = this.#outputAcks.get(w);
        if (a) {
          Atomics.add(a, 0, 1);
          Atomics.notify(a, 0);
        }
        break;
      }
      case 'droppedModules': {
        // A Worker let go of modules its process can no longer reach
        // (`Gex#reapDead`): the relay forgets them, so their code can go.
        // Its siblings and the Workers in the pool hold them too (relayed
        // and pooled), and so does the page's unit cache: all of them let go,
        // or the code stays. A unit dropped here is compiled again if the
        // same bytes come back (a DLL loaded again).
        const proc = String(pid).split(':')[0];
        const keys = m.keys ?? [];
        const cache = this.#moduleCache.get(proc);
        const units = [];
        for (const k of keys) {
          if (cache?.has(k)) {
            cache.delete(k);
            cache.bytes = (cache.bytes ?? 0) - regKeyBytes(k);
          }
          if (this.#unitModules.delete(k)) {
            this.#unitBytes -= Number(k.split(':')[1]);
            units.push(k);
          }
        }
        for (const [key, other] of this.#workers) {
          if (other !== w && String(key).split(':')[0] === proc) other.postMessage({ kind: 'droppedModules', keys });
        }
        if (units.length) {
          for (const p of this.#warm) {
            if (!p[POOLED]) continue;
            for (const k of units) p[POOLED].delete(k);
            p.postMessage({ kind: 'droppedModules', keys: units });
          }
        }
        break;
      }
      case 'compiledModule': {
        // A Worker compiled a module with content key `m.key`. Remember it for this process
        // and relay it to the process's other live Workers so none recompiles
        // the same bytes. `WebAssembly.Module` crosses `postMessage` by
        // reference, so this is cheap. See `#moduleCache`.
        const proc = String(pid).split(':')[0];
        let cache = this.#moduleCache.get(proc);
        if (!cache) this.#moduleCache.set(proc, (cache = new Map()));
        if (this.#extra?.fillTime) this.#relayCensus();
        if (typeof m.key === 'string' && m.key.startsWith('wasm:')) {
          this.#pageModules.delete(m.key);
          this.#pageModules.set(m.key, m.mod);
          this.#poolModule(m.key, m.mod);
          let bytes = 0;
          for (const k of this.#pageModules.keys()) bytes += keyBytes(k);
          while (this.#pageModules.size > 1 && (this.#pageModules.size > PAGE_MODULES || bytes > PAGE_MODULE_BYTES)) {
            const oldest = this.#pageModules.keys().next().value;
            bytes -= keyBytes(oldest);
            this.#pageModules.delete(oldest);
          }
        }
        // A translation unit for the page's cache (`Gex#unitKey`): kept
        // for processes to come, not relayed -- the process's own Workers
        // share it by its content key.
        if (typeof m.key === 'string' && m.key.startsWith('u:')) {
          if (this.#extra?.unitCache && !this.#unitModules.has(m.key)) {
            this.#unitModules.set(m.key, m.mod);
            this.#poolModule(m.key, m.mod);
            this.#unitBytes += Number(m.key.split(':')[1]);
            while ((this.#unitBytes > UNIT_MODULE_BYTES || this.#unitModules.size > this.#relayModules) && this.#unitModules.size > 1) {
              const oldest = this.#unitModules.keys().next().value;
              this.#unitBytes -= Number(oldest.split(':')[1]);
              this.#unitModules.delete(oldest);
            }
          }
          break;
        }
        if (typeof m.key === 'string' && !cache.has(m.key)) {
          cache.set(m.key, m.mod);
          cache.bytes = (cache.bytes ?? 0) + regKeyBytes(m.key);
          while ((cache.bytes > PROCESS_MODULE_BYTES || cache.size > this.#relayModules) && cache.size > 1) {
            const oldest = cache.keys().next().value;
            cache.bytes -= regKeyBytes(oldest);
            cache.delete(oldest);
            if (!cache.capped) {
              cache.capped = true;
              console.error(`[host] process ${proc}'s compiled-module relay passed ${PROCESS_MODULE_BYTES >> 20} MiB: dropping the oldest (a pc translated over and over? see the "LIVELOCK?" line)`);
            }
          }
          if (this.#extra?.shareTrace) console.warn(`[share] host cached ${m.key.slice(0, 30)} from ${pid} (${cache.size} for process ${proc})`);
          for (const [key, other] of this.#workers) {
            if (other !== w && String(key).split(':')[0] === proc) {
              other.postMessage({ kind: 'sharedModule', key: m.key, mod: m.mod });
            }
          }
        }
        break;
      }
      case 'fork': {
        const c = m.child?.forkCost;
        if (c) {
          const hop1 = Math.round(performance.timeOrigin + performance.now() - c.sentAt);
          narrate(
            `[gex] fork -> pid ${m.child.pid}: image ${(c.bytes / 1048576).toFixed(1)} MiB`
              + ` in ${c.regions} regions, built ${c.buildMs}ms,`
              + ` ${hop1}ms to reach the host`,
          );
          c.atHost = performance.now();
        }
        this.#forked++;
        {
          // The exec'd program's memory, or the parent's.
          const f = m.child.mem64 ?? this.#mem64(pid);
          if (f !== !!this.#extra?.mem64) this.#flavourOf.set(m.child.pid, f);
          else this.#flavourOf.delete(m.child.pid);
        }
        this.#onFork(m.child, pid);
        if (TEST_FORK_RECORDS) this.#forkRecords.set(m.child.pid, new WeakRef(m.child));
        // Started and let run -- nobody waits for a child. The handler below
        // outlives this call by the child's whole life, so it holds the pid
        // alone, and the message is let go (see `#spawn`).
        const child = m.child.pid;
        this.#spawn(child, { kind: 'adopt', child: m.child }).catch((e) => {
          this.#onOutput(2, `gex: process ${child} failed: ${e.message}\n`, pid);
          // As an event too: this is caught, so no `error` fires and a harness
          // may miss the line above. See task 146.
          crashed(`pid ${child}: ${e.message}`);
        });
        m = null;
        break;
      }
      case 'execAway': {
        // The process's exec of a program of the other memory flavour (a
        // wasm32 one in a 64-bit memory's page: shim lib.rs `exec_away`):
        // its Worker has ended without the process, which goes on in a new
        // one with that flavour's runtime, as a posix_spawn child's exec
        // does, and ends there.
        const child = m.child;
        if (child.mem64 !== undefined && child.mem64 !== !!this.#extra?.mem64) this.#flavourOf.set(Number(pid), child.mem64);
        else this.#flavourOf.delete(Number(pid));
        this.#releaseNow(pid, w);
        this.#spawn(pid, { kind: 'adopt', child }).then(resolve, reject);
        break;
      }
      case 'thread': {
        // A thread of a running process: same memory, same kernel, its own
        // Worker. Nothing waits for it.
        this.#threads++;
        const key = `${pid}:${m.thread.tid}`;
        // A threaded process's memory, for a harness (`MEMORIES`).
        if (MEMORIES && m.thread.memory) (globalThis.__gexMemories ??= new Map()).set(Number(String(pid).split(':')[0]), m.thread.memory);
        if (globalThis.GEX_ATTACH_TRACE) console.error(`[spawn] host got 'thread' for ${key}, taking a Worker (t=${Date.now()})`);
        // From the warm pool, as a process's: a fresh Worker was ~100 ms of
        // script load and shim instantiation before the thread could start,
        // and a starting Wine process makes its threads one after another
        // (61 of them before Paint's window, 13.5 s in all).
        const w2 = this.#foreign(pid) ? this.#makeWorker() : this.#takeWorker();
        this.#workers.set(key, w2);
        workerRan(w2, key);
        // Keyed by the thread, so its end stops the right Worker and frees the
        // right kernel stack.
        const deliver = (mm) => this.#onMessage(key, w2, mm, () => {}, () => {});
        w2.addEventListener?.('message', (e) => deliver(e.data));
        w2.on?.('message', deliver);
        // A thread that throws, said out loud: a thread Worker had no `error`
        // listener, so an exception was swallowed and read as a hang.
        const threadFailed = (e) => {
          this.#onOutput(2, `gex: thread ${key} failed: ${e?.message ?? String(e)}\n`, pid);
          crashed(`thread ${key}: ${e?.message ?? String(e)}`);
        };
        w2.addEventListener?.('error', threadFailed);
        w2.on?.('error', threadFailed);
        this.#pending.set(key, { kind: 'thread', ...m.thread });
        // A pooled Worker's 'ready' came while it sat in the pool: deliver it
        // now that `#pending` names this thread.
        const held = w2[EARLY];
        if (held) {
          delete w2[EARLY];
          w2.removeEventListener?.('message', held.buf);
          w2.off?.('message', held.buf);
          for (const mm of held.early) deliver(mm);
        }
        // A thread's start message is keyed by tid and handed over here, once.
        const start = (mm) => {
          if (mm?.kind !== 'ready') return;
          if (globalThis.GEX_ATTACH_TRACE) console.error(`[spawn] host got 'ready' from ${key}, sending start (t=${Date.now()})`);
          this.#pending.delete(key);
          // Without the closure, as the fork path: `closure` entries are mostly
          // promises (a library is fetched on first branch into it), and a
          // promise is not structured-cloneable. A count instead; the Worker
          // asks for what it needs.
          const { closure: _closure, ...extra } = this.#extra ?? {};
          this.#runtimeFor(pid).then((rt) =>
          w2.postMessage({
            ...extra,
            closureCount: (this.#extra?.closure ?? []).length,
            kind: 'thread',
            outputAck: this.#outputAck(w2),
            tid: m.thread.tid,
            // `ctx` and `pc` are forwarded explicitly: `adoptThread` re-derives
            // `pc`, but a dropped `ctx` became NaN -> 0, so every guest thread
            // read its registers from address 0 and died at its first
            // instruction.
            ctx: m.thread.ctx,
            pc: m.thread.pc,
            memory: m.thread.memory,
            moduleStore: m.thread.moduleStore,
            procOpts: m.thread.procOpts,
            kernelStack: this.#stackFor(w2, key),
            shim: rt.shim,
            guest: rt.guest,
            kernel: rt.kernel,
            mem64: rt.mem64,
            pageFlavours: this.#pageFlavours(),
            // Every module the process's Workers have compiled so far, so this
            // thread reuses them instead of recompiling from bytes in its own
            // `#catchUp`. `WebAssembly.Module`s cross by reference, so this is
            // references, not a byte copy. See `#moduleCache`.
            sharedModules: this.#sharedFor(pid, true),
            door: this.#door ?? undefined,
          }),
          );
        };
        // NOT registered as a second listener: `start` is already reached
        // through `deliver` (which finds this thread in `#pending`). Registering
        // it too sent the start message twice and ran `gex.run` twice on one
        // thread. One record, one path, one start.
        void start;
        break;
      }
      case 'stdin': {
        // Only pid 1 has a terminal; a child reads the inherited descriptor,
        // which is the kernel's business, not this file's.
        if (!this.#readStdin) {
          w.postMessage({ kind: 'eof' });
          break;
        }
        Promise.resolve(this.#readStdin(m.lflag)).then((bytes) => {
          // `true` means "input is already in the shared queue you read from".
          if (bytes === true) w.postMessage({ kind: 'stdin', bytes: true });
          else if (bytes && bytes.length) w.postMessage({ kind: 'stdin', bytes });
          else w.postMessage({ kind: 'eof' });
        });
        break;
      }
      case 'exit':
        if (m.threadGone) {
          // Not the process's end: it goes on in another Worker.
          const c = this.#carried.get(pid);
          this.#release(pid, w);
          if (c && 'status' in c) {
            this.#carried.delete(pid);
            resolve(c.status);
          } else {
            this.#carried.set(pid, { resolve });
          }
          break;
        }
        if (m.carries) {
          // A thread Worker that took its process over by `execve`: its exit
          // is the process's, owed to whoever waits on the first Worker.
          const proc = Number(String(pid).split(':')[0]);
          const c = this.#carried.get(proc);
          this.#onExit(m, proc);
          forgetMemory(proc);
          this.#release(pid, w);
          if (c?.resolve) {
            this.#carried.delete(proc);
            c.resolve(m.status);
          } else {
            this.#carried.set(proc, { status: m.status });
          }
          break;
        }
        // Kept for the page flavour's fork children only.
        if (m.recycle instanceof WebAssembly.Memory && !this.#foreign(pid)) this.#keepMemory(m.recycle);
        if (this.#extra?.exitTrace) {
          this.#onOutput(
            2,
            `[gex] worker ${pid} ended with status ${m.status}` +
              `${m.exitPc ? ` at pc 0x${m.exitPc.toString(16)}` : ''}` +
              ` (guest ${m.guestMillis ?? -1}ms)\n`,
            typeof pid === 'number' ? pid : 0,
          );
        }
        // The page passes no `onExit`, so these per-process numbers were
        // computed and thrown away. Printed here, guarded so the Node harness
        // (which prints its own) does not double up.
        if (typeof window !== 'undefined' && m.millis !== undefined && logOn('debug')) {
          // How long the process existed, against what it can account for.
          const lived = this.#asked.has(pid)
            ? Math.round(performance.now() - this.#asked.get(pid))
            : 0;
          this.#asked.delete(pid);
          console.log(
            // `guest` is the time inside `gex.run`; `lived` minus `guest` is
            // Worker creation, adoption and the wait for a start message. The
            // two must be separable -- a cost has been blamed on the wrong side.
            `[gex] pid ${pid}: lived ${lived}ms (guest ${m.guestMillis ?? -1}ms)` +
              (m.cmdline ? ` {${m.cmdline}}` : '') +
              // What the runtime still held on exit. -1 without GEX_HEAP_COUNTERS=1.
              (m.heapKib >= 0 ? `, runtime holds ${Math.round(m.heapKib / 1024)} MiB` : '') +
              // Dispatch slots the page has left. A budget nothing refills, so
              // this only falls, and how fast it falls is what says how many
              // more processes the run can translate code in.
              (m.slotsLeftK >= 0 ? `, ${m.slotsLeftK}k dispatch slots left` : '') +
              // What this process's memory grew to. One memory per process, so
              // this times the number live at once is what the tab costs.
              (m.memoryMib !== undefined ? `, memory ${m.memoryMib} MiB` : '') +
              // And what that memory went on, which is the only way to tell a
              // guest that really maps a lot from a runtime spending it.
              (m.arena ? ` (maps ${m.arena[0]}, heap ${m.arena[1]}, stacks ${m.arena[2]})` : '') +
              // Dense modules this Worker left for a miss, and those a miss took.
              (m.fill?.denseLeft ? `, dense modules ${m.fill.denseWanted} wanted of ${m.fill.denseLeft} left (${m.fill.denseAsks} asks, ${m.fill.denseFillMs.toFixed(1)} ms, worst ${m.fill.denseFillWorst.toFixed(2)} ms)` : '') +
              (m.fill ? `, tables blocks ${m.fill.blocksLen} ftab ${m.fill.ftabLen}` : '') +
              // The one memory every process shares: the filesystem and the
              // store of translated modules live in it, and it only grows.
              (this.#kernel?.memory
                ? `, kernel ${Math.round(this.#kernel.memory.buffer.byteLength / 1048576)} MiB`
                : '') +
              // What the fork wrote into this process. There is no
              // copy-on-write, so this is paid in full, per fork.
              // A posix_spawn child that ran in its parent's Worker, in its
              // memory, until its exec: no copy, and this long to the exec.
              (m.vforkMs !== undefined ? `, forked in place (${m.vforkMs.toFixed(1)}ms to exec, nothing copied)` : '') +
              // A Worker given back after an earlier process (`#giveBack`).
              (m.workerRuns > 1 ? `, on a given-back worker (its process ${m.workerRuns})` : '') +
              (m.forkCopied
                ? `, fork copied ${Math.round(m.forkCopied / 1048576)} MiB`
                  + (m.forkZeros ? ` (${Math.round(m.forkZeros / 1048576)} MiB of it zero, left alone)` : '')
                  + (m.forkSkipped ? ` and skipped ${Math.round(m.forkSkipped / 1048576)}` : '')
                : '') +
              `, ${m.millis}ms translating ${m.blocks} blocks` +
              // Of those, ones on a page the closure already had: if most of
              // `blocks`, the fill is failing to reach the guest, not a
              // recompiler coverage gap.
              (m.liftInClosure ? ` (${m.liftInClosure} on closure pages)` : '') +
              // Why those misses translated rather than being filled.
              (m.liftWhy && m.liftWhy.some((n) => n)
                ? ` why[nofill=${m.liftWhy[0]} unowned=${m.liftWhy[1]} asked=${m.liftWhy[2]} fresh=${m.liftWhy[3]}]`
                : '') +
              (m.liftSlots ? ` slots[${m.liftSlots}]` : '') +
              (m.waits && (m.waits.park || m.waits.turn || m.waits.nap || m.waits.net)
                ? `, waited park=${m.waits.park} turn=${m.waits.turn} nap=${m.waits.nap} net=${m.waits.net ?? 0}`
                : '') +
              (m.byObject ? ` [${m.byObject}]` : '') +
              (m.xlate ? ` xlate[insns=${m.xlate[0]} wasmKiB=${Math.round(m.xlate[1] / 1024)} units=${m.xlate[2]} smc=${m.xlate[3]} retiredPages=${m.xlate[4]} retires=${m.xlate[5]} relift=${m.xlate[6]}blk/${Math.round((m.xlate[8] ?? 0) / 1024)}KiB anon=${m.xlate[7]}blk/${Math.round((m.xlate[9] ?? 0) / 1024)}KiB]` : '') +
              (m.installs ? `, ${m.installMs}ms installing ${m.installs} modules` +
            ` (${m.compileMs}ms compile${m.compiles !== undefined ? ` of ${m.compiles}` : ''}${m.unitHits ? `, ${m.unitHits} from the unit cache` : ''}, ${m.instantiateMs}ms instantiate, ${m.growMs}ms grow)` : '') +
          `, worker start ${(m.shimCompileMs ?? 0) + (m.shimInstMs ?? 0) + (m.guestInstMs ?? 0)}ms` + ` (shim ${m.shimCompileMs}c/${m.shimInstMs}i, guest ${m.guestInstMs}i)` +
              (m.loader && m.loader.calls
                ? `, ld.so ${m.loader.calls} syscalls, last at ${m.loader.doneMs}ms`
                : '') +
              (m.loader && m.loader.link && m.loader.link !== 'off'
                ? `, link ${m.loader.link}` +
                  (m.loader.cost && m.loader.cost[0]
                    ? ` (${m.loader.cost[0]} regions, ` +
                      `${Math.round(m.loader.cost[1] / 1024)} KiB reread in ${m.loader.cost[3]}ms, ` +
                      `${Math.round(m.loader.cost[2] / 1024)} KiB copied in ${m.loader.cost[4]}ms)`
                    : '')
                : '') +
              (m.fill && (m.fill.filled || m.fill.unanswered)
                ? `, fill ${m.fill.filled} answered ${m.fill.unanswered} not`
                  + (m.fill.sysN ? `, sys ${m.fill.sysN}x ${m.fill.sysMs}ms` : '')
                  + ` (${m.fill.now ?? 0} fast, ${m.fill.nowMs ?? 0}ms[base ${m.fill.faBase ?? 0} init ${m.fill.faInit ?? 0}];`
                  + ` ${m.fill.slow ?? 0} slow, ${m.fill.slowMs ?? 0}ms;`
                  + ` ${m.fill.pub ?? 0} published, ${m.fill.pubMs ?? 0}ms,`
                  + ` grew ${m.fill.grew ?? 0} slots in ${m.fill.growMs ?? 0}ms,`
                  + ` want ${m.fill.wantMs ?? 0}ms inst ${m.fill.instMs ?? 0}ms`
                  + ` (grow ${m.fill.iGrow ?? 0} comp ${m.fill.iComp ?? 0}`
                  + ` inst ${m.fill.iInst ?? 0};`
                  + ` ${m.fill.iGrows ?? 0} grows +${m.fill.iAdded ?? 0}`
                  + ` copying ${m.fill.iCopied ?? 0}, tables ${m.fill.tlen ?? ''})`
                  + (m.fill.bigGrows ? `, big [${m.fill.bigGrows}]` : '')
                  + (m.fill.tn ? `, tn [${m.fill.tn}]` : '')
                  + ` tail ${m.fill.tailMs ?? 0}ms`
                  + (m.fill.strictGrows ? ` retry ${m.fill.strictGrows}x/${m.fill.strictSlots}` : '')
                  + `)`
                : '') +
              (m.reuse
                ? `, reuse have=${m.reuse[0]} hit=${m.reuse[1]} miss=${m.reuse[2]}` +
                  ` noident=${m.reuse[3]} served=${m.reuse[4]} stored=${m.reuse[5]} writable=${m.reuse[9]} nocode=${m.reuse[10]}`
                : ''),
          );
        }
        // The guest's own hot spots, when `?hot=1` asked for them.
        for (const line of m.hot ?? []) {
          this.#onOutput(2, `[hot] pid ${pid}: ${line}\n`, pid);
        }
        // Before the release: releasing terminates the Worker.
        this.#onExit(m, pid);
        forgetMemory(pid);
        this.#release(pid, w, !!m.giveBack);
        resolve(m.status);
        break;
      case 'threw':
        if (!String(pid).includes(':')) forgetMemory(pid);
        this.#release(pid, w);
        // Here as well as in the Worker: only this side knows which process
        // fell over, and a child's promise often has nobody awaiting it.
        console.error(`[gex] pid ${pid} threw: ${m.message}\n${m.stack ?? ''}`);
        // As an event: the console line is INFO to a watcher and easy to miss.
        // See task 146.
        crashed(`pid ${pid} threw: ${m.message}`);
        reject(
          Object.assign(new Error(`pid ${pid}: ${m.message}`), { guestStack: m.stack }),
        );
        break;
      default:
        this.#onOther(m, pid, w);
        break;
    }
  }
}
