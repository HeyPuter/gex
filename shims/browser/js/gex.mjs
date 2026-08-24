// The browser runtime: instantiate the shim (gex-kernel compiled to wasm) and
// a guest module against one memory, and run. The shim imports the guest's
// memory, so a syscall reads guest buffers in place. `run-node.mjs` is the
// Node entry.

// How far a process's memory may grow. One memory per process. See
// `gex_abi::GUEST_MEM_INITIAL_BYTES`.
const GUEST_BYTES = 0xffff0000;

/**
 * Decode UTF-8 out of guest memory.
 *
 * `TextDecoder.decode` refuses a `SharedArrayBuffer`-backed view, so a shared
 * memory needs the `slice` copy; it is only paid when the memory is shared.
 */
const DECODER = new TextDecoder();
export function utf8(view) {
  return DECODER.decode(view.buffer instanceof SharedArrayBuffer ? view.slice() : view);
}
const GUEST_INITIAL_BYTES = 0x05000000;
// The shim's stack sits at the bottom of its region, below its data.
const SHIM_STACK_TOP = 0x04800000;
/**
 * Whether a fork hands the child its parent's compiled modules (default 1).
 * `?forkmods=0` sends none and the child recompiles what it branches into.
 *
 * A function, not a module constant: a constant evaluates at import, before
 * the page sets the global.
 */
function forkModules() {
  const v = globalThis.GEX_FORK_MODULES;
  return v === undefined ? 1 : Number(v);
}

function adoptAfterTraps() {
  // Traps a forked child takes before adopting its parent's modules in bulk.
  // Default 1: deferred -- it picks up what it branches into rather than
  // installing all inherited modules up front (which almost every child throws
  // away on an immediate execve).
  return Number(globalThis.GEX_ADOPT_AFTER ?? 1);
}

// The module export that writes its passive element segments into the imported
// tables. See `Module::add_fill`.
const FILL = 'gexfill';
// The one a miss can ask for: fill whichever segment holds this slot.
const FILL_AT = 'gexfillat';
// Dispatch slots one 4 KiB guest page covers. `gex_abi::SLOTS_PER_PAGE`.
const SLOTS_PER_PAGE = 1024;
const FILL_ALL = 'gexfillall';

// Above this, a module's slots are written when a branch asks rather than at
// instantiation: small units are wanted immediately, closure libraries mostly
// never reached. Provisional.
const FILL_ON_DEMAND_BYTES = 4 << 20;

const DISPATCH_DIR = 0x00100000;
const LOW_END = DISPATCH_DIR + 0x80000 * 4;
const PAGES = GUEST_BYTES / 65536;
const INITIAL_PAGES = GUEST_INITIAL_BYTES / 65536;

/**
 * A cheap content hash for whether two copies of a region agree. FNV-1a over
 * words, not bytes: only ever answers same-or-different.
 */
function sum32(buf) {
  const u = new Uint32Array(buf.buffer ?? buf, buf.byteOffset ?? 0, (buf.byteLength / 4) | 0);
  let h = 0x811c9dc5;
  for (let i = 0; i < u.length; i++) h = Math.imul(h ^ u[i], 0x01000193);
  return h >>> 0;
}

/**
 * How far into the slot space a module's element segments reach, from its
 * `gex.meta` (`next_slot` at offset 24).
 *
 * `need` cannot be trusted: a closure library passes zero deliberately, so a
 * forked child adopting it would grow nothing and instantiation would write
 * past the end of the table.
 */
function slotsNeeded(meta) {
  if (!meta || meta.length < 32) return 0;
  const v = new DataView(meta.buffer, meta.byteOffset, meta.byteLength);
  return Number(v.getBigUint64(24, true));
}


export class Env {
  /** Called with the bytes of a guest write to fd 1 or 2. */
  write(fd, bytes) {
    const text = utf8(bytes);
    if (fd === 2) console.error(text);
    else console.log(text);
  }
  /** Nanoseconds since an arbitrary epoch. */
  now() { return BigInt(Math.round(performance.now() * 1e6)); }

  /**
   * Called when every guest thread is waiting for input. Resolve once some has
   * been delivered with `pushStdin`, or return false for end-of-file. The
   * default has no terminal, so it returns false and a read returns zero.
   */
  async readStdin() { return false; }

  /**
   * Start a guest process the guest itself forked.
   *
   * `child` is `{ pid, pc, ctx, sp, tls, image }`, where `image` is the
   * parent's mapped memory as `{ at, bytes }` records. A host makes a
   * `WebAssembly.Memory`, writes every record into it, instantiates the shim
   * and guest against it, calls `gex_adopt_child(pid, ctx, pc, sp, tls, brk)`
   * and enters at what it returns. No default: the answer differs by host.
   */
  async spawnProcess(child) {
    throw new Error(
      `this environment cannot start guest process ${child.pid}: no spawnProcess`,
    );
  }

  /**
   * Start a guest thread the guest itself cloned.
   *
   * `thread` is `{ tid, ctx, pc, memory }`. A host makes a Worker, constructs a
   * `Gex` on the *same* memory, and calls `adoptThread`. Nothing is copied --
   * unlike `spawnProcess`, which is why a thread needs a shared memory.
   *
   * Declared and deliberately not defined: a class field would shadow a
   * subclass's method of the same name. `Gex` asks whether the method exists.
   */
}

/**
 * What each dispatch table must hold, for this module, read from its
 * `dir_entries` -- each a `(page, base)` with the table in the base's top
 * eight bits. `next_slot` alone is one running total and cannot say which
 * table wants what. Returns a sparse array of lengths, indexed by table.
 */
export function metaTableNeeds(meta) {
  if (!meta || meta.byteLength < 48) return [];
  const dv = new DataView(meta.buffer, meta.byteOffset, meta.byteLength);
  const count = Number(dv.getBigUint64(40, true));
  const needs = [];
  let at = 48;
  for (let i = 0; i < count && at + 12 <= meta.byteLength; i++, at += 12) {
    const base = dv.getUint32(at + 8, true);
    const t = base >>> 24;
    const end = (base & 0x00ffffff) + SLOTS_PER_PAGE;
    if (!(needs[t] >= end)) needs[t] = end;
  }
  return needs;
}

/**
 * What each dispatch table must hold, read from the module's active element
 * segments -- the only authority, since `Metadata` cannot give per-table needs.
 * Returns a sparse array indexed by table; passive segments are skipped.
 */
export function elementTableNeeds(wasm) {
  if (!(wasm instanceof Uint8Array) || wasm.length < 8) return [];
  const dv = new DataView(wasm.buffer, wasm.byteOffset, wasm.byteLength);
  let i = 8;
  const leb = () => {
    let n = 0, shift = 0, b;
    do { b = dv.getUint8(i++); n |= (b & 0x7f) << shift; shift += 7; } while (b & 0x80);
    return n >>> 0;
  };
  const needs = [];
  try {
    while (i < wasm.length) {
      const id = dv.getUint8(i++);
      const size = leb();
      const end = i + size;
      if (id !== 9) { i = end; continue; }
      const count = leb();
      for (let seg = 0; seg < count; seg++) {
        const flags = leb();
        let table = 0;
        if (flags === 2 || flags === 6) table = leb();
        let off = -1;
        if ((flags & 3) === 0 || flags === 2 || flags === 6) {
          // The offset expression: only a constant is understood.
          if (dv.getUint8(i) === 0x41) { i++; off = leb(); }
          while (dv.getUint8(i) !== 0x0b) i++;
          i++;
        }
        if (flags === 1 || flags === 2 || flags === 3) leb();
        else if (flags >= 5) leb();
        const n = leb();
        for (let k = 0; k < n; k++) {
          if (flags & 4) { while (dv.getUint8(i) !== 0x0b) i++; i++; } else leb();
        }
        // Passive and declared segments write nothing now.
        const active = flags === 0 || flags === 2 || flags === 4 || flags === 6;
        if (active && off >= 0 && !(needs[table] >= off + n)) needs[table] = off + n;
      }
      i = end;
    }
  } catch {
    return needs;
  }
  return needs;
}

export function metadataSection(wasm) {
  const dv = new DataView(wasm.buffer, wasm.byteOffset, wasm.byteLength);
  let i = 8;
  const leb = () => {
    let n = 0, shift = 0, b;
    do { b = dv.getUint8(i++); n |= (b & 0x7f) << shift; shift += 7; } while (b & 0x80);
    return n >>> 0;
  };
  while (i < wasm.length) {
    const id = dv.getUint8(i++);
    const size = leb();
    const end = i + size;
    if (id === 0) {
      const nameLen = leb();
      const name = utf8(wasm.subarray(i, i + nameLen));
      // A copy, not a view: `subarray` shares the module's ArrayBuffer, and
      // postMessage would then clone every compiled module behind it.
      if (name === 'gex.meta') return wasm.slice(i + nameLen, end);
    }
    i = end;
  }
  return null;
}

// The dispatch tables a translated module imports and exports, in index order.
// More than one because V8 caps a table at ten million entries (38 MiB of guest
// text) and the graphics rootfs exceeds that. Table 0 keeps its bare name for
// modules built when there was only one. See #64.
const TABLE_NAMES = [
  'dispatch',
  'dispatch1',
  'dispatch2',
  'dispatch3',
  'dispatch4',
  'dispatch5',
  'dispatch6',
  'dispatch7',
];

export class Gex {
  #memory; #shim; #guest; #env; #u8; #imports; #net = null; #gl = null; #virgl = null; #virglTrace = false; #wl = null;
  // The channel `#turn` posts to itself on, made once and kept.
  #tick = null;
  // Translated modules this Worker has put into its own dispatch tables. Per
  // Worker, not per process: tables belong to an instance, so every thread
  // starts empty however much the process has translated.
  #installed = 0;
  // What compiling and instantiating shared modules costs. See
  // `#installModuleInner`.
  #installMs = 0;
  #faBaseMs = 0;
  #faInitMs = 0;
  #fillWidthTold = false;
  #imageMs = 0;
  #imageBytes = 0;
  #imageRegions = 0;
  #nowFills = 0;
  #nowMs = 0;
  #bigGrows = [];
  /// `gex_table_need(0)` at named moments, so the step that raises it is
  /// visible rather than guessed at.
  #tn = [];
  #tnAt(tag) {
    const e = this.#shim?.exports;
    if (!e?.gex_table_need || this.#tn.length > 24) return;
    const v = e.gex_table_need(0) >>> 0;
    const last = this.#tn[this.#tn.length - 1];
    if (!last || !last.endsWith(`:${v}`)) this.#tn.push(`${tag}:${v}`);
  }
  #iGrows = 0;
  #iAdded = 0;
  #iCopied = 0;
  #compileMs = 0;
  #instantiateMs = 0;
  #growMs = 0;
  /// What every Worker pays instantiating the shim and guest before the guest
  /// runs -- the floor under every process, which `installMs` does not cover.
  #shimCompileMs = 0;
  #shimInstantiateMs = 0;
  #guestInstantiateMs = 0;
  #installs = 0;
  // Instances whose slots have not been written, newest first: a miss asks
  // each in turn whether it holds the slot. See `fillSlot`.
  #unfilled = [];
  /** Closure libraries that arrived after this process started. */
  #streamed = 0;
  /** Closure libraries registered but not yet instantiated, in shim order. */
  #lazyClosure = [];
  /** Inherited modules registered but not yet instantiated, in shim order. */
  #lazyInherited = [];
  // Fill requests answered, and requests nobody could answer.
  #filled = 0;
  #unanswered = 0;
  #slowFills = 0;
  #slowMs = 0;
  #publishMs = 0;
  #publishes = 0;
  #pubGrowMs = 0;
  #pubGrewSlots = 0;
  #wantMs = 0;
  #wants = 0;
  #instMs = 0;
  #tailMs = 0;
  /// Modules that only instantiated after growing every table the running
  /// total implies. If common, the loose reading is the wrong default.
  #strictGrows = 0;
  #strictSlots = 0;
  /// Next free slot in each table, for pages this process claims.
  #claimed = [];
  // The parent's modules, held rather than installed. See `#maybeAdopt`.
  #inherited = [];
  /// The exec generation last seen, so `#maybeAdopt` can tell when this process
  /// has become a different program.
  #execGen = 0;
  #traps = 0;
  // What `load` was told, so a forked child can be told the same: a wrong tty
  // or identity changes how libc buffers and whether a shell prints a prompt.
  #opts = {};
  // Every module this process has installed, compiled. Kept because a forked
  // child needs the same code in the same slots, and a `WebAssembly.Module`
  // crosses to a Worker without recompiling.
  #compiled = [];
  // `{ module, memory }` for the global kernel and its instance. The global
  // kernel is a second wasm module holding state that spans processes (process
  // table, file table, pipes), instantiated by every Worker against the same
  // shared kernel memory. Null is a run of one process, which needs none of it.
  #kernelSpec = null;
  #census = null;
  #kernel = null;
  // This Worker's shim stack, claimed on the way in. See `#workerStack`.
  #stackTop = null;

  /**
   * One turn of the event loop, to deliver messages that arrived while this
   * Worker was parked. A message task, not a timer: nothing to wait for.
   */
  #turn() {
    if (!this.#tick) {
      this.#tick = new MessageChannel();
      this.#tick.port1.start();
    }
    return new Promise((r) => {
      this.#tick.port1.onmessage = () => r();
      this.#tick.port2.postMessage(0);
    });
  }

  /** Consecutive turns taken for a wait that is only on the network. */
  #netTurns = 0;

  /**
   * How the run loop waited, by kind. `park` is a sleep on the shared word
   * another Worker ends; `turn` is a MessageChannel task; `nap` is a timer.
   */
  waits = { park: 0, turn: 0, nap: 0 };

  // A MessageChannel first, then `setTimeout(_, 1)`: a bare timer nests and the
  // HTML spec clamps a nested timer to 4ms, but the level resets after a
  // message. Not a bare message task either -- that spins.
  #nap() {
    if (!this.#tick) {
      this.#tick = new MessageChannel();
      this.#tick.port1.start();
    }
    return new Promise((r) => {
      this.#tick.port1.onmessage = () => setTimeout(r, 1);
      this.#tick.port2.postMessage(0);
    });
  }

  constructor(env = new Env(), { shared = false, memory = null, kernel = null } = {}) {
    this.#env = env;
    // `memory` adopts a process's existing address space rather than making a
    // new one -- which is what a *thread* is: a second Worker on the same
    // memory, seeing the same guest and kernel. A new memory is a new process.
    this.#memory =
      memory ?? new WebAssembly.Memory({ initial: INITIAL_PAGES, maximum: PAGES, shared });
    this.#kernelSpec = kernel;
  }

  /**
   * Where this Worker's shim stack goes. Each Worker needs its own, since two
   * sharing a memory (a thread) would clash; `gex_worker_init` claims a stack
   * and thread-local block per Worker. The plain build has one of each.
   */
  #workerStack() {
    // Claimed once and remembered: asked again after every trap (the stack
    // pointer resets to the top, since a trap abandons the stack), and a fresh
    // slot each time would exhaust the sixty-four available in a second.
    if (this.#stackTop === null) {
      const at = this.#shim.exports.gex_worker_init?.();
      this.#stackTop = at ? at | 0 : SHIM_STACK_TOP | 0;
    }
    return this.#stackTop;
  }

  /**
   * Make this process's memory cover `end`, growing it if it does not. For the
   * places JavaScript writes guest memory directly -- a forked child's image
   * and nothing else; the shim grows for itself from inside.
   */
  #growTo(end) {
    const have = this.#memory.buffer.byteLength;
    if (end <= have) return;
    // A 64-bit memory counts its pages in BigInt, here as everywhere else.
    const pages = Math.ceil((end - have) / 65536);
    this.#memory.grow(pages);
    this.#u8 = null;
  }

  /** Closure library modules waiting for the guest instance to exist. */
  #closureModules = [];
  /// Ask whoever holds the closure for library `i`, compiled. Null when the
  /// caller handed the modules over up front, which the page still does for
  /// pid 1 -- it compiled them itself and has nowhere to ask.
  #wantLibrary = null;
  /// The dispatch pages to narrate, by number. Empty in a normal run; the gate
  /// is the set itself, since `opts.strace` never reaches a Worker. `?trace=1`
  /// prints for these only. See #136.
  static watchPages = new Set();
  /// Fill a whole image page on first ask. See `fillSlotNow`.
  static wholePage = false;
  /// How many times the fill has said why it gave up. See `fillSlotNow`.
  static #whyNo = 0;
  /// And how many times it has said what the fill itself answered.
  static #whyDid = 0;
  /// Report the exact slot a missing pc dispatches through. See `fillSlotNow`.
  static watchSlot = false;
  /// Narrate a fill request no module answered. Off by default: a closure that
  /// deliberately leaves a library out (`GEX_SKIP`, the crypto libs) hands
  /// every one of that library's pages to the run-time translator, and each is
  /// a fill nobody holds followed by a clean lift -- expected, not a fault, and
  /// a flood of `console.error` that fails a harness counting them. On to debug
  /// a genuinely unfillable page. See `#fillSlotTail`.
  static debugFill = false;
  /// What each ask saw, keyed `page:from`. See `fillSlotNow` and `liftSlots`.
  static askState = new Map();
  /// A dispatch page whose directory writes should be narrated, or -1: every
  /// writer of `DISPATCH_DIR` reports itself, naming the one that moves it.
  /// See #136.
  static watchDir = -1;
  static #whySlot = 0;

  #bytes() {
    // Rebuilt when the memory has grown, not only when detached. A shared
    // memory never detaches -- the old SharedArrayBuffer stays valid at its
    // old length -- so a length check is needed: the shim grows the memory
    // from inside on every guest `mmap`, and a stale view reads nothing.
    const buf = this.#memory.buffer;
    if (!this.#u8 || this.#u8.buffer !== buf || this.#u8.byteLength !== buf.byteLength) {
      this.#u8 = new Uint8Array(buf);
    }
    return this.#u8;
  }

  get memory() { return this.#memory; }

  /**
   * Let go of everything that would keep a host's event loop alive. `#tick` is
   * a started `MessageChannel`, and in node a started port is an active handle
   * that keeps the process alive on its own -- so a run that ever yielded never
   * exited. Closed here rather than `unref`'d so it holds the run while in use.
   */
  close() {
    if (this.#tick) {
      this.#tick.port1.close();
      this.#tick.port2.close();
      this.#tick = null;
    }
  }

  get shim() { return this.#shim; }

  /**
   * Translated modules this Worker has installed into its own dispatch tables.
   * A late-joining thread must reach the process's count or its tables miss
   * code the shared page directory already points at.
   */
  get installedModules() { return this.#installed; }
  /** Milliseconds this process spent compiling and instantiating modules. */
  get installMillis() { return Math.round(this.#installMs); }
  get compileMillis() { return Math.round(this.#compileMs); }
  get instantiateMillis() { return Math.round(this.#instantiateMs); }
  get growMillis() { return Math.round(this.#growMs); }
  /// Syscalls the guest's dynamic linker made, and when it made its last. See
  /// `gex_loader_calls`.
  get loader() {
    const e = this.#shim?.exports;
    if (!e?.gex_loader_calls) return null;
    return {
      calls: Number(e.gex_loader_calls()),
      doneMs: Number(e.gex_loader_done_ms()),
      // What the linked-image store did for this process, if anything.
      link: ['off', 'no-kernel', 'armed', 'kept', 'restored', 'asked'][
        Number(e.gex_link_state?.() ?? 0)
      ],
      // What restoring the address space cost, when one was restored: regions,
      // bytes re-read from files, bytes copied out of the image, ms in each.
      cost: e.gex_link_cost ? (() => {
        const p = e.gex_alloc(40) >>> 0;
        e.gex_link_cost(p);
        const v = new BigUint64Array(this.#bytes().buffer, p, 5);
        return [Number(v[0]), Number(v[1]), Number(v[2]), Number(v[3]), Number(v[4])];
      })() : null,
    };
  }
  /// Which objects this process lifted blocks from, `name=count` separated.
  get blocksByObject() {
    const e = this.#shim?.exports;
    if (!e?.gex_jit_by_object) return '';
    const max = 512;
    const p = e.gex_alloc(max) >>> 0;
    const n = e.gex_jit_by_object(p, max) >>> 0;
    const s = utf8(this.#bytes().subarray(p, p + n));
    e.gex_free?.(p, max);
    return s;
  }
  get shimCompileMillis() { return Math.round(this.#shimCompileMs); }
  get shimInstantiateMillis() { return Math.round(this.#shimInstantiateMs); }
  get guestInstantiateMillis() { return Math.round(this.#guestInstantiateMs); }
  /** How many modules that was. */
  get installCount() { return this.#installs; }

  /** Fill requests answered, and requests nobody could answer. */
  get fillStats() {
    return {
      filled: this.#filled,
      unanswered: this.#unanswered,
      // `fillSlotNow` (a directory write and table grow) vs `fillSlot` (an
      // await, which is a whole event-loop turn), separated because neither
      // showed in the other counters.
      now: this.#nowFills,
      nowMs: Math.round(this.#nowMs),
      sysMs: Math.round(Number(this.#shim.exports.gex_sys_us?.() ?? 0n) / 1000),
      sysN: Number(this.#shim.exports.gex_sys_calls?.() ?? 0n),
      faBase: Math.round(this.#faBaseMs),
      faInit: Math.round(this.#faInitMs),
      slow: this.#slowFills,
      slowMs: Math.round(this.#slowMs),
      pub: this.#publishes,
      pubMs: Math.round(this.#publishMs),
      growMs: Math.round(this.#pubGrowMs),
      grew: this.#pubGrewSlots,
      wants: this.#wants,
      wantMs: Math.round(this.#wantMs),
      instMs: Math.round(this.#instMs),
      // What `instMs` is made of: `iGrow` is `Table.grow`, `iComp` is
      // `WebAssembly.compile` (should be ~0 -- a `Module` clones by reference),
      // `iInst` is `new WebAssembly.Instance`.
      iGrow: Math.round(this.#growMs),
      iComp: Math.round(this.#compileMs),
      iInst: Math.round(this.#instantiateMs),
      iGrows: this.#iGrows,
      iAdded: this.#iAdded,
      iCopied: this.#iCopied,
      bigGrows: this.#bigGrows.slice(0, 6).join(' | '),
      tn: this.#tn.join(' '),
      tlen: this.#shim ? this.#dispatchTables().map((t) => t.length).join('/') : '',
      tailMs: Math.round(this.#tailMs),
      strictGrows: this.#strictGrows,
      strictSlots: this.#strictSlots,
    };
  }

  /**
   * Read a file back out of the guest's filesystem, or null if absent. The
   * other half of bundling: a page that runs a compiler wants the object file.
   */
  readFile(path) {
    const p = new TextEncoder().encode(path);
    const pp = this.#shim.exports.gex_alloc(p.length) >>> 0;
    this.#bytes().set(p, pp);
    const len = this.#shim.exports.gex_get_file(pp, p.length) >>> 0;
    if (len === 0) return null;
    const at = this.#shim.exports.gex_file_data() >>> 0;
    return this.#bytes().slice(at, at + len);
  }


  /**
   * Everything the shim asks of JS: console, clock, network, two graphics
   * paths. A method, not an inline object, because `attach` needs the same set.
   */
  #hostImports() {
    return {
        write: (fd, ptr, len) =>
          this.#env.write(fd, this.#bytes().subarray(ptr >>> 0, (ptr >>> 0) + (len >>> 0))),
        // The shim's own narration, to the console -- never to the guest's
        // output, which shares fd 2 with a terminal a person is reading.
        debug: (ptr, len) => console.debug(
          utf8(
            this.#bytes().subarray(ptr >>> 0, (ptr >>> 0) + (len >>> 0)),
          ).replace(/\n$/, ''),
        ),
        warn: (ptr, len) => console.error(
          utf8(
            this.#bytes().subarray(ptr >>> 0, (ptr >>> 0) + (len >>> 0)),
          ).replace(/\n$/, ''),
        ),
        now: () => this.#env.now(),
        // The network. Every one of these must return without waiting: there is
        // one wasm stack, and the guest thread is parked by the kernel, not JS.
        net_open: () => (this.#net ? this.#net.open() : 0),
        net_connect: (id, host, len, port) => {
          const name = utf8(
            this.#bytes().subarray(host >>> 0, (host >>> 0) + (len >>> 0)),
          );
          this.#net?.connect(id, name, port);
        },
        net_status: (id) => (this.#net ? this.#net.status(id) : 2),
        net_send: (id, ptr, len) =>
          this.#net
            ? this.#net.send(id, this.#bytes().subarray(ptr >>> 0, (ptr >>> 0) + (len >>> 0)))
            : -1,
        net_recv: (id, ptr, len) =>
          this.#net
            ? this.#net.recv(id, this.#bytes().subarray(ptr >>> 0, (ptr >>> 0) + (len >>> 0)))
            : 0,
        net_avail: (id) => (this.#net ? (this.#net.available(id) ? 1 : 0) : 1),
        net_close: (id) => this.#net?.close(id),
        // Graphics: the guest's GL library batched commands in its own memory;
        // this hands over the address and the page runs them on WebGL.
        gl_submit: (ring) => (this.#gl ? this.#gl.submit(this.#memory, ring >>> 0) : -1),
        // The other GPU path: unmodified Mesa speaking VirGL over a socket,
        // decoded by virglrenderer (a separate wasm module with its own heap,
        // so bytes not an address -- see web/src/virgl.js). `session` says
        // which vtest client these bytes belong to.
        virgl_submit: (session, ptr, len) =>
          this.#virgl
            ? this.#virgl.submit(session >>> 0, this.#memory, ptr >>> 0, len >>> 0)
            : -1,
        // The compositor's output: a wayland client committed a frame, and its
        // pixels are at `ptr` (tightly-packed RGBA) in this memory. One-way --
        // the page copies them to its canvas; nothing is returned.
        wl_present: (ptr, w, h, stride, format) =>
          this.#wl?.present(this.#memory, ptr >>> 0, w, h, stride >>> 0, format >>> 0),
        // The reverse: drain the page's mouse/keyboard ring into `ptr` as packed
        // i32 quintuples, so the compositor pump can deliver them to the client.
        wl_poll_input: (ptr, max) =>
          this.#wl?.pollInput(this.#memory, ptr >>> 0, max >>> 0) ?? 0,
        virgl_take: (session, ptr, max) =>
          this.#virgl
            ? this.#virgl.take(session >>> 0, this.#memory, ptr >>> 0, max >>> 0)
            : 0,
        virgl_close: (session) => this.#virgl?.close(session >>> 0),
    };
  }

  /**
   * `shimWasm` is shims/browser's build output, `guestWasm` a module from
   * `gexc build --exhaustive --no-shared-memory`, `elf` the program's bytes.
   *
   * `files` is a map of guest path to contents. A browser has no filesystem to
   * pass through to, so whatever the program needs to read is bundled here.
   */
  async load(shimWasm, guestWasm, elf, files = {}, args = [], opts = {}) {
    // A compiled `WebAssembly.Module` if the host has one, bytes otherwise:
    // a module is structured-cloneable and the engine shares the code behind
    // it, so passing one saves recompiling the 8.7 MB shim per Worker.
    const shimMod =
      shimWasm instanceof WebAssembly.Module ? shimWasm : await WebAssembly.compile(shimWasm);
    await this.#startKernel();
    this.#shim = await WebAssembly.instantiate(shimMod, {
      env: { memory: this.#memory },
      gex_env: this.#hostImports(),
      gex_k: this.#kernelImports(shimMod),
    });
    this.#u8 = null;
    // First, before any call into the shim: `gex_worker_init` gives this Worker
    // its thread-local block, and until it runs `__tls_base` is zero, so a shim
    // function touching a thread-local writes through a null base.
    this.#shim.exports.__stack_pointer.value = this.#workerStack();
    // An older shim loads but fails on the first call to something it lacks,
    // far from the mistake. Named here so the skew is a sentence, not a mystery.
    const required = [
      'gex_alloc', 'gex_load', 'gex_add_file', 'gex_push_stdin', 'gex_wake',
      // Each of these is called through `?.` at its use site so a shim that
      // predates it still loads -- which is how a stale runtime turns into a
      // silent wrong answer (no `gex_chmod` leaves every file 0644).
      'gex_chmod', 'gex_add_dir', 'gex_add_env',
      'gex_paused_for_input', 'gex_paused_for_net', 'gex_paused_for_host',
      'gex_park', 'gex_resume_pc',
      'gex_resume_ctx', 'gex_exit_code', 'gex_jit_pending_len',
    ];
    const missing = required.filter((n) => typeof this.#shim.exports[n] !== 'function');
    if (missing.length) {
      throw new Error(
        `the gex runtime is out of date: it has no ${missing.join(', ')}. ` +
          'Rebuild it with web/build-assets.sh and reload.',
      );
    }
    // Whatever `useGl` was told before the shim existed.
    this.#shim.exports.gex_set_gl?.(this.#gl ? 1 : 0);
    this.#shim.exports.gex_set_wl?.(this.#wl ? 1 : 0);
    // Only when there really is one: Mesa's vtest winsys does not fall back
    // from a socket nobody serves -- it blocks on a reply that cannot come.
    this.#shim.exports.gex_set_virgl?.(this.#virgl ? (this.#virglTrace ? 2 : 1) : 0);
    // `kernel.net` is decided inside `gex_load` from this flag, so it must be
    // set before it (not merely in `useNetwork`, before the shim exists).
    this.#shim.exports.gex_set_network?.(this.#net ? 1 : 0);
    // Narrate one page's directory writes, in every process. See #136.
    if (Gex.watchDir >= 0) this.#shim.exports.gex_watch_dir?.(BigInt(Gex.watchDir));
    // wasm-ld puts the shim's stack at address zero (the guest's image); move
    // it into the shim's own region, above everything the guest maps.
    this.#shim.exports.__stack_pointer.value = this.#workerStack();
    this.#opts = { strace: opts.strace, tty: opts.tty, uid: opts.uid, gid: opts.gid };
    if (opts.strace) this.#shim.exports.gex_set_trace(1);
    // How many of an object's stored units one miss may take.
    if (opts.perMiss) this.#shim.exports.gex_set_per_miss?.(opts.perMiss);
    // Keep what the guest's dynamic linker produces, and start the next process
    // of the same program from it instead of linking again.
    if (opts.link) this.#shim.exports.gex_set_snapshot?.(Number(opts.link));
    // A terminal changes how libc buffers and whether a shell prints a prompt.
    if (opts.tty) this.#shim.exports.gex_set_tty(1);
    // A page has no user; an embedder that has one says so.
    if (opts.uid !== undefined) this.#shim.exports.gex_set_identity(opts.uid, opts.gid ?? opts.uid);

    // The module describes its own dispatch in a custom section; runtime
    // translation needs it to carry on allocating slots where the build stopped.
    this.#setMetadata(guestWasm);
    // Where the guest starts, for a program given relative paths.
    if (opts.cwd) {
      const c = new TextEncoder().encode(opts.cwd);
      const cp = this.#shim.exports.gex_alloc(c.length) >>> 0;
      this.#bytes().set(c, cp);
      this.#shim.exports.gex_set_cwd(cp, c.length);
    }
    // Standard input, if the embedder has any: a page has nothing to block
    // on, so the guest is given all of it up front.
    if (opts.stdin && opts.stdin.length) {
      const sp = this.#shim.exports.gex_alloc(opts.stdin.length) >>> 0;
      this.#bytes().set(opts.stdin, sp);
      this.#shim.exports.gex_set_stdin(sp, opts.stdin.length);
    }
    // The bundle goes over before the program does: a dynamic executable names
    // an interpreter the loader reads out of this filesystem. Freed one file at
    // a time -- `gex_add_file` copies, so the buffer is dead once it returns,
    // and keeping it doubled a rootfs's cost in this process's memory.
    const enc = new TextEncoder();
    const free = this.#shim.exports.gex_free;
    // Empty directories first, so a file arriving under one finds it.
    // `gex_add_file` makes directories on the way to each file, so an empty one
    // would otherwise arrive not at all; `web/src/tar.js` collects them.
    for (const path of opts.dirs ?? []) {
      const p = enc.encode(path);
      const pp = this.#shim.exports.gex_alloc(p.length) >>> 0;
      this.#bytes().set(p, pp);
      this.#shim.exports.gex_add_dir?.(pp, p.length);
      free?.(pp, p.length);
    }
    // Which of them are executable: the VFS makes every file 0644 and the tar's
    // mode did not travel with its bytes, so programs read as not executable.
    const modes = opts.modes ?? {};
    // The environment, when the page has one in mind. Empty leaves the shim's
    // default (Debian's PATH for root plus HOME).
    for (const kv of opts.env ?? []) {
      const e = enc.encode(kv);
      const ep = this.#shim.exports.gex_alloc(e.length) >>> 0;
      this.#bytes().set(e, ep);
      this.#shim.exports.gex_add_env?.(ep, e.length);
      free?.(ep, e.length);
    }
    for (const [path, contents] of Object.entries(files)) {
      const p = enc.encode(path);
      const d = contents instanceof Uint8Array ? contents : enc.encode(contents);
      const pp = this.#shim.exports.gex_alloc(p.length) >>> 0;
      const dp = this.#shim.exports.gex_alloc(d.length) >>> 0;
      this.#bytes().set(p, pp);
      this.#bytes().set(d, dp);
      this.#shim.exports.gex_add_file(pp, p.length, dp, d.length);
      // Optional so an older shim still loads.
      if (modes[path] !== undefined) {
        this.#shim.exports.gex_chmod?.(pp, p.length, modes[path]);
      }
      free?.(pp, p.length);
      free?.(dp, d.length);
    }

    // Which program the image module was built from, so the shim can refuse to
    // hand its pages to a process running something else. See
    // `image_pages_are_ours`.
    if (opts.program && this.#shim.exports.gex_set_image_program) {
      const b = enc.encode(opts.program);
      const bp = this.#shim.exports.gex_alloc(b.length) >>> 0;
      this.#bytes().set(b, bp);
      this.#shim.exports.gex_set_image_program(bp, b.length);
      free?.(bp, b.length);
    }
    const pin = this.#shim.exports.gex_pin_object;
    for (const [path, where] of Object.entries(opts.pins ?? {})) {
      if (!pin) break;
      const b = enc.encode(path);
      const bp = this.#shim.exports.gex_alloc(b.length) >>> 0;
      this.#bytes().set(b, bp);
      // The span as well as the base: the placement table needs to know how
      // much of the address space a pinned object owns, or it hands the next
      // one an address inside it.
      pin(bp, b.length, BigInt(where.base ?? where), BigInt(where.span ?? 0));
      free?.(bp, b.length);
    }

    const ptr = this.#shim.exports.gex_alloc(elf.length) >>> 0;
    this.#bytes().set(elf, ptr);
    const argvBlob = new TextEncoder().encode(args.join('\0'));
    let ap = 0;
    if (argvBlob.length) {
      ap = this.#shim.exports.gex_alloc(argvBlob.length) >>> 0;
      this.#bytes().set(argvBlob, ap);
    }
    // Phase markers, so a boot that never finishes says where it stopped.
    if (globalThis.GEX_DEBUG_EXIT) {
      console.error(`[gex] load: ${Object.keys(opts.pins ?? {}).length} pins, `
        + `${Object.keys(files).length} files staged, loading the program`);
    }
    const entry = this.#shim.exports.gex_load(ptr, elf.length, ap, argvBlob.length);
    if (entry === 0n) throw new Error('gex: could not load the program');
    if (globalThis.GEX_DEBUG_EXIT) {
      console.error(`[gex] load: program at ${entry}, `
        + `${(opts.closure ?? []).length} closure modules to install`);
    }

    // The rest of the closure: one module per shared library, translated ahead
    // of time by the same `gexc` run as the image. Held until now because the
    // image must be placed first (its metadata says how far the tables reach)
    // and `#installClosure` below builds the tables the modules import.
    this.#closureModules = opts.closure ?? [];
    this.#wantLibrary = opts.wantLibrary ?? this.#wantLibrary;


    const e = this.#shim.exports;
    this.#imports = {
      memory: this.#memory,
      syscall: e.gex_syscall,
      dispatch_miss: e.gex_dispatch_miss,
      illegal: e.gex_illegal,
      breakpoint: e.gex_breakpoint,
      sysreg_read: e.gex_sysreg_read,
      sysreg_write: e.gex_sysreg_write,
      cache_op: e.gex_cache_op,
      safepoint: e.gex_safepoint,
      fma: e.gex_fma,
    };
    const tGuest = performance.now();
    this.#guest = await WebAssembly.instantiate(
      guestWasm instanceof WebAssembly.Module ? guestWasm : await WebAssembly.compile(guestWasm),
      { gex: this.#imports },
    );
    this.#guestInstantiateMs = performance.now() - tGuest;
    this.#tnAt('boot');
    this.#offerToFill(this.#guest);
    if (globalThis.GEX_DEBUG_EXIT) console.error('[gex] load: instantiated, installing');
    await this.#installClosure();
    if (globalThis.GEX_DEBUG_EXIT) console.error('[gex] load: done');
    return entry;
  }

  /**
   * Take one module a fork handed over, without instantiating it yet. A child
   * branches into almost none of what it inherits, so the directory entries are
   * published now but the instance waits for a branch, as a closure library
   * does. Without metadata there is nothing to key a later fill on, so those
   * install the old way (as does the image).
   */
  async #adoptModule(m) {
    const e = this.#shim.exports;
    // What this module actually reaches: a closure library claims zero here.
    // See `slotsNeeded`.
    const truly = Math.max(m.need >>> 0, slotsNeeded(m.meta));
    if (!m.meta || !m.meta.length || !e.gex_add_lazy_module) {
      await this.#installModule(m.mod, truly, 64, m.meta, 'adopt-nometa');
      return;
    }
    const gp = e.gex_alloc(m.meta.length) >>> 0;
    this.#bytes().set(m.meta, gp);
    const need = e.gex_add_lazy_module(gp, m.meta.length) >>> 0;
    e.gex_free?.(gp, m.meta.length);
    if (!need) {
      await this.#installModule(m.mod, truly, 64, m.meta, 'adopt-noneed');
      return;
    }
    this.#growTables(Math.max(need, truly), 'adopt');
    this.#lazyInherited.push({ mod: m.mod, need: truly, inst: null, meta: m.meta });
  }

  /**
   * Install one closure library, arriving after the process is already up.
   *
   * The whole closure cannot be handed to a Worker at once (694 MB of
   * `WebAssembly.Module` overflows `postMessage`), so they come one at a time
   * and the guest runs while they arrive -- safe because the miss path forgives
   * a page whose module has not landed and asks again when it does.
   */
  async installClosureModule(entry) {
    if (!this.#shim || !entry) return false;
    const meta = entry instanceof Uint8Array ? metadataSection(entry) : entry.meta;
    if (!meta || !meta.length) return false;
    const add = this.#shim.exports.gex_add_closure_module;
    if (!add) return false;
    const gp = this.#shim.exports.gex_alloc(meta.length) >>> 0;
    this.#bytes().set(meta, gp);
    const need = add(gp, meta.length) >>> 0;
    this.#shim.exports.gex_free?.(gp, meta.length);
    if (!need) return false;
    const mod = entry instanceof Uint8Array ? entry : entry.mod;
    // Registered and reserved, not instantiated: the directory points at this
    // library's pages and the tables are grown, so a branch resolves and
    // faults. `fillSlot` builds the instance when a branch actually wants one
    // -- most of a closure's libraries are never reached.
    this.#growTables(need, 'closure');
    this.#lazyClosure.push({ mod, need, inst: null });
    this.#streamed++;
    return true;
  }

  async #installClosure() {
    const add = this.#shim.exports.gex_add_closure_module;
    if (!add) return;
    // The highest slot total any of them needs, so the tables grow once.
    let most = 0;
    for (const [i, entry] of this.#closureModules.entries()) {
      // Either the raw wasm, or the pair a page should send: the metadata
      // section alone plus the already-compiled module. A `Module` crosses
      // `postMessage` by reference, so the pair avoids cloning and recompiling
      // hundreds of megabytes of closure per process.
      const paired = !(entry instanceof Uint8Array);
      const wasm = paired ? null : entry;
      const meta = paired ? entry.meta : metadataSection(entry);
      let need = 0;
      if (meta && meta.length) {
        const t0 = performance.now();
        const gp = this.#shim.exports.gex_alloc(meta.length) >>> 0;
        this.#bytes().set(meta, gp);
        const tCopied = performance.now();
        // With the program this library is the image of, when it is one: every
        // root of a closure claims `PIE_BASE`, so they are told apart by program.
        const prog = new TextEncoder().encode(paired ? (entry.program ?? '') : '');
        let pp = 0;
        if (prog.length) {
          pp = this.#shim.exports.gex_alloc(prog.length) >>> 0;
          this.#bytes().set(prog, pp);
        }
        need = add(gp, meta.length, pp, prog.length) >>> 0;
        if (pp) this.#shim.exports.gex_free?.(pp, prog.length);
        const tAdded = performance.now();
        this.#shim.exports.gex_free?.(gp, meta.length);
        if (globalThis.GEX_DEBUG_EXIT) {
          console.error(
            `[closure] library ${i}: ${(meta.length / 1024) | 0} KiB of metadata, `
            + `${Math.round(tCopied - t0)}ms copying, `
            + `${Math.round(tAdded - tCopied)}ms registering, ${need} slots`,
          );
        }
      }
      // `need` is 0 now: registration reserves and publishes nothing. The
      // metadata is kept because `fillSlot` publishes from it. Registered and
      // reserved, not instantiated -- `fillSlot` builds an instance when a
      // branch wants a page it covers, and most libraries are never reached.
      void need;
      most = Math.max(most, need);
      this.#lazyClosure.push({
        mod: paired ? entry.mod : wasm,
        // From the metadata, not `need` (which registration returns as zero).
        // `#publish` grows the tables when a page is first wanted.
        need: Math.max(need, slotsNeeded(meta)),
        inst: null,
        meta,
        published: false,
      });
    }
    // Nothing is grown here: `#publish` grows for a library when a branch asks
    // for one of its pages, instead of growing to the whole closure up front.
    void most;
    this.#closureModules = [];
  }

  /**
   * Join a process that is already loaded, as one of its threads. Same memory,
   * shim and guest as `load`, but no program is placed and no kernel built --
   * both already exist in the adopted memory.
   *
   * Instantiating a second time is safe only because the shared build is linked
   * with `--shared-memory`: wasm-ld emits passive data segments and an
   * atomic-guarded `__wasm_init_memory`, so the first instance initialises the
   * data and later ones leave it alone. A plain memory would wipe the kernel.
   */
  async attach(shimWasm, guestWasm, { devices = true } = {}) {
    await this.#startKernel();
    // A compiled `Module` if the host has one, bytes otherwise: passing a
    // module saves recompiling the shim per Worker. See `load`.
    const tShimC = performance.now();
    const shimMod =
      shimWasm instanceof WebAssembly.Module ? shimWasm : await WebAssembly.compile(shimWasm);
    this.#shimCompileMs = performance.now() - tShimC;
    const tShimI = performance.now();
    this.#shim = await WebAssembly.instantiate(shimMod, {
      env: { memory: this.#memory },
      gex_env: this.#hostImports(),
      gex_k: this.#kernelImports(shimMod),
    });
    this.#shimInstantiateMs = performance.now() - tShimI;
    this.#u8 = null;
    // The stack pointer is a per-instance global, so every Worker has its own
    // and must be told where it goes.
    this.#shim.exports.__stack_pointer.value = this.#workerStack();
    // The devices this process has -- process-wide flags in shared memory, so
    // only a *process* sets them. A thread answering for itself would turn a
    // GPU or network off underneath the threads using it. Before the kernel is
    // built, which is when the device nodes are made and the vtest socket bound.
    if (devices) {
      this.#shim.exports.gex_set_gl?.(this.#gl ? 1 : 0);
      this.#shim.exports.gex_set_wl?.(this.#wl ? 1 : 0);
      this.#shim.exports.gex_set_virgl?.(this.#virgl ? (this.#virglTrace ? 2 : 1) : 0);
      this.#shim.exports.gex_set_network?.(this.#net ? 1 : 0);
    // Narrate one page's directory writes, in every process. See #136.
    if (Gex.watchDir >= 0) this.#shim.exports.gex_watch_dir?.(BigInt(Gex.watchDir));
    }
    const e = this.#shim.exports;
    this.#imports = {
      memory: this.#memory,
      syscall: e.gex_syscall,
      dispatch_miss: e.gex_dispatch_miss,
      illegal: e.gex_illegal,
      breakpoint: e.gex_breakpoint,
      sysreg_read: e.gex_sysreg_read,
      sysreg_write: e.gex_sysreg_write,
      cache_op: e.gex_cache_op,
      safepoint: e.gex_safepoint,
      fma: e.gex_fma,
    };
    const tGuest = performance.now();
    this.#guest = await WebAssembly.instantiate(
      guestWasm instanceof WebAssembly.Module ? guestWasm : await WebAssembly.compile(guestWasm),
      { gex: this.#imports },
    );
    this.#guestInstantiateMs = performance.now() - tGuest;
    this.#offerToFill(this.#guest);
    // Everything the process translated before this thread existed.
    await this.#catchUp();
  }

  /**
   * Everything this process has mapped, as `{ at, bytes }` -- what a forked
   * child needs and only that: its image, stack and heap (the parent's after
   * `fork`), plus the low region, where the register contexts and dispatch
   * directory live below the image and outside the memory map.
   *
   * Copied rather than shared: a copy once at `fork`, not a copy on every
   * switch for the life of both, which is what the switcher was built to cope
   * with.
   */
  #processImage() {
    const t0 = performance.now();
    const e = this.#shim.exports;
    const out = [{ at: 0, bytes: this.#bytes().slice(0, LOW_END) }];
    const scratch = e.gex_alloc(24) >>> 0;
    const dv = new DataView(this.#memory.buffer);
    const n = e.gex_vma_count();
    for (let i = 0; i < n; i++) {
      if (e.gex_vma_at(i, scratch) !== 0) continue;
      const start = Number(dv.getBigUint64(scratch, true));
      const end = Number(dv.getBigUint64(scratch + 8, true));
      const prot = Number(dv.getBigUint64(scratch + 16, true));
      // `prot` travels with the bytes: the child rebuilds its memory *map* from
      // these, and a map is not recoverable from the memory it describes.
      if (end > start) out.push({ at: start, end, prot, bytes: this.#bytes().slice(start, end) });
    }
    e.gex_free?.(scratch, 24);
    // A checksum per region, so the child can say whether what it runs on is
    // what was sent: a corrupt copy and a good copy under a disagreeing kernel
    // look identical otherwise (#96).
    if (globalThis.GEX_DEBUG_EXIT) for (const r of out) r.sum = sum32(r.bytes);
    if (globalThis.GEX_DEBUG_EXIT) {
      const bytes = out.reduce((n, r) => n + r.bytes.byteLength, 0);
      // How much of it the child could have re-read instead. See
      // `gex_vma_reloadable`: read-only library text is identical to its file.
      let again = 0;
      if (e.gex_vma_reloadable) {
        for (let i = 0; i < n; i++) {
          if (e.gex_vma_at(i, scratch) !== 0) continue;
          if (!e.gex_vma_reloadable(i)) continue;
          const a = Number(dv.getBigUint64(scratch, true));
          const b = Number(dv.getBigUint64(scratch + 8, true));
          if (b > a) again += b - a;
        }
      }
      console.error(
        `[gex] fork image: ${out.length} regions, ${(bytes / 1048576).toFixed(1)} MiB, `
        + `${(again / 1048576).toFixed(1)} MiB of it re-readable, `
        + `${Math.round(performance.now() - t0)}ms to copy`,
      );
    }
    return out;
  }

  /**
   * The memory map whole: each region's name and the file its code came from,
   * which the three numbers of `gex_vma_at` cannot carry. A child given only
   * spans identifies no region, finds nothing in the shared module store, and
   * lifts its parent's libraries again.
   */
  #vmaBlob() {
    const e = this.#shim.exports;
    if (!e.gex_vma_blob_len || !e.gex_vma_blob_read) return null;
    const len = e.gex_vma_blob_len() >>> 0;
    if (!len) return null;
    const buf = e.gex_alloc(len) >>> 0;
    let blob = null;
    if (e.gex_vma_blob_read(buf, len) === len) {
      blob = new Uint8Array(this.#memory.buffer, buf, len).slice();
    }
    e.gex_free?.(buf, len);
    return blob;
  }

  /**
   * Instantiate the global kernel against this process's memory. Every Worker
   * does this with the same module and the same *kernel* memory (where the
   * module's Rust state lives, so all see one process table) and a different
   * process memory (memory 1, how it copies bytes in and out). Nothing without
   * a `kernel` in the constructor options: one process needs no such table.
   */
  async #startKernel() {
    if (!this.#kernelSpec || this.#kernel) return;
    const { module, memory, stackTop } = this.#kernelSpec;
    const mod = module instanceof WebAssembly.Module
      ? module
      : await WebAssembly.compile(module);
    const inst = await WebAssembly.instantiate(mod, {
      env: { memory, process: this.#memory },
    });
    // The stack, before anything is called. Each Worker's `__stack_pointer` is
    // a per-instance global that wasm-ld inits to the same address in shared
    // memory, so two Workers in the kernel would overwrite each other's locals.
    // `stackTop` is a per-Worker slice the kernel reserves; the first Worker
    // has none and keeps the linked stack.
    if (stackTop) inst.exports.__stack_pointer.value = stackTop | 0;
    this.#kernel = inst.exports;
  }

  /**
   * Where the global kernel's per-Worker stacks are, for the host to hand out.
   * Null when there is no global kernel or it is too old to say. Read from this
   * Worker, which must be the first: see `#startKernel`.
   */
  kernelStacks() {
    const k = this.#kernel;
    if (!k?.k_stack_area) return null;
    return { area: k.k_stack_area() >>> 0, size: k.k_stack_size() >>> 0, count: k.k_stack_count() >>> 0 };
  }

  /// Armed once: every few seconds, say what this process is waiting for.
  /// A deadlock reaches neither census the shim already had (idle spins, wakes),
  /// so a wedged run printed nothing at all. Around the whole run loop, since a
  /// process parked on a pipe another process fills exits the loop differently.
  #armCensus() {
    if (this.#census) return;
    const e = this.#shim?.exports;
    if (!e?.gex_wait_census) return;
    let last = '';
    this.#census = setInterval(() => {
      const p = e.gex_alloc(512) >>> 0;
      const n = e.gex_wait_census(p, 512) >>> 0;
      if (n) {
        const text = utf8(this.#bytes().slice(p, p + n));
        // Only when it has stopped changing: a wedged process repeats the same
        // wait forever, which is the signal; a busy one churns through them.
        if (text === last) console.debug(`[gex] quiet: ${text}`);
        last = text;
      }
      e.gex_free?.(p, 512);
    }, 3000);
    // Node keeps a process alive for a pending timer; this must not be the
    // reason a Worker outlives its guest.
    this.#census.unref?.();
  }

  /** The global kernel's exports, or null. */
  get kernel() { return this.#kernel; }

  /**
   * What the shim calls to reach the process table every process shares.
   * `k_present` is the whole negotiation: zero means no global kernel, and the
   * shim keeps the table in its own memory. The stubs below then go unused --
   * they exist only because a wasm import cannot be absent.
   */
  #kernelImports(shimMod) {
    const k = this.#kernel;
    const absent = () => 0;
    // What the shim actually imports, from `WebAssembly.Module.imports` -- the
    // only authority. A hand-written list was a third place to keep in step and
    // `check-glue.mjs` did not catch drift, because these arrive via the kernel.
    const asked = shimMod
      ? WebAssembly.Module.imports(shimMod)
          .filter((i) => i.module === 'gex_k' && i.kind === 'function')
          .map((i) => i.name)
      : [];
    // Kept only for a caller that has no module to ask -- there is none in
    // this file, and this list is not to be extended.
    const LEGACY_NAMES = [
      'k_spawn', 'k_exit', 'k_reap', 'k_wait', 'k_wait_ready',
      'k_has_children', 'k_kill', 'k_take_signals', 'k_has_signals',
      'k_ppid', 'k_pgid', 'k_sid', 'k_setsid', 'k_setpgid',
      'k_request_halt', 'k_halt_code',
      'k_fs_open', 'k_fs_close', 'k_fs_read', 'k_fs_write', 'k_fs_meta',
      'k_fs_truncate', 'k_fs_read_dir', 'k_fs_path_meta', 'k_fs_read_link',
      'k_fs_unlink', 'k_fs_mkdir', 'k_fs_rmdir', 'k_fs_rename', 'k_fs_chmod',
      'k_fs_symlink', 'k_fs_link', 'k_fs_mksocket', 'k_fs_add', 'k_fs_populated',
      'k_fs_populate_done',
      'k_pipe_new', 'k_pipe_open_end', 'k_pipe_close_end', 'k_pipe_read',
      'k_pipe_write', 'k_pipe_readable', 'k_pipe_writable', 'k_pipe_buffered',
      'k_sock_bind', 'k_sock_connect', 'k_sock_accept', 'k_sock_pending',
      'k_sock_read', 'k_sock_write', 'k_sock_readable', 'k_sock_writable',
      'k_sock_available', 'k_sock_close', 'k_sock_shutdown', 'k_sock_exists',
      'k_virgl_take', 'k_virgl_release',
      'k_wake_seq', 'k_wake', 'k_park',
      // Which dispatch slots an identity's code is in, system-wide rather than
      // per Worker. Without a kernel these answer zero: one process has no
      // sibling to share with, and the shim keeps its own map.
      'k_slot_get', 'k_slot_put', 'k_slot_alloc', 'k_slot_floor',
      'k_stdin_write', 'k_stdin_read', 'k_stdin_ready', 'k_stdin_eof',
      'k_mod_claim_object', 'k_mod_for_object',
      'k_place', 'k_place_set',
      'k_mod_store', 'k_mod_claim', 'k_mod_for', 'k_mod_slots',
      'k_mod_len', 'k_mod_dirs_len', 'k_mod_read', 'k_mod_dirs_read',
    ];
    const NAMES = asked.length ? asked : LEGACY_NAMES;
    // Not imported by the shim -- the host reads it after a crash -- so it is
    // not in the list above and its absence is not a link error.
    // Two of these return an i64, which JS has to answer with a BigInt.
    const BIG = new Set(['k_wait', 'k_take_signals', 'k_tty_winsize']);
    const out = { k_present: () => (k ? 1 : 0) };
    for (const n of NAMES) {
      // Some `gex_k` imports are answered here, not by the kernel (`k_present`
      // is the negotiation itself), so skip names this file already owns.
      if (n in out) continue;
      if (k) {
        if (typeof k[n] !== 'function') {
          throw new Error(
            `the global kernel has no ${n}: it is older than this glue. ` +
              'Rebuild it with shims/kernel/build.sh.',
          );
        }
        out[n] = k[n];
      } else {
        out[n] = BIG.has(n) ? () => 0n : absent;
      }
    }
    // Nobody has asked for a halt, rather than "halt with status 0".
    if (!k) out.k_halt_code = () => -1;
    return out;
  }

  /**
   * Hand the shim the module's `gex.meta` section -- the record of where the
   * ahead-of-time build stopped, which runtime translation carries on from.
   * Without it a kernel reports "nothing to translate" at the first uncovered
   * address.
   */
  #setMetadata(guestWasm) {
    const meta = metadataSection(guestWasm);
    if (!meta || !meta.length) return;
    const mp = this.#shim.exports.gex_alloc(meta.length) >>> 0;
    this.#bytes().set(meta, mp);
    this.#shim.exports.gex_set_metadata(mp, meta.length);
  }

  /**
   * Which dispatch slots this process's translated pages are in. Read as one
   * block, not page by page: a real process has tens of thousands of pages.
   */
  #pageMap() {
    const e = this.#shim.exports;
    if (!e.gex_page_count) return [];
    const n = e.gex_page_count() >>> 0;
    if (!n) return [];
    const p = e.gex_alloc(n * 12) >>> 0;
    const wrote = e.gex_page_dump(p, n * 12);
    const out = wrote > 0 ? this.#bytes().slice(p, p + wrote * 12) : new Uint8Array(0);
    e.gex_free?.(p, n * 12);
    return out;
  }

  /**
   * Which object each translated page belongs to, as identities. The companion
   * to `#pageMap` and the one that outlives an `execve`: the page map describes
   * an address space about to be replaced, this describes the libraries in it.
   */
  #sharedMap() {
    const e = this.#shim.exports;
    if (!e.gex_shared_count) return [];
    const n = e.gex_shared_count() >>> 0;
    if (!n) return [];
    const p = e.gex_alloc(n * 20) >>> 0;
    const wrote = e.gex_shared_dump(p, n * 20);
    const out = wrote > 0 ? this.#bytes().slice(p, p + wrote * 20) : new Uint8Array(0);
    e.gex_free?.(p, n * 20);
    return out;
  }

  /**
   * Become the child a `fork` in another Worker produced.
   *
   * Everything a process is, in dependency order: its memory (the parent's
   * mapped regions, copied), a shim and guest instantiated against it, a kernel
   * (pid, break, memory map), its dispatch slots and the code in them. The
   * child inherits the parent's dispatch *directory* with the memory, so its
   * code branches into slots the parent allocated -- and without the same map
   * and modules those slots are empty or past the end of an ungrown table.
   */
  async adoptProcess(shimWasm, guestWasm, child, opts = {}) {
    // Narrated and timed when asked: every step is a place a child can stop
    // dead, and a hang is indistinguishable from a slow start otherwise.
    const began = performance.now();
    let last = began;
    // `GEX_ADOPT_TRACE` narrates adopt phases WITHOUT the heavy GEX_DEBUG_EXIT
    // apparatus (per-guest-write console.error flood + image sum-checks), so a
    // near-native-speed run can reach a deep fork (e.g. Xvfb's xkbcomp helper)
    // and show where its adopt hangs. The sum-checks below stay GEX_DEBUG_EXIT-
    // only, and their sums are only computed then (fork side), so this path
    // computes none.
    const step = globalThis.GEX_DEBUG_EXIT || globalThis.GEX_ADOPT_TRACE
      ? (s) => {
          const now = performance.now();
          console.error(
            `[gex] adopt ${child.pid}: ${s} (+${Math.round(now - last)}ms, `
            + `${Math.round(now - began)}ms in)`,
          );
          last = now;
        }
      : () => {};
    step('copying the image');
    // Grow to cover the parent's mapped regions before writing a byte, or the
    // copy throws `RangeError: offset is out of bounds` above the initial size.
    this.#growTo(child.image.reduce((n, r) => Math.max(n, r.at + r.bytes.byteLength), 0));
    const u8 = this.#bytes();
    for (const r of child.image) u8.set(new Uint8Array(r.bytes), r.at);
    // Checked here, before anything else writes to this memory: the check
    // further down runs after the child has rewritten the low region itself,
    // so it cannot tell a damaged copy from a deliberate rewrite. This one can.
    if (globalThis.GEX_DEBUG_EXIT && child.image?.[0]?.sum !== undefined) {
      let bad = 0;
      for (const r of child.image) {
        const end = r.at + r.bytes.byteLength;
        if (sum32(u8.subarray(r.at, end)) === r.sum) continue;
        bad++;
        console.error(
          `[adopt] on arrival, region ${r.at.toString(16)}..${end.toString(16)} `
            + `(${r.prot ?? '-'}) is not what the parent sent`,
        );
      }
      step(bad ? `${bad} regions arrived wrong` : 'image arrived intact');
    }
    // How much of the context survives each step, to find which step empties it
    // when a child enters at zero.
    const live = () => {
      let n = 0;
      const at = Number(child.ctx);
      for (let i = at; i < at + 512; i++) if (u8[i]) n++;
      return n;
    };
    step(`image copied, ${live()} non-zero bytes in the context`);
    await this.attach(shimWasm, guestWasm);
    step(`instantiated, ${live()} non-zero bytes in the context`);
    const e = this.#shim.exports;
    // Before the kernel is built, not after: the metadata is what the child's
    // dispatch table starts from, and `gex_adopt_child` reads it as it builds.
    this.#setMetadata(guestWasm);
    // A deferred child must not claim its parent's slots: the inherited
    // directory names one per page the parent translated, none of which this
    // child has filled. Rebuilt rather than erased, keeping the guest module's
    // own passive-segment entries (the copied memory marks them applied, so
    // they are never rewritten). Not conditional on there being modules -- a
    // child handed none needs it more. See `gex_reset_directory`.
    if (adoptAfterTraps() > 0 && e.gex_reset_directory) {
      step(`directory rebuilt: ${e.gex_reset_directory() >>> 0} entries kept`);
    }
    // Before the kernel is built: `Kernel::new` fixes the identity at
    // construction, so a uid set afterwards leaves every child running as the
    // default. `gex_adopt_child` reads it as it builds.
    {
      const o0 = child.opts ?? {};
      const uid = o0.uid ?? child.uid;
      if (uid !== undefined) e.gex_set_identity(uid, o0.gid ?? child.gid ?? uid);
    }
    // The parent's working directory, so a child inherits it as it does on
    // Linux. `gex_adopt_child` reads the CWD global as it builds its kernel,
    // so this has to be set first -- same ordering as the identity above.
    if (child.cwd && e.gex_set_cwd) {
      const c = new TextEncoder().encode(child.cwd);
      const cp = e.gex_alloc(c.length) >>> 0;
      this.#bytes().set(c, cp);
      e.gex_set_cwd(cp, c.length);
      e.gex_free?.(cp, c.length);
    }
    const entry = e.gex_adopt_child(
      child.pid,
      child.ctx,
      child.pc,
      child.sp,
      child.tls,
      child.brk ?? 0n,
    );
    step(`kernel built, entry 0x${entry.toString(16)}, ${live()} non-zero bytes in the context`);
    // The kernel exists now, so everything that configures one can be said.
    const o = child.opts ?? {};
    this.#opts = o;
    if (o.strace) e.gex_set_trace(1);
    if (o.perMiss) e.gex_set_per_miss?.(o.perMiss);
    if (o.tty) e.gex_set_tty(1);
    // Identity was already said above, before the kernel was built.
    // The map, which the bytes do not carry: without it the child's first
    // `mmap` hands out an address its own image is already at.
    // Preferring the blob, which carries each region's name and the file its
    // code came from as well as its span. The three-number form stays for a
    // shim built before the blob existed.
    const blob = child.vmaBlob;
    if (blob && e.gex_adopt_vma_blob) {
      const p = e.gex_alloc(blob.length) >>> 0;
      new Uint8Array(this.#memory.buffer, p, blob.length).set(blob);
      e.gex_adopt_vma_blob(p, blob.length);
      e.gex_free?.(p, blob.length);
    } else {
      for (const r of child.image) {
        if (r.end !== undefined) e.gex_adopt_vma(BigInt(r.at), BigInt(r.end), r.prot);
      }
    }
    step(`${child.image.length} regions mapped`);
    // Before the modules, because installing one asks the shim which slot
    // each of its blocks belongs in and the answer is in this map.
    if (child.pages?.length && e.gex_adopt_page) {
      const dv = new DataView(child.pages.buffer, child.pages.byteOffset, child.pages.byteLength);
      for (let i = 0; i + 12 <= child.pages.byteLength; i += 12) {
        e.gex_adopt_page(dv.getBigUint64(i, true), dv.getUint32(i + 8, true));
      }
    }
    // And the identity map, what is left of all this once the child execs.
    // Nothing reads it until a translation misses, so seeding early is free.
    if (child.shared?.length && e.gex_shared_seed) {
      const n = (child.shared.byteLength / 20) | 0;
      const p = e.gex_alloc(n * 20) >>> 0;
      this.#bytes().set(child.shared.subarray(0, n * 20), p);
      e.gex_shared_seed(p, n);
      e.gex_free?.(p, n * 20);
    }
    step('pages adopted');
    // What this child's isolate is holding, for the OOM in #145: measured
    // rather than guessed. `measureUserAgentSpecificMemory` needs cross-origin
    // isolation (already on for `SharedArrayBuffer`) and reports a breakdown by
    // type. Fired from the child right after adopt, awaited in the background
    // since the measurement waits for a GC.
    // One line per forked child, so a hundred-process `apt-get update` prints a
    // hundred: narration for the #145 OOM, behind the same `debugexit` flag as
    // the rest of the fork story, so an ordinary run does not fail a harness
    // counting `console.error`.
    if (globalThis.GEX_DEBUG_EXIT) {
      console.error(
        `[heap] debug=${!!globalThis.GEX_DEBUG_EXIT}`
          + ` api=${typeof performance.measureUserAgentSpecificMemory}`
          + ` isolated=${globalThis.crossOriginIsolated}`
          // Standard API absent in this Worker even when isolated, so fall back
          // to Chrome's own coarse counter.
          + ` used=${performance.memory?.usedJSHeapSize ?? 'n/a'}`
          + ` limit=${performance.memory?.jsHeapSizeLimit ?? 'n/a'}`
          + ` image=${(child.image ?? []).reduce((n, r) => n + (r.bytes?.byteLength ?? 0), 0)}`
          + ` modules=${(child.modules ?? []).length}`,
      );
    }
    if (performance.measureUserAgentSpecificMemory) {
      const imageBytes = (child.image ?? []).reduce(
        (n, r) => n + (r.bytes?.byteLength ?? 0), 0);
      performance
        .measureUserAgentSpecificMemory()
        .then((m) => {
          const by = {};
          for (const b of m.breakdown ?? []) {
            if (!b.bytes) continue;
            const k = (b.types ?? []).join('+') || 'other';
            by[k] = (by[k] ?? 0) + b.bytes;
          }
          const parts = Object.entries(by)
            .sort((a, b) => b[1] - a[1])
            .slice(0, 6)
            .map(([k, v]) => `${k}=${v}`)
            .join(' ');
          console.error(
            `[heap] pid ${this.#env.pid ?? '?'}: total ${m.bytes}`
              + ` image ${imageBytes} modules ${(child.modules ?? []).length}`
              + ` | ${parts}`,
          );
        })
        .catch((e) => console.error(`[heap] measure failed: ${e.message}`));
    }
    // After the kernel exists and before the guest runs: the first thing a
    // forked child does is read or write a descriptor it expects to have.
    if (child.fds?.length && e.gex_adopt_fds) {
      const p = e.gex_alloc(child.fds.length) >>> 0;
      this.#bytes().set(child.fds, p);
      e.gex_adopt_fds(p, child.fds.length);
      e.gex_free?.(p, child.fds.length);
    }
    // The closure first: its slots sit below anything the parent translated,
    // and a child's dispatch tables start empty. The directory came with the
    // adopted memory, so without this the child's branches point at unfilled
    // slots.
    this.#closureModules = opts?.closure ?? this.#closureModules;
    this.#wantLibrary = opts?.wantLibrary ?? this.#wantLibrary;
    await this.#installClosure();
    // Reachable, not filled: a child mostly execs and throws the inherited
    // image away, so instantiating every parent module up front was wasted.
    // Growing the tables cannot be skipped -- the inherited directory names
    // slots by number, and a branch past a table's end is a trap not a miss;
    // grown slots hold the miss stub, which fetches code from the shared store.
    //
    // Only as far as this child will actually dispatch through: a deferred
    // child rebuilt the directory (`gex_reset_directory` above), so the
    // parent's slot total is not a bound and growing to it wasted 272ms of
    // stub-filling per process.
    //
    // Growing to the parent's total came from #114, on the theory that a branch
    // was running off the end of a table. It was not: `enter` was failing to
    // mask the table index out of a directory entry, and the tables were never
    // too small. An eager child installs its parent's modules here, each
    // growing the tables to its own `need`, which the maximum below is for.
    let need = 0;
    if (adoptAfterTraps() <= 0) {
      for (const m of child.modules ?? []) need = Math.max(need, m.need);
      need = Math.max(need, (child.slots ?? 0) >>> 0);
    }
    if (need) this.#growTables(need, 'a1806');
    // Kept, not dropped: a child that stays wants them after all, and by then
    // its parent's Worker may be gone. See `#maybeAdopt`.
    this.#inherited = child.modules ?? [];
    // Eager (adoptAfterTraps <= 0): install all inherited modules before the
    // child runs. Kept for comparison against the deferred path, which starts
    // with empty tables and meets the parent's code through the miss path.
    if (adoptAfterTraps() <= 0) {
      step(`installing ${this.#inherited.length} modules`);
      const mine = this.#inherited;
      this.#inherited = [];
      for (const m of mine) await this.#adoptModule(m);
    } else {
      step(`${this.#inherited.length} modules reachable, ${need} slots`);
    }
    // Last of all: everything above writes to this memory, and any of it can
    // land on the parent's bytes, which the guest would meet later elsewhere.
    if (globalThis.GEX_DEBUG_EXIT && child.image?.[0]?.sum !== undefined) {
      const now = this.#bytes();
      let bad = 0;
      for (const r of child.image) {
        const end = r.at + r.bytes.byteLength;
        if (sum32(now.subarray(r.at, end)) === r.sum) continue;
        if (bad++ < 8) {
          console.error(
            `[adopt] region ${r.at.toString(16)}..${end.toString(16)} ` +
              `(${r.prot ?? '-'}) is not what the parent sent`,
          );
        }
      }
      step(bad ? `${bad} of ${child.image.length} regions differ` : 'image verified');
    }
    step('running');
    return entry;
  }

  /**
   * Hand the host any thread `clone` has just made. A thread is not a process:
   * nothing is copied, because the memory, kernel, file table and thread table
   * are already shared. All that crosses is which thread-table entry to run.
   */
  async #drainThread() {
    const e = this.#shim.exports;
    if (!e.gex_thread_pending) return;
    const tid = e.gex_thread_pending();
    if (!tid) return;
    const thread = { tid, ctx: e.gex_thread_ctx(), pc: e.gex_thread_pc(), memory: this.#memory };
    e.gex_thread_taken();
    // A thread needs a memory two Workers can both hold. Posting a non-shared
    // memory does not fail -- the structured clone copies it, and the thread
    // runs on a private image nothing else can see -- so it is refused here.
    if (!(this.#memory?.buffer instanceof SharedArrayBuffer)) {
      throw new Error(
        `the guest started thread ${tid} on a memory that is not shared. ` +
          'A guest thread is a Worker and a Worker needs a SharedArrayBuffer: ' +
          'serve the page with COOP/COEP and load the shared build.',
      );
    }
    if (globalThis.GEX_DEBUG_EXIT) {
      console.error(
        `[gex] thread -> tid ${thread.tid} pc=0x${thread.pc.toString(16)} ` +
          `ctx=0x${thread.ctx.toString(16)}`,
      );
    }
    if (typeof this.#env.spawnThread !== 'function') {
      throw new Error(
        `the guest started thread ${tid} and this environment cannot run one. ` +
          'Give the Env a `spawnThread`, or the thread will never run.',
      );
    }
    await this.#env.spawnThread(thread);
  }

  /**
   * Join a process as one of its threads, and say where to enter. The
   * counterpart of `adoptProcess` and much smaller: a process is rebuilt from a
   * copy, a thread only pointed at what is already there.
   */
  async adoptThread(shimWasm, guestWasm, thread) {
    await this.attach(shimWasm, guestWasm, { devices: false });
    const entry = this.#shim.exports.gex_adopt_thread(thread.tid);
    if (!entry) throw new Error(`this process has no thread ${thread.tid}`);
    // Install every already-translated module into this Worker's own tables
    // before it runs an instruction. The funcref tables are per-thread (a wasm
    // table is not shareable) while the directory naming their slots is shared
    // and already written, so a fresh thread branches into a slot its own table
    // has never filled. Catching up on the next trap raced the branch and
    // deadlocked; doing it here once makes the first run deterministic.
    await this.#catchUp();
    return { entry, ctx: Number(thread.ctx) };
  }

  async #drainFork() {
    const e = this.#shim.exports;
    if (!e.gex_fork_pending) return;
    const pid = e.gex_fork_pending();
    if (!pid) return;
    const child = {
      pid,
      pc: e.gex_fork_pc(),
      ctx: e.gex_fork_ctx(),
      sp: e.gex_fork_sp(),
      tls: e.gex_fork_tls(),
      // Where the parent's heap had reached. Without the number the child's
      // kernel thinks it has no heap and grows one from zero, over its contexts.
      brk: e.gex_fork_brk?.() ?? 0n,
      image: (() => {
        const t0 = performance.now();
        const img = this.#processImage();
        this.#imageMs = performance.now() - t0;
        this.#imageBytes = img.reduce((n, r) => n + (r.bytes?.byteLength ?? 0), 0);
        this.#imageRegions = img.length;
        return img;
      })(),
      // The map, separately from the bytes: names and code identities the child
      // needs to recognise a page as belonging to an already-translated library.
      vmaBlob: this.#vmaBlob(),
      // Every slot this process has claimed -- not the highest any module needed.
      // The directory is written when a page is *claimed*, not when its module
      // installs, so the max over inherited modules misses a page claimed after
      // the last install, and a branch there traps rather than misses. See #114.
      slots: e.gex_jit_slots_total?.() ?? 0,
      pages: this.#pageMap(),
      // Which object each page belongs to -- the only part that outlives the
      // child's first `execve`. See `gex_shared_dump`.
      shared: this.#sharedMap(),
      // What this process *holds*, not just what it has instantiated. Three
      // places a module can be: `#inherited`, `#lazyInherited` (not yet
      // instantiated -- an instantiated one is already in `#compiled`), and
      // `#compiled`. Carrying all of them keeps a grandchild from inheriting a
      // directory whose modules it lacks. `WebAssembly.Module` clones for free.
      modules: forkModules()
        ? [
            ...this.#inherited,
            ...this.#lazyInherited.filter((l) => !l.inst),
            ...this.#compiled,
          ]
        : [],
      opts: this.#opts,
      // The descriptors the child inherits, described rather than cloned: an
      // open file is an `Arc` in the parent's heap the child never sees.
      fds: e.gex_fork_fds_len
        ? this.#bytes().slice(
            e.gex_fork_fds_ptr() >>> 0,
            (e.gex_fork_fds_ptr() >>> 0) + (e.gex_fork_fds_len() >>> 0),
          )
        : null,
      // The parent's working directory, so the child resolves relative paths
      // where the parent stood. Decoded to a string; the child restages it.
      cwd:
        e.gex_fork_cwd_len && e.gex_fork_cwd_len() > 0
          ? new TextDecoder().decode(
              this.#bytes().slice(
                e.gex_fork_cwd_ptr() >>> 0,
                (e.gex_fork_cwd_ptr() >>> 0) + (e.gex_fork_cwd_len() >>> 0),
              ),
            )
          : null,
    };
    child.forkCost = {
      buildMs: Math.round(this.#imageMs),
      bytes: this.#imageBytes,
      regions: this.#imageRegions,
      // Absolute, not `performance.now()`: a Worker and the page have different
      // time origins, so a raw reading across them is skew, not a duration.
      sentAt: performance.timeOrigin + performance.now(),
    };
    // What a fork costs to hand over, per fork and in bytes: this record is the
    // largest thing crossing per fork and is copied three times. See task 145.
    if (globalThis.GEX_DEBUG_EXIT) console.error(
      `[fork] pid ${child.pid} image ${child.forkCost.bytes} bytes in ` +
        `${child.forkCost.regions} regions, built in ${child.forkCost.buildMs}ms, ` +
        // Every child inherits its parent's modules and adds its own, so this
        // list grows down the fork tree. The isolate that OOMs has been alive
        // about four seconds -- a freshly spawned child -- which points here
        // rather than at the forking parent. See task 145.
        `${child.modules.length} modules (${this.#inherited.length} inherited ` +
        `+ ${this.#compiled.length} own)`,
    );
    e.gex_fork_taken();
    if (globalThis.GEX_DEBUG_EXIT) {
      // The record as it leaves, not as the child reports it. A child that
      // enters at zero was either handed a zero or lost it on the way, and
      // those are different bugs in different files.
      console.error(
        `[gex] fork -> pid ${child.pid} pc=0x${child.pc.toString(16)} ` +
          `ctx=0x${child.ctx.toString(16)} sp=0x${child.sp.toString(16)} ` +
          `${child.image.length} regions, ${child.modules.length} modules`,
      );
    }
    if (!this.#env.spawnProcess) {
      throw new Error(
        `the guest forked pid ${pid} and this environment cannot start a process. ` +
          'Give the Env a `spawnProcess`, or the child will never run.',
      );
    }
    await this.#env.spawnProcess(child);
  }

  async #catchUp() {
    const e = this.#shim.exports;
    if (!e.gex_module_count) return;
    // A Worker that has attached but not yet been given a process has no
    // kernel to ask, and asking anyway is a panic rather than a zero. That is
    // the order a forked child arrives in: attach, then adopt.
    if (e.gex_has_host && !e.gex_has_host()) return;
    const have = e.gex_module_count() >>> 0;
    // Every slot the *process* has claimed, not the pending module's -- this
    // Worker is catching up on all of them at once.
    const need = (e.gex_jit_slots_total?.() ?? 0) >>> 0;
    for (; this.#installed < have; this.#installed++) {
      const ptr = e.gex_module_ptr(this.#installed) >>> 0;
      const len = e.gex_module_len(this.#installed) >>> 0;
      if (!len) continue;
      // Copied out before compiling: the registry is in the guest's memory,
      // and another thread allocating can move it.
      await this.#installModule(this.#bytes().slice(ptr, ptr + len), need, 64, null, 'jit2049');
    }
  }

  /** Hand the guest some standard input. Safe to call at any time. */
  pushStdin(bytes) {
    const p = this.#shim.exports.gex_alloc(bytes.length) >>> 0;
    this.#bytes().set(bytes, p);
    this.#shim.exports.gex_push_stdin(p, bytes.length);
    this.#shim.exports.gex_free(p, bytes.length);
  }

  /** Say there will be no more input. */
  endStdin() { this.#shim.exports.gex_stdin_eof(); }

  /**
   * Give the guest a network. `net` has the seven calls in `Net` below. Off by
   * default: a guest with no proxy should see a machine with no network, which
   * fails cleanly, not one whose connections hang.
   */
  useNetwork(net) {
    this.#net = net;
    // Guarded, because this runs before the shim exists (a Worker is given its
    // devices between construction and `adoptProcess`); `load`/`attach` set it
    // from `#net` too.
    this.#shim?.exports?.gex_set_network?.(net ? 1 : 0);
  }

  /**
   * Give the guest a graphics device. `gl` is a `GlDevice` from web/src/gl.js.
   * Off by default, like the network. Best set before `load`, since the device
   * node is made while the filesystem is built; a later call still works.
   */
  useGl(gl) {
    this.#gl = gl;
    this.#shim?.exports?.gex_set_gl?.(gl ? 1 : 0);
  }

  /**
   * Give the guest a Wayland compositor. `wl` is a `WlProxy` (wl-bridge.mjs)
   * that carries committed frames to the page's canvas. Like `useGl`, best set
   * before `load` -- the socket is bound while the filesystem is built.
   */
  useWl(wl) {
    this.#wl = wl;
    this.#shim?.exports?.gex_set_wl?.(wl ? 1 : 0);
  }

  /**
   * Give the guest a virtio-gpu renderer instead: the GPU path that needs no
   * change to the guest (unmodified Debian Mesa, not gex's libGLESv2).
   * `renderer` is a `VirglRenderer` from web/src/virgl.js. Must be set before
   * `load` -- the socket is bound while the namespace is built, and Mesa's
   * vtest winsys blocks rather than falling back when it is missing.
   */
  useVirgl(renderer, trace = false) {
    this.#virgl = renderer;
    this.#virglTrace = trace;
    this.#shim?.exports?.gex_set_virgl?.(renderer ? (trace ? 2 : 1) : 0);
  }

  async run(entry, startCtx = 0x1000) {
    // Every switch, signal and miss leaves wasm by trapping, and V8 captures a
    // stack trace per trap that nothing here reads -- hundreds of thousands of
    // them for a program with contended locks.
    const limit = Error.stackTraceLimit;
    if (!globalThis.__gexKeepTrace) Error.stackTraceLimit = 0;
    try {
      return await this.#runLoop(entry, startCtx);
    } finally {
      Error.stackTraceLimit = limit;
    }
  }

  async #runLoop(entry, startCtx = 0x1000) {
    let pc = entry;
    // Which thread's registers to enter with. The first thread unless a thread
    // in its own Worker says otherwise, or it runs the main thread's registers.
    let ctx = startCtx;
    for (;;) {
      try {
        // Address zero is never a guest instruction: entering there is how a
        // lost resume address shows up, far from whatever forgot to set it.
        if (!pc) {
          throw new Error(
            `gex: nothing said where to resume (ctx=0x${ctx.toString(16)}). ` +
              'A trap gave the stack back without setting a resume address.',
          );
        }
        this.#guest.exports.enter(ctx, pc);
        // `enter` returning means the guest gave the stack back for good,
        // normally because the last process exited.
        if (globalThis.GEX_DEBUG_EXIT) {
          console.error(`[gex] run ended: enter returned, code ${this.#shim.exports.gex_exit_code()}`);
        }
        return this.#shim.exports.gex_exit_code();
      } catch (err) {
        // First, before asking the shim anything: was this a handoff or a
        // crash? Every way out is the same `unreachable` to the engine, and
        // treating a crash as a handoff reported a crashed runtime as a clean
        // exit 0. `trap()` sets a flag; nothing else does. First because the
        // recovery below calls into the shim, whose kernel a crash may leave
        // borrowed -- a second call would bury the first error.
        if (this.#shim.exports.gex_handoff && !this.#shim.exports.gex_handoff()) {
          // Each question in its own try: after a crash the kernel may be
          // borrowed and a second call would trap on top of the first.
          const ask = (f, how) => {
            try {
              return f ? how(f()) : '';
            } catch {
              return '';
            }
          };
          const e = this.#shim.exports;
          const at = ask(e.gex_current_pc, (pc) =>
            `, guest pc=0x${(pc < 0n ? pc + (1n << 64n) : pc).toString(16)}`);
          const code = ask(e.gex_exit_code, (c) => (c ? `, exit code ${c}` : ''));
          const heap = ask(e.gex_heap_used_kib, (k) =>
            k >= 0 ? `, ${(k / 1024) | 0} MiB of shim heap in use` : '');
          // "inside" only when the syscall is still on the stack: a plain
          // static, so it answers even with the kernel borrowed.
          const within = ask(e.gex_in_syscall, (v) => v) === 1;
          // `!0` reaches JS as -1, not 0xffffffff (u32 in Rust, i32 in wasm),
          // so both sentinels are checked.
          const nr = ask(e.gex_last_syscall, (n) =>
            n === -1 || n === 0xffffffff
              ? ''
              : `, ${within ? 'inside' : 'after'} syscall ${n}`);
          if (globalThis.GEX_TRAP_STACK) console.error(err.stack ?? err);
          // Dump every thread's register context: two with the same ctx is #126
          // (two Workers on one context). Prints via the shim; safe if borrowed.
          ask(e.gex_dump_threads, () => '');
          // The target of the last indirect branch, stored by the dispatch
          // itself. `enter` is long gone by now (dispatch ends in
          // `return_call_indirect`, which replaces frames), and the argument it
          // was called with and the saved PC both name where the guest already
          // left -- so a table-index trap can only be pinned from this.
          const target = ask(e.gex_last_target, (v) => v) || 0n;
          const where = ask(() => (target ? target : pc), (entered) => {
            if (!/table index/.test(err.message)) return '';
            const raw = BigInt(entered);
            const guest = raw < 0n ? raw + (1n << 64n) : raw;
            const page = Number(guest >> 12n);
            const dir = new DataView(this.#memory.buffer).getUint32(DISPATCH_DIR + page * 4, true);
            const tables = this.#dispatchTables();
            const packed = dir >>> 0;
            const idx = packed >>> 24;
            const slot = packed & 0xffffff;
            // The page's base plus the block's offset within the page: the
            // base alone showed a slot inside the table while the branch was
            // past its end.
            const at = slot + ((Number(guest) & 0xfff) >> 2);
            const len = tables[idx]?.length ?? 0;
            // Asked, not computed: `get` settles whether the table really is
            // short (RangeError) or the trap belongs to some other operation.
            let probe;
            try {
              probe = tables[idx].get(at) ? 'holds a function' : 'is empty';
            } catch (e) {
              probe = `cannot be read: ${e.message}`;
            }
            // And when that entry is fine, the whole directory: a dispatch is
            // for the branch's target, not the pc, so the trapping entry is
            // usually not the one at `dispatch_page(pc)`. Every entry is checked
            // against the tables; a high-table entry (tables 2-7 are usually
            // length 0) is a corrupt write, a real table past its end was not
            // grown.
            let bad = 0;
            let highTable = 0;
            const sample = [];
            const dv = new DataView(this.#memory.buffer);
            for (let p = 0; p < 0x80000; p++) {
              const v = dv.getUint32(DISPATCH_DIR + p * 4, true) >>> 0;
              if (v === 0) continue;
              const t = v >>> 24;
              const b = v & 0xffffff;
              const cap = tables[t]?.length ?? 0;
              // The whole page, because any block in it may be branched to.
              if (t < tables.length && b + 1024 <= cap) continue;
              bad++;
              if (t >= 2) highTable++;
              if (sample.length < 6) {
                sample.push(`page ${p} -> ${t}[${b}] (table is ${cap})`);
              }
            }
            const survey = bad
              ? `. ${bad} directory entries name a slot this process cannot ` +
                `reach, ${highTable} of them in a table above 1: ${sample.join('; ')}`
              : '. every other directory entry is reachable';
            return (
              `. the last indirect branch was to 0x${guest.toString(16)}` +
              `${target ? '' : ' (no target recorded, so this is where enter was called)'}` +
              `, page ${page} of the directory says table ${idx} slot ${slot}` +
              `, so this branch wants ${idx}[${at}] and that table is ${len} long` +
              ` (${at < len ? 'in range' : 'past the end'}, and reading it says it ${probe})` +
              survey +
              `, and this process has ${tables.length} tables of ` +
              `[${tables.map((t) => t.length).join(', ')}]` +
              `, ${this.#installs} modules installed`
            );
          });
          throw new Error(
            `the gex runtime trapped inside the shim (${err.message}${at}${nr}` +
              `${code}${heap}${where}). That is a crash in the runtime, not a guest exit; ` +
              'anything printed above is all the guest managed to do first.',
          );
        }
        // A trap abandons the engine's frames without running the epilogues
        // that restore the shim's shadow-stack global, leaking a few dozen
        // bytes each. Nothing is live below here, so reset it exactly -- left
        // alone it slid into the guest's stack and overwrote saved registers.
        this.#shim.exports.__stack_pointer.value = this.#workerStack();
        const e = this.#shim.exports;
        // A slot the shim wants written before the guest goes back to it: its
        // module is instantiated with passive segments, so the code is present
        // and simply not in the table. Filling here costs one segment; at
        // instantiation it cost every block of the object, in every process.
        const want = e.gex_fill_wanted ? Number(e.gex_fill_wanted()) : -1;
        if (want >= 0) {
          // Whether anybody answered matters: a page nobody holds yet must be
          // askable again, a page nobody ever will must stop trapping.
          const q0 = performance.now();
          const quick = this.fillSlotNow(want >>> 0);
          this.#nowFills++;
          this.#nowMs += performance.now() - q0;
          let answer = quick;
          if (answer === null || answer === undefined) {
            const t0 = performance.now();
            answer = await this.fillSlot(want >>> 0);
            this.#slowFills++;
            this.#slowMs += performance.now() - t0;
          }
          e.gex_fill_done(answer ? 1 : 0);
        }
        // One grow for the whole queue before any install: `#installModule`
        // grows per module and `Table.grow` reallocates, so a batch of hundreds
        // with a rising need copies an ever-larger table hundreds of times.
        if (e.gex_jit_pending_len() >>> 0) {
          const need = (e.gex_jit_pending_slots?.() ?? 0) >>> 0;
          if (need) this.#growTables(need, 'a2358');
        }
        while (e.gex_jit_pending_len() >>> 0) await this.#installTranslation();
        // The parent's modules go in after this Worker's own queue, never
        // between a translation and its install: a module's page assignments
        // are the next one's starting point, so interleaving two orderings
        // corrupts the assignments.
        await this.#maybeAdopt();
        // And anything another Worker of this process translated: the pending
        // queue is one queue for the process, so whichever Worker traps next
        // drains it into its own tables and the asker never sees it. `#catchUp`
        // is a no-op comparison when there is nothing to do.
        this.#armCensus();
        await this.#catchUp();
        // A `fork` leaves a child here for the host to start. Drained before
        // the parent runs on, so the two are only ever one process apart --
        // and before the memory is touched again, because what the child gets
        // is a copy of it as `fork` left it.
        await this.#drainFork();
        await this.#drainThread();
        // A process that has exited is not resumed, whatever the shim left in
        // the resume slot. The shim clears it in `exit_process` now, and this
        // is the same rule stated on the side that acts on it: re-entering a
        // guest whose descriptors have been released produces failures that
        // read as bugs anywhere but here.
        const next = e.gex_exited?.() ? 0n : e.gex_resume_pc();
        if (next !== 0n) {
          pc = next;
          const nextCtx = e.gex_resume_ctx() >>> 0;
          if (nextCtx !== 0) ctx = nextCtx;
          continue;
        }
        // The guest gave the stack back unfinished: every thread waits on
        // something only the page can produce. Waiting happens out here because
        // a guest holding the one wasm stack is a guest nothing can reach. The
        // loop matters: one event-loop turn is no promise the wait is over.
        if (e.gex_paused_for_net() || e.gex_paused_for_input()) {
          let woke = 0;
          while (e.gex_paused_for_net() || e.gex_paused_for_input()) {
            // Input first, and the two are not exclusive: a guest can wait on
            // the keyboard and a socket at once.
            if (e.gex_paused_for_input()) {
              // A keystroke is an indefinite wait, but when a clock or socket
              // waits too it must end on its own so that thread can run. Losing
              // the race is free -- a late key is still delivered.
              const key = this.#env.readStdin();
              const more = e.gex_paused_for_net()
                ? await Promise.race([key, new Promise((r) => setTimeout(() => r(true), 5))])
                : await key;
              if (more === false) {
                this.endStdin();
                // An environment with no input answers instantly, so one tick
                // keeps this loop from spinning.
                await new Promise((r) => setTimeout(r, 1));
              }
            } else {
              // `gex_park` sleeps this Worker in the global kernel's memory
              // until another Worker bumps the wake counter (every syscall
              // does) or the bound passes -- a wake, not a poll. The bound
              // keeps it answerable: messages the host posts are queued while
              // asleep and the turn below runs that queue. -1 is "no global
              // kernel to sleep on", and a network wait cannot sleep at all --
              // its bytes arrive on this Worker's own event loop.
              if (e.gex_paused_for_host() || e.gex_park(20) < 0) {
                // A network wait runs on this Worker's event loop, so it cannot
                // sleep -- but it needs a turn, not a timer: a MessageChannel
                // turn returns to the loop in microseconds where a nested
                // `setTimeout(_, 1)` is clamped to 4ms, which was the whole
                // speed of a download. `gex_paused_for_host` is the network
                // case, so it is the condition for turning.
                const onLoop = !!e.gex_paused_for_host();
                this.#netTurns = onLoop ? this.#netTurns + 1 : 0;
                // A nap every 1024th turn keeps an empty socket from spinning
                // the Worker hot and lets timer-driven work run. Raising it
                // from 64 did not change throughput -- the naps overlap with
                // waiting for bytes rather than serialising it.
                if (onLoop && (this.#netTurns & 1023) !== 0) {
                  this.waits.turn++;
                  await this.#turn();
                } else {
                  this.waits.nap++;
                  // See `#nap` for why this is not a bare `setTimeout`.
                  await this.#nap();
                }
              } else {
                this.waits.park++;
                await this.#turn();
              }
            }
            woke = e.gex_wake();
            if (woke === 1) break;
          }
          if (woke !== 1) {
            // Every thread is waiting and nothing woke them. Two cases, told
            // apart by whether the guest actually exited: a process whose last
            // thread `exit`ed leaves its Worker here with nothing to wake, which
            // is the ordinary end of every forked child -- it reports the guest's
            // own code and stays quiet. A give-up (nothing exited, nothing woke)
            // is the runtime stuck; that is 125 -- outside a program's range so
            // it is not mistaken for a clean exit -- and it is said out loud.
            const stalled = e.gex_exited ? !e.gex_exited() : false;
            if (stalled) {
              const why =
                `[gex] the run stopped and nothing could wake it ` +
                `(wake=${woke}, exited=${!stalled})`;
              this.#env.write(2, new TextEncoder().encode(`${why}\n`));
              console.error(why);
              return 125;
            }
            return e.gex_exit_code();
          }
          pc = e.gex_resume_pc();
          const wakeCtx = e.gex_resume_ctx() >>> 0;
          if (wakeCtx !== 0) ctx = wakeCtx;
          continue;
        }
        // Anything this far is an unexpected trap whose message ("memory access
        // out of bounds") says nothing about where. So attach the guest pc and
        // which library it is in.
        if (e.gex_current_pc && !String(err).includes('unreachable')) {
          const pc = e.gex_current_pc();
          let where = '';
          if (e.gex_object_at) {
            const at = e.gex_alloc(160) >>> 0;
            const n = e.gex_object_at(pc, at, 160) >>> 0;
            if (n) where = ' in ' + utf8(this.#bytes().subarray(at, at + n));
          }
          const ctx = e.gex_current_ctx ? e.gex_current_ctx() : 0n;
          // And how full the shim's heap was: it is a fixed region that cannot
          // grow, so running out surfaces only as a fault somewhere in the
          // guest. Present only with `--features heap-counters`.
          const used = e.gex_heap_used_kib?.() ?? -1;
          const total = e.gex_heap_total_kib?.() ?? -1;
          const heap =
            used >= 0 && total > 0
              ? `, heap ${(used / 1024).toFixed(0)}/${(total / 1024).toFixed(0)} MiB`
              : '';
          // And where dispatch thought that address lived: for "table index is
          // out of bounds", the slot wanted and how long that table is, which
          // the engine's message omits.
          let dispatch = '';
          if (e.gex_dispatch_slot) {
            const packed = e.gex_dispatch_slot(pc);
            const tables = this.#dispatchTables();
            const lens = tables.map((t) => t.length).join(',');
            dispatch =
              packed < 0
                ? `, no slots for this page, tables ${lens}`
                : `, table ${packed >>> 24} slot ${packed & 0xffffff}, tables ${lens}`;
          }
          err.message +=
            ` (guest pc=0x${(pc < 0n ? pc + (1n << 64n) : pc).toString(16)}${where}` +
            `, ctx=0x${ctx.toString(16)}${heap}${dispatch})`;
        }
        if (String(err).includes("unreachable")) {
          // The ordinary end: the shim set an exit status and abandoned the
          // stack. `enter` never returns then, so this is the path a finished
          // program takes.
          if (globalThis.GEX_DEBUG_EXIT) {
            // With where it was, because a crash with no status set looks
            // identical to a clean finish otherwise.
            const say = (fn, f) => {
              try {
                return fn ? f(fn()) : '';
              } catch {
                return '';
              }
            };
            const where =
              say(e.gex_current_pc, (pc) => `, pc=0x${(pc < 0n ? pc + (1n << 64n) : pc).toString(16)}`) +
              say(e.gex_last_syscall, (n) => (n === 0xffffffff ? '' : `, in syscall ${n}`));
            // How much of this process's memory the shim used, which decides
            // how many processes a browser can hold.
            const heap = say(e.gex_heap_used_kib, (k) =>
              k >= 0 ? `, shim heap ${(k / 1024).toFixed(1)} MiB` : '');
            console.error(
              // `exited` or `ABANDONED`: both a finished program and a crashed
              // shim produce `unreachable` and code 0; `gex_exited` tells them
              // apart.
              `[gex] run ended: ${e.gex_exited?.() ? 'exited' : 'ABANDONED'}`
              + `, code ${e.gex_exit_code()}${where}${heap}`,
            );
            // What the global kernel has to say: it has no console, so its panic
            // hook writes the message into its own memory. `worker.mjs` reads it
            // only when the error propagates, which an abandoned stack does not.
            if (!e.gex_exited?.()) {
              try {
                const k = this.#kernel;
                const n = k?.k_panic_len?.() >>> 0;
                if (n) {
                  const at = k.k_panic_ptr() >>> 0;
                  const mem = this.#kernelSpec.memory;
                  console.error(
                    '[gex] the global kernel panicked: '
                    + utf8(new Uint8Array(mem.buffer, at, n)),
                  );
                }
              } catch {
                // A crash report must not crash.
              }
            }
            // And the file it was asking for, when it died inside `openat`.
            if (!e.gex_exited?.() && e.gex_last_open && e.gex_alloc) {
              try {
                const max = 256;
                const p = e.gex_alloc(max) >>> 0;
                const n = e.gex_last_open(p, max) >>> 0;
                const path = utf8(this.#bytes().subarray(p, p + n));
                e.gex_free?.(p, max);
                if (path) console.error(`[gex] the last path it opened: ${path}`);
              } catch {
                // A crash report must not crash.
              }
            }
            // And the wasm stack, when nobody set a status: a finished program's
            // says nothing, but a faulted shim's names the function that did it.
            if (!e.gex_exited?.()) {
              console.error(`[gex] the stack under it:\n${err.stack ?? String(err)}`);
            }
          }
          return e.gex_exit_code();
        }
        throw err;
      }
    }
  }

  /**
   * Make every dispatch table long enough to hold `need` slots, and return
   * them. Separate from installing a module: a forked child must *reach* every
   * slot its parent's directory names (or a branch traps rather than missing)
   * without instantiating the modules that fill them. An unfilled slot holds
   * the miss stub, which is what makes growing without filling safe.
   */
  #growTables(need, who = '?') {
    this.#tnAt(`grow-${who}`);
    const e = this.#shim.exports;
    const tables = this.#dispatchTables();
    // The shim reports a running total across one slot space that spans every
    // table; each table's share is that total clamped to its window. Finite,
    // not `Infinity`: the window arithmetic multiplies this by the table index
    // and `0 * Infinity` is `NaN`. A pre-export shim has one table, so any
    // unreachable number is right. See #64.
    const per = (e.gex_slots_per_table?.() ?? 0) >>> 0 || Number.MAX_SAFE_INTEGER;
    // Which table the module's highest slot lands in, and how far into it.
    // Slot indices are global (table `slot / per`, offset `slot % per`), so a
    // slot in table 4 does not require tables 0-3 to be full.
    const top = need > 0 ? need - 1 : -1;
    const topTable = top >= 0 ? Math.floor(top / per) : -1;
    const topWants = top >= 0 ? (top % per) + 1 : 0;
    // The shim's per-table high-water *and* what the module reaches -- neither
    // alone is enough. `gex_table_need` counts only slots this shim allocated,
    // so a module handed in from JS (closure or inherited) is invisible to it;
    // the running-total fallback would instead grow lower tables to their max.
    const need_of = e.gex_table_need
      ? (i) => Math.max(e.gex_table_need(i) >>> 0, i === topTable ? topWants : 0)
      : (i) => Math.min(Math.max(need - i * per, 0), per);
    // Slot 0 of table 0 is the miss stub, what every unfilled slot must hold so
    // a branch to untranslated code reaches the host. Tables past the first
    // start empty, with no element 0 of their own.
    const stub = tables[0].get(0);
    for (let i = 0; i < tables.length; i++) {
      const need_i = need_of(i);
      if (tables[i].length >= need_i) continue;
      // Geometrically, not exactly: `Table.grow` reallocates and copies, so
      // growing by exactly what each module wants copies an ever-larger table
      // once per module. Doubling makes total copying linear in the final size.
      const want = Math.min(Math.max(need_i, tables[i].length * 2), per);
      this.#iGrows++;
      this.#iAdded += want - tables[i].length;
      this.#iCopied += want;
      // Big grows tallied and shipped with the fill stats -- a Worker's console
      // does not reach the page.
      if (want - tables[i].length > 100000) {
        this.#bigGrows.push(
          `${who}:t${i}:${tables[i].length}->${want}:need${need}`
            + `:ni${need_i}:tn${e.gex_table_need ? e.gex_table_need(i) >>> 0 : -1}`,
        );
      }
      try {
        tables[i].grow(want - tables[i].length, stub);
      } catch (err) {
        throw new Error(
          `dispatch table ${i} would not grow from ${tables[i].length} to ${want}: ${err.message}`,
        );
      }
    }
    return tables;
  }

  /**
   * Take the parent's modules after all, if this child turns out to be staying.
   * A child arrives with its tables grown but nothing filled, so one that execs
   * (almost all of them) pays nothing. One that keeps running its parent's code
   * meets it a miss at a time; past a few dozen misses it is plainly not about
   * to exec, and adopting the modules in one go is cheaper than faulting on.
   */
  async #maybeAdopt() {
    if (!this.#inherited.length) return;
    // An exec makes the parent's modules worthless -- their directory would
    // point at code belonging to a program that is gone -- so let go of them.
    // A generation, compared: two execs between checks still differ.
    const gen = this.#shim.exports.gex_exec_gen?.() ?? 0;
    if (gen !== this.#execGen) {
      this.#execGen = gen;
      this.#inherited = [];
      return;
    }
    if (++this.#traps < adoptAfterTraps()) return;
    const mine = this.#inherited;
    this.#inherited = [];
    for (const m of mine) await this.#adoptModule(m);
  }

  /**
   * Instantiate the module a dispatch miss left behind, and put its blocks in
   * the table the guest dispatches through. The shim lifted and encoded it;
   * only the engine can compile it, and only JS can ask the engine.
   */
  async #installTranslation() {
    const e = this.#shim.exports;
    const ptr = e.gex_jit_pending_ptr() >>> 0;
    const len = e.gex_jit_pending_len() >>> 0;
    await this.#installModule(this.#bytes().slice(ptr, ptr + len), e.gex_jit_slots() >>> 0, 64, null, 'jit2878');
    e.gex_jit_installed();
    // This Worker translated it, so the entry `gex_jit_installed` just added is
    // the newest here and must not be replayed on top of itself.
    this.#installed = e.gex_module_count ? e.gex_module_count() >>> 0 : this.#installed;
  }

  /**
   * Put one translated module's blocks into this Worker's dispatch tables.
   * One function for the two ways a module arrives (this Worker translated it,
   * or another did and this one catches up): a thread that installed them
   * differently would dispatch to different code.
   */
  async #installModule(bytes, need, segments = 64, meta = null, who = '?') {
    const t0 = performance.now();
    this.#installs++;
    try {
      return await this.#installModuleInner(bytes, need, segments, meta, who);
    } finally {
      this.#installMs += performance.now() - t0;
    }
  }

  /**
   * The install itself, timed by the wrapper above -- because the counter the
   * project steers by (`millis translating`) measures only lifting, which
   * turned out not to be what a run costs: each process installs thousands of
   * shared modules, and compiling and instantiating those was uncounted.
   */
  async #installModuleInner(bytes, need, segments = 64, meta = null, who = '?') {
    const t0 = performance.now();
    const e = this.#shim.exports;
    const tg = performance.now();
    const tables = this.#growTables(need, 'install');
    this.#growMs += performance.now() - tg;
    this.#tnAt('pre-inst');
    const tc = performance.now();
    const mod = bytes instanceof WebAssembly.Module ? bytes : await WebAssembly.compile(bytes);
    this.#compileMs += performance.now() - tc;
    // With its metadata: a fork hands the child `#compiled`, and without the
    // metadata the child cannot tell which module covers which page and must
    // instantiate all of them. The section is in the bytes; keeping it is a
    // parse.
    this.#compiled.push({
      mod,
      need,
      meta: bytes instanceof Uint8Array ? metadataSection(bytes) : undefined,
    });
    const imports = { ...this.#imports };
    for (let i = 0; i < tables.length; i++) imports[TABLE_NAMES[i]] = tables[i];
    // What this module's own entries need per table, where known. `need` alone
    // is one running total that cannot say which tables absolute segments fall
    // in; the segments (or `Metadata` for an already-compiled module) can, which
    // avoids the strict-grow retry below.
    const own = meta ?? (bytes instanceof Uint8Array ? metadataSection(bytes) : null);
    const seg = elementTableNeeds(bytes);
    const needs = seg.length ? seg : own ? metaTableNeeds(own) : [];
    for (let t = 0; t < needs.length; t++) if (needs[t]) this.#reach(t, needs[t]);
    let inst;
    const ti = performance.now();
    try {
      // Synchronously: `mod` is already compiled, so this is legal and does not
      // yield -- an `await` here goes to the back of the Worker's task queue.
      inst = new WebAssembly.Instance(mod, { gex: imports });
      this.#instantiateMs += performance.now() - ti;
    } catch (first) {
      // Grow to everything the module could need and retry once. `need` read
      // loosely (only the table the total lands in) is right for a module whose
      // segments live in one table, nearly all of them; the strict reading
      // (lower tables full) is expensive, so pay it only when the engine says
      // "table index is out of bounds".
      const per = (e.gex_slots_per_table?.() ?? 0) >>> 0 || Number.MAX_SAFE_INTEGER;
      const stub = tables[0].get(0);
      let grew = 0;
      for (let i = 0; i < tables.length; i++) {
        const want = Math.min(Math.max(need - i * per, 0), per);
        if (tables[i].length < want) {
          grew += want - tables[i].length;
          tables[i].grow(want - tables[i].length, stub);
        }
      }
      try {
        inst = new WebAssembly.Instance(mod, { gex: imports });
        this.#instantiateMs += performance.now() - ti;
        this.#strictGrows++;
        this.#strictSlots += grew;
        this.#bigGrows.push(`RETRY:${who}:need${need}:own${own ? own.length : -1}:needs[${needs.join(',')}]:+${grew}`);
      } catch (err) {
        throw new Error(
          `installing a module of ${need} slots failed: ${err.message}. ` +
            `tables are [${tables.map((t) => t.length).join(', ')}], ` +
            `${(e.gex_slots_per_table?.() ?? 0) >>> 0} slots per table, ` +
            `${this.#installs} modules in so far, ` +
            `${grew} slots grown on retry after: ${first.message}`,
        );
      }
    }
    // Write the module's blocks into the tables. Its element segments are
    // passive, so instantiating wrote nothing -- deliberate, because a module
    // is instantiated once per process and an active segment would make every
    // process write a slot for every block it holds (1.2-2.2s a process for a
    // closure library, mostly code never reached). Filling here moves that work
    // under the host's control, so a segment can be filled only when a branch
    // reaches its page. `gexfill` is absent from a pre-fill module and the image.
    if (globalThis.GEX_DEBUG_EXIT && this.#installs <= 2) {
      console.error(`[fill] module ${this.#installs}: gexfill ${typeof inst.exports[FILL]}, exports ${Object.keys(inst.exports).slice(0, 4).join(',')}`);
    }
    // Written now, or when a branch asks. The module decides: only one built
    // passive exports `gexfillat` and is deferred. A runtime unit has active
    // segments and is filled here -- it exists because a branch missed on it,
    // so deferring would only guarantee a second miss.
    if (typeof inst.exports[FILL_AT] === 'function') {
      this.#unfilled.unshift(inst);
      // Tell the shim there is now something worth asking for: until a module
      // is deferred, the miss path must not spend a trap per page on a host
      // with nothing to give.
      e.gex_fill_possible?.(1);
    } else if (typeof inst.exports[FILL_ALL] === 'function') {
      // One crossing, not one per segment -- a module has up to 355.
      inst.exports[FILL_ALL]();
    } else if (typeof inst.exports[FILL] === 'function') {
      // Until it says there are no more: the count is the module's own (1 for
      // the image, 355 for libc), and guessing left segments unwritten.
      for (let seg = 0; inst.exports[FILL](seg); seg++);
    }
    // This loop writes nothing for a module this translator built: it exports
    // only `enter` and fills its blocks through its own element segments. Kept
    // for modules of the older shape, which name each block.
    for (const name of Object.keys(inst.exports)) {
      if (name[0] !== 'b') continue;
      // The shim packs the table index into the top eight bits, like a
      // directory entry.
      const packed = e.gex_dispatch_slot(BigInt('0x' + name.slice(1)));
      if (packed < 0) continue;
      tables[packed >>> 24].set(packed & 0xffffff, inst.exports[name]);
    }
    // The instance, so the caller can remember which library it belongs to.
    return inst;
  }

  /**
   * Let an already-instantiated module answer fills. `#installModule` does this
   * for the modules it installs; the image module does not go through it (it is
   * the guest module itself, instantiated by `load`), so without this its
   * passive segments are never written and its pages stay on the miss stub.
   */
  #offerToFill(inst) {
    if (typeof inst?.exports?.[FILL_AT] !== 'function') return;
    this.#unfilled.unshift(inst);
    this.#shim.exports.gex_fill_possible?.(1);
  }

  /**
   * Put a registered closure library into the directory, and grow for it. Doing
   * this per library at registration cost each process the whole closure's
   * slots; here it costs a process only the libraries it enters.
   */
  #publish(lazy, i) {
    this.#tnAt('pre-pub');
    const e = this.#shim.exports;
    if (!lazy.meta || !lazy.meta.length || !e.gex_publish_closure_module) return;
    const t0 = performance.now();
    const gp = e.gex_alloc(lazy.meta.length) >>> 0;
    this.#bytes().set(lazy.meta, gp);
    e.gex_publish_closure_module(gp, lazy.meta.length, i);
    e.gex_free?.(gp, lazy.meta.length);
    // No grow here: a library's pages reach the table one at a time as
    // `#pageBase` claims them. Counted unconditionally because publishing is a
    // real per-process cost that no other counter accounts for (#125).
    this.#publishes++;
    this.#publishMs += performance.now() - t0;
    if (globalThis.GEX_DEBUG_EXIT) {
      console.error(
        `[closure] library ${i} published in ${Math.round(performance.now() - t0)}ms`,
      );
    }
  }

  /**
   * Where this process's dispatch directory says a page's slots live.
   * `gexfillat` is told this rather than using the slot numbers baked in at
   * build time, which are one sequence across the whole closure. The directory
   * is per process, so the numbering can be too.
   */
  #pageBase(page) {
    const e = this.#shim.exports;
    if (!e.gex_claim_page || !e.gex_page_table) return -1;
    const t = e.gex_page_table(BigInt(page >>> 0));
    if (t < 0) return 0;
    // The shim numbers the page from a PROCESS-WIDE atomic allocator and writes
    // the directory with a compare-and-swap, so every thread of this process
    // agrees on the base (a per-Worker counter here used to let two threads
    // number one page differently and dispatch it through the wrong slot -- the
    // multithreaded-GTK stack smash). It returns the base, whether this thread
    // won the claim or adopted another's. The table is per instance, so reach
    // this Worker's to cover the base or a branch traps past its end.
    const base = e.gex_claim_page(BigInt(page >>> 0)) >>> 0;
    if (base === 0) return 0;
    this.#reach(t, base + SLOTS_PER_PAGE);
    return base >>> 0;
  }

  /**
   * Make dispatch table `t` at least `end` slots long. In fixed chunks, not
   * doubling: the watermark advances a page at a time, so doubling overshoots
   * enormously. A chunk amortises the reallocation over sixteen pages.
   */
  #reach(t, end) {
    const e = this.#shim.exports;
    const tables = this.#dispatchTables();
    if (t < 0 || t >= tables.length || tables[t].length >= end) return;
    const per = (e.gex_slots_per_table?.() ?? 0) >>> 0 || Number.MAX_SAFE_INTEGER;
    const CHUNK = SLOTS_PER_PAGE * 16;
    const want = Math.min(Math.ceil(end / CHUNK) * CHUNK, per);
    const stub = tables[0].get(0);
    const delta = want - tables[t].length;
    if (delta <= 0) return;
    const g0 = performance.now();
    tables[t].grow(delta, stub);
    this.#pubGrewSlots += delta;
    this.#pubGrowMs += performance.now() - g0;
  }

  /**
   * Where in the page the fill should start, in slots -- recorded by the shim
   * from the pc that missed, rounded down to a multiple of `FILL_SPAN`. A stale
   * shim answers zero, which fills from the start of the page: wrong, not unsafe.
   */
  #fillFrom() {
    return this.#shim.exports.gex_fill_from ? this.#shim.exports.gex_fill_from() >>> 0 : 0;
  }

  /**
   * For each address that translated although its span had been filled, whether
   * its dispatch slot is written (#136 diagnostic). A written slot means
   * dispatch reads elsewhere than the fill wrote; a stub means the module holds
   * no block there. The shim collects the addresses; the table is read here.
   */
  liftSlots() {
    const e = this.#shim.exports;
    if (!e.gex_lift_pc || !e.gex_page_base) return undefined;
    const out = [];
    const tabs = this.#dispatchTables();
    if (!tabs.length) return undefined;
    const stub = tabs[0].get(0);
    const seen = new Set();
    for (let i = 0; i < 16; i++) {
      const pc = Number(e.gex_lift_pc(i));
      if (!pc) break;
      const page = Math.floor(pc / 4096);
      // The base the shim recorded at lift, not the one this page has now: a
      // forked child lifts its parent's code then execs a different program.
      const then = e.gex_lift_base ? e.gex_lift_base(i) >>> 0 : 0;
      const base = then || (e.gex_page_base(page >>> 0) >>> 0);
      const tn = e.gex_page_table ? e.gex_page_table(BigInt(page >>> 0)) : -1;
      let what;
      if (!base) {
        what = 'no base';
        // And why there is none: which images claim this page, which one
        // `image_is_ours` picks, and whether that one has a table for it.
        // Only the shim can see any of that. See #136.
        if (seen.size < 3 && !seen.has(page)) {
          seen.add(page);
          e.gex_explain_page?.(BigInt(page));
        }
      }
      else if (tn < 0 || tn >= tabs.length) what = 'no table';
      else {
        const slot = base + ((pc & 0xfff) >>> 2);
        what = slot >= tabs[tn].length
          ? `past end ${tabs[tn].length}`
          : tabs[tn].get(slot) !== stub ? 'written' : 'stub';
        // A stub, asked again for exactly this address's span: `FILL_ASKED`
        // keys a span once, so if the fill later runs at a different base the
        // gap is lost. `stub->written` means the span was mismatched,
        // `stub->stub` means there is nothing to write. See #136.
        if (what === 'stub') {
          const owner = e.gex_closure_owner
            ? e.gex_closure_owner(BigInt(page >>> 0)) : -1;
          const inst = owner >= 0 ? this.#lazyClosure[owner]?.instObj : null;
          const fill = inst?.exports?.[FILL_AT];
          if (typeof fill === 'function') {
            const mine = ((pc & 0xfff) >>> 2) & ~31;
            const said = fill(page, base, mine);
            const then = Gex.askState.get(`${page}:${mine}`);
            const now = `at=${base} len=${tabs[tn].length}`;
            const was = then
              ? `at=${then.at} len=${then.len} pub=${then.published ? 1 : 0}`
                + `${then.inst === inst ? '' : ' DIFFERENT-INSTANCE'}`
              : 'never asked for this span';
            // When the fill claims it wrote (`said=1`) but the slot is still a
            // stub, which table it actually wrote: a module's segments carry
            // their build table, which need not be the page's per-process table.
            // See task 148.
            let elsewhere = '';
            if (said && tabs[tn].get(slot) === stub) {
              for (let t = 0; t < tabs.length; t++) {
                if (t === tn) continue;
                if (slot < tabs[t].length && tabs[t].get(slot) !== stub) {
                  elsewhere += ` WROTE-TABLE-${t}`;
                }
              }
              if (!elsewhere) elsewhere = ' WROTE-NOWHERE';
            }
            what = tabs[tn].get(slot) !== stub
              ? `stub->written(was[${was}] now[${now}] said=${said ? 1 : 0})`
              : `stub->stub(was[${was}] now[${now}] said=${said ? 1 : 0}`
                + `${elsewhere} tn=${tn} slot=${slot})`;
          } else {
            what = `stub, owner=${owner} no fill`;
          }
        }
      }
      out.push(`${pc.toString(16)}:${what}`);
    }
    return out.length ? out.join(' ') : undefined;
  }

  // Whether a fill actually wrote the slot that *missed*, not merely something
  // in its span. `gexfillat` reports success when it wrote any of the page's
  // covered blocks, but on a page the closure covers only partly the missing
  // slot is a gap it leaves on the miss stub. Reporting that filled sends the
  // guest back to re-dispatch the same stub instead of demand-translating the
  // block, and the churn on a partial page corrupted execution -- the
  // closure-only fault `apt-get update` hit verifying an EdDSA signature and in
  // its forked methods. `gex_fill_pc` is the address that missed; with none
  // (a bulk fill) there is no single slot to insist on, so trust the report.
  #wroteWanted(page, at) {
    const mpc = Number(this.#shim.exports.gex_fill_pc?.() ?? 0n);
    if (!mpc || at <= 0) return true;
    const tn = this.#shim.exports.gex_page_table
      ? this.#shim.exports.gex_page_table(BigInt(page >>> 0)) : -1;
    const tabs = this.#dispatchTables();
    if (tn < 0 || tn >= tabs.length) return true;
    const want = at + ((mpc & 0xfff) >>> 2);
    return want < tabs[tn].length && tabs[tn].get(want) !== tabs[0].get(0);
  }

  // Fill a page without waiting, or null if impossible. The common case: the
  // page belongs to a library this process already built, and filling is a call
  // into wasm rather than the `await` of `fillSlot` (a whole event-loop turn).
  fillSlotNow(page) {
    const e = this.#shim.exports;
    // Every object that covers this page, not just the first: translation is
    // batched by block, so one image page can span several objects. See #136
    // and `gex_closure_owner_at`.
    let owned = false;
    let filled = false;
    for (let k = 0; ; k++) {
      const owner = e.gex_closure_owner_at
        ? e.gex_closure_owner_at(BigInt(page), k)
        : (k === 0 && e.gex_closure_owner ? e.gex_closure_owner(BigInt(page)) : -1);
      if (owner < 0) break;
      owned = true;
      {
      const lazy = this.#lazyClosure[owner];
      // Which of the three ways this gives up, for the watched pages: an owned,
      // claimed image page should be answered here and after an execve was not.
      // See #136.
      if (Gex.watchPages.has(page) && Gex.#whyNo < 24) {
        Gex.#whyNo++;
        const why = !lazy ? 'no lazy entry'
          : !lazy.instObj ? 'module not instantiated'
          : typeof lazy.instObj.exports[FILL_AT] !== 'function' ? 'no fill export'
          : 'reached the fill';
        console.error(`[closure] fill page ${page} owner ${owner}: ${why}`);
      }
      if (!lazy) continue;
      // Publishing is synchronous -- a directory write and a table grow --
      // so it belongs on this side of the fence.
      if (!lazy.published) {
        lazy.published = true;
        this.#publish(lazy, owner);
      }
      if (!lazy.instObj) continue;
      // A module below the block gate has *active* segments and no fill
      // helper: instantiating it wrote its slots, so there is nothing to ask
      // for and the page is already answered.
      if (typeof lazy.instObj.exports[FILL_AT] !== 'function') { filled = true; continue; }
      // An old closure exports `gexfillat(page, base)` and fills whole pages;
      // JS drops the extra `from` argument silently, so the arity is the honest
      // signal that the shim must ask once per page rather than per span.
      if (!this.#fillWidthTold) {
        this.#fillWidthTold = true;
        const wide = lazy.instObj.exports[FILL_AT].length < 3;
        if (wide) this.#shim.exports.gex_set_fill_width?.(SLOTS_PER_PAGE);
      }
      const b0 = performance.now();
      const at = this.#pageBase(page);
      const b1 = performance.now();
      this.#faBaseMs += b1 - b0;
      // `> 0`, not `>= 0`: zero is `#pageBase`'s "no translation" sentinel, and
      // filling at base 0 writes funcrefs over the bottom of table 0 and reports
      // success, wedging the address for good. See #136.
      // askState keeps what the module and table looked like here, so the retry
      // in `liftSlots` can be compared against it.
      const askFrom = this.#fillFrom();
      if (Gex.askState.size < 4096) {
        const tn0 = e.gex_page_table ? e.gex_page_table(BigInt(page >>> 0)) : -1;
        const tabs0 = this.#dispatchTables();
        Gex.askState.set(`${page}:${askFrom}`, {
          at,
          len: tn0 >= 0 && tn0 < tabs0.length ? tabs0[tn0].length : -1,
          published: !!lazy.published,
          inst: lazy.instObj,
        });
      }
      let did = at > 0 && lazy.instObj.exports[FILL_AT](page, at, askFrom);
      this.#faInitMs += performance.now() - b1;
      // Read back the one slot the guest will dispatch through: a span read-back
      // says the fill wrote something, this says it wrote the slot that missed.
      // See #136.
      if (Gex.watchSlot && at > 0) {
        const mpc = Number(this.#shim.exports.gex_fill_pc?.() ?? 0n);
        if (mpc && Gex.#whySlot < 12) {
          Gex.#whySlot++;
          const want = at + ((mpc & 0xfff) >>> 2);
          const tn2 = this.#shim.exports.gex_page_table
            ? this.#shim.exports.gex_page_table(BigInt(page >>> 0)) : -1;
          const tabs2 = this.#dispatchTables();
          let set = 'no table';
          if (tn2 >= 0 && tn2 < tabs2.length) {
            set = want < tabs2[tn2].length
              ? (tabs2[tn2].get(want) !== tabs2[0].get(0) ? 'WRITTEN' : 'still stub')
              : `past end (${tabs2[tn2].length})`;
          }
          console.error(`[closure] pc ${mpc.toString(16)} page ${page} slot ${want}`
            + ` (base ${at} + ${(mpc & 0xfff) >>> 2}) t=${tn2}: ${set}`);
        }
      }
      if (Gex.watchPages.has(page) && Gex.#whyDid < 24) {
        Gex.#whyDid++;
        // `did` only says the module wrote something in the span; count the
        // span rather than trusting it.
        const from = this.#fillFrom();
        const tn = this.#shim.exports.gex_page_table
          ? this.#shim.exports.gex_page_table(BigInt(page >>> 0)) : -1;
        const tabs = this.#dispatchTables();
        let wrote = -1, len = -1;
        if (tn >= 0 && tn < tabs.length) {
          const stub = tabs[0].get(0);
          len = tabs[tn].length;
          wrote = 0;
          for (let i = from; i < from + 128 && at + i < len; i++) {
            if (tabs[tn].get(at + i) !== stub) wrote++;
          }
        }
        console.error(`[closure] fill page ${page} owner ${owner}: at=${at} did=${did} `
          + `t=${tn} from=${from} wrote=${wrote}/128 tlen=${len}`);
      }
      // `did` says the module wrote *somewhere* in the span, not that it wrote
      // the slot that actually missed -- see `#wroteWanted`.
      if (did && this.#wroteWanted(page, at)) {
        this.#filled++;
        filled = true;
      }
      }
    }
    if (filled) return true;
    // Owned but unfillable: the caller's fallback scan cannot help, because
    // the objects that claim this page are the only ones that hold its code.
    if (owned) return null;
    if (page * 4096 < 0x0300_0000) return null;
    const where = this.#pageBase(page);
    // Same sentinel as above: zero is "no translation", not slot zero.
    if (where <= 0) return null;
    for (const inst of this.#unfilled) {
      if (inst.exports[FILL_AT](page, where, this.#fillFrom())
          && this.#wroteWanted(page, where)) {
        this.#filled++;
        return true;
      }
    }
    return null;
  }

  async fillSlot(slot) {
    // The library that covers this page, instantiated now if not yet. The
    // shim's ordinals are the registration order, the same as `#lazyClosure`.
    const owner = this.#shim.exports.gex_closure_owner
      ? this.#shim.exports.gex_closure_owner(BigInt(slot))
      : -1;
    if (owner >= 0) {
      const lazy = this.#lazyClosure[owner];
      if (lazy && !lazy.published) {
        lazy.published = true;
        this.#publish(lazy, owner);
      }
      if (lazy && !lazy.inst) {
        lazy.inst = true;
        // The compiled module, fetched now if registration only brought its
        // metadata: a Worker gets the metadata blobs up front and asks for a
        // module when a branch lands in it. Timed apart, since it is the host
        // round-trip, not the install, that costs the slow fill its second.
        const w0 = performance.now();
        const mod = lazy.mod ?? (this.#wantLibrary ? await this.#wantLibrary(owner) : null);
        this.#wants++;
        this.#wantMs += performance.now() - w0;
        if (mod) {
          lazy.mod = mod;
          // For a demand-filled (claimed) library, need 0 and no metadata: it
          // numbers its own pages one at a time, so the build's `dir_entries`
          // would grow the tables to a layout nobody uses. Only a module that
          // writes absolute slots at instantiation wants the metadata.
          const claimed = this.#shim.exports.gex_lib_lazy?.(owner >>> 0);
          const i0 = performance.now();
          lazy.instObj = await this.#installModule(
            mod,
            claimed ? 0 : lazy.need,
            64,
            claimed ? null : lazy.meta,
            'closure',
          );
          this.#instMs += performance.now() - i0;
        } else {
          // Nothing to install; let the miss fall through and be lifted rather
          // than wedging on a library that never arrives.
          lazy.inst = false;
        }
      }
    }
    const tail0 = performance.now();
    try {
      return await this.#fillSlotTail(slot, owner);
    } finally {
      this.#tailMs += performance.now() - tail0;
    }
  }

  async #fillSlotTail(slot, owner) {
    // The same for a module inherited from a parent: instantiating writes its
    // active segments, so there is nothing further to fill, but asking beats
    // the miss path re-serving code this process already holds.
    const heir = this.#shim.exports.gex_lazy_owner
      ? this.#shim.exports.gex_lazy_owner(BigInt(slot))
      : -1;
    if (heir >= 0) {
      const lazy = this.#lazyInherited[heir];
      if (lazy && !lazy.inst) {
        lazy.inst = true;
        await this.#installModule(lazy.mod, lazy.need, 64, lazy.meta, 'inherit');
        return true;
      }
    }
    // The owner first, and below `INTERP_BASE` the owner only: those pages
    // belong to every image at once and only one of them is ours.
    const mine = owner >= 0 ? this.#lazyClosure[owner]?.instObj : null;
    if (mine && typeof mine.exports[FILL_AT] !== 'function') return true;
    const at = this.#pageBase(slot);
    // Only "filled" when the slot that missed is off the stub, not merely when
    // the module wrote its span. See `#wroteWanted`.
    if (mine && at > 0 && mine.exports[FILL_AT](slot, at, this.#fillFrom())
        && this.#wroteWanted(slot, at)) {
      this.#filled++;
      return true;
    }
    if (slot * 4096 >= 0x0300_0000) {
      for (const inst of this.#unfilled) {
        if (at > 0 && inst.exports[FILL_AT](slot, at, this.#fillFrom())
            && this.#wroteWanted(slot, at)) {
          this.#filled++;
          return true;
        }
      }
    }
    // A request nobody can answer, then a clean run-time lift of the block: for
    // a skipped library (its pages have a run-time base but no deferred module)
    // this is every page and expected, so it narrates only under `debugFill`.
    // Capped even then, since a flood would be the failure, not the diagnosis.
    if (Gex.debugFill && this.#unanswered++ < 24) {
      console.error(
        `[fill] nothing holds entry ${slot >>> 0}: ` +
          `${this.#unfilled.length} deferred modules, ${this.#filled} filled`,
      );
    }
    return false;
  }

  /** Every dispatch table the guest module exports, in index order. */
  #dispatchTables() {
    const out = [];
    for (const n of TABLE_NAMES) {
      const t = this.#guest.exports[n];
      if (!t) break;
      out.push(t);
    }
    return out;
  }

}
