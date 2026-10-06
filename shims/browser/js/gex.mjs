// The browser runtime: instantiate the shim (gex-kernel compiled to wasm) and
// a guest module against one memory, and run. The shim imports the guest's
// memory, so a syscall reads guest buffers in place. `run-node.mjs` is the
// Node entry.

// How far a process's memory may grow. One memory per process. See
// `gex_abi::GUEST_MEM_INITIAL_BYTES`.
const GUEST_BYTES = 0xffff0000;

// A 64-bit guest memory (docs/mem64.md step 6): the wasm64 shim and seed, a
// memory64 of `gex_abi::LIMIT64` (16 GiB). The shim's own addresses (its
// stack, data and the low runtime words) stay below 4 GiB either way; only
// the memory's type and the i64 globals of a wasm64 module differ.
// Read when used, not at import: a Worker learns it from its first message.
const m64 = () => !!globalThis.GEX_MEM64;
const LIMIT64_BYTES = 16 * 2 ** 30;
/** A value for an address-typed global: i64 (BigInt) in a wasm64 module. */
const addrVal = (v) => (m64() ? BigInt(v >>> 0) : v | 0);

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

// Registry indices describe a shared queue, not module identity: two Workers
// can drain and publish that queue in different turns. A compiled Module may
// only be reused for byte-identical input, or its element segments install
// unrelated functions into otherwise-correct dispatch slots. Two independent
// 32-bit accumulators plus the exact length make a cheap content key; this is
// validation against an index race, not a persistent content-addressed cache.
function moduleContentKey(bytes) {
  if (!(bytes instanceof Uint8Array)) return null;
  let a = 0x811c9dc5;
  let b = 0x9e3779b9;
  for (let i = 0; i < bytes.length; i++) {
    const x = bytes[i];
    a = Math.imul(a ^ x, 0x01000193);
    b = Math.imul(b ^ (x + i), 0x85ebca6b);
    b ^= b >>> 13;
  }
  return `${bytes.length}:${a >>> 0}:${b >>> 0}`;
}
const GUEST_INITIAL_BYTES = 0x07000000;
// The shim's stack sits at the bottom of its region, below its data.
const SHIM_STACK_TOP = 0x06000000;
/** `gex_emit::dense::FIRST_TABLE`: dispatch tables from here on may be dense. */
const DENSE_FIRST_TABLE = 4;
/** The most a dispatch table grows past what is needed, in entries. */
const TABLE_STEP = 1 << 20;
/** `gex_abi::ctx::LAST_TARGET`. */
const CTX_LAST_TARGET = 44 * 8;
/** `gex_abi::ctx::PC`, and x86's `rsp` (`ctx::x(4)`). */
const CTX_PC = 32 * 8;
const CTX_SP = 4 * 8;
/** cgen.rs `CALLOUT_UNWOUND`: 64 KiB. */
const CALLOUT_UNWOUND = 64n << 10n;
/** gex-kernel `tramp::NATIVE_RETURN_PAD` and the native host's
 *  `NATIVE_UNWOUND` (docs/wine-native.md §3.15). */
const NATIVE_RETURN_PAD = 0x3ffff1n;
const NATIVE_UNWOUND = 512n;
/**
 * `table.grow`, said out loud once a table passes a million entries: each
 * Worker's dispatch tables are V8 heap, and their size is the renderer's.
 */
function growTable(table, delta, init, which) {
  const to = table.length + delta;
  if (to > (1 << 20)) console.warn(`[gex] dispatch table ${which} grows ${table.length} -> ${to} entries`);
  table.grow(delta, init);
}

/** `gex_abi::BOOT_STACK_COUNTER`: which bootstrap stack slice is next. */
const BOOT_STACK_COUNTER = 0x00100000 - 192;
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
// The shim's debug channel. A heartbeat line gets this isolate's own memory
// figures appended at node (GEX_MEMSTATS): a Worker's wasm memories and heap,
// which the main thread's `process.memoryUsage()` cannot see.
function debugLine(text) {
  if (typeof process !== 'undefined' && process.env?.GEX_MEMSTATS && text.startsWith('[beat]')) {
    try {
      const m = process.memoryUsage();
      const mib = (n) => (n / 1048576) | 0;
      text += ` | isolate heap=${mib(m.heapUsed)}MiB arrayBuffers=${mib(m.arrayBuffers)}MiB external=${mib(m.external)}MiB`;
      if (process.env.GEX_MEMSTATS === 'spaces') {
        const v8 = process.getBuiltinModule?.('v8') ?? null;
        if (v8) text += ' | ' + v8.getHeapSpaceStatistics().filter((sp) => sp.space_used_size > 8 << 20).map((sp) => `${sp.space_name}=${mib(sp.space_used_size)}`).join(' ');
      }
    } catch {}
  }
  console.debug(text);
}

// Dispatch slots one 4 KiB guest page covers (`gex_abi::SLOTS_PER_PAGE`) and
// the `log2` of the bytes a slot spans (`gex_abi::DISPATCH_SHIFT`): 1024 and
// 2 on both guests today. Asked of the shim once it exists (see
// `syncSlotShift`), so the glue never has to assume; 2 until then.
let SLOT_SHIFT = 2;
let SLOTS_PER_PAGE = 4096 >> SLOT_SHIFT;
function syncSlotShift(exports) {
  const s = exports?.gex_inst_align_shift?.();
  if (typeof s === 'number' && s >= 0 && s <= 2) {
    SLOT_SHIFT = s;
    SLOTS_PER_PAGE = 4096 >> s;
  }
}
const FILL_ALL = 'gexfillall';

// Above this, a module's slots are written when a branch asks rather than at
// instantiation: small units are wanted immediately, closure libraries mostly
// never reached. Provisional.
const FILL_ON_DEMAND_BYTES = 4 << 20;

/// Write `src` at `at`, leaving the pages of it that are entirely zero alone.
///
/// A page a guest process never made resident costs nothing, and a page written
/// with zeros over zeros is resident for no reason. A fresh
/// `WebAssembly.Memory` is zero-filled and so is every page `grow` adds, so any
/// all-zero page of a parent's image is *already* what the child should have --
/// writing it is the only thing that makes it cost memory.
///
/// This is not a guess about what the child needs, unlike `?forkskip`: the
/// bytes end up identical either way. It pays a read of the source (already
/// resident here -- it was just transferred in) to avoid a write to the
/// destination. Guests are sparse in the ordinary case: `.bss`, a fresh
/// anonymous mapping, the untouched tail of a heap or a stack.
///
/// Returns `[written, skipped]`.
function writeSparse(dst, src, at) {
  const PAGE = 4096;
  // Word-at-a-time when the source is aligned, which it is: the regions are
  // `slice`d, so each owns its buffer from offset zero.
  const words = src.byteOffset % 4 === 0 && src.byteLength % 4 === 0
    ? new Uint32Array(src.buffer, src.byteOffset, src.byteLength >> 2)
    : null;
  const blank = (from, to) => {
    if (words) {
      for (let i = from >> 2; i < to >> 2; i++) if (words[i] !== 0) return false;
      return true;
    }
    for (let i = from; i < to; i++) if (src[i] !== 0) return false;
    return true;
  };
  let written = 0;
  let skipped = 0;
  let i = 0;
  while (i < src.length) {
    const end = Math.min(i + PAGE, src.length);
    if (blank(i, end)) {
      skipped += end - i;
      i = end;
      continue;
    }
    // Coalesce the run of non-blank pages into one `set`, so a dense region is
    // still one copy and not one per page.
    let run = end;
    while (run < src.length) {
      const next = Math.min(run + PAGE, src.length);
      if (blank(run, next)) break;
      run = next;
    }
    dst.set(src.subarray(i, run), at + i);
    written += run - i;
    i = run;
  }
  return [written, skipped];
}

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
   * Block until everything `write` has been handed is delivered. Called just
   * before the process's exit is published. Synchronous output needs nothing.
   */
  drainOutput() {}

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

/** A module's `gex.dense` section (gex_emit::dense::SECTION), or null. */
export function denseSection(wasm) {
  if (wasm instanceof WebAssembly.Module) {
    const secs = WebAssembly.Module.customSections(wasm, 'gex.dense');
    return secs.length ? new Uint8Array(secs[0]) : null;
  }
  if (!(wasm instanceof Uint8Array)) return null;
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
      if (utf8(wasm.subarray(i, i + nameLen)) === 'gex.dense') return wasm.slice(i + nameLen, end);
    }
    i = end;
  }
  return null;
}

export function metadataSection(wasm) {
  // A compiled `WebAssembly.Module` -- which `attach` accepts, so the guest
  // image and the closure's libraries cross `postMessage` by reference rather
  // than re-cloning and re-compiling hundreds of megabytes per forked process
  // -- has no bytes to scan. Its custom sections are read through the
  // reflection API instead. Without this, a forked process in the browser threw
  // "First argument to DataView constructor must be an ArrayBuffer".
  if (wasm instanceof WebAssembly.Module) {
    const secs = WebAssembly.Module.customSections(wasm, 'gex.meta');
    return secs.length ? new Uint8Array(secs[0]) : null;
  }
  // A bare ArrayBuffer (or SharedArrayBuffer) has no `.buffer`, so the DataView
  // below would throw "First argument to DataView constructor must be an
  // ArrayBuffer"; wrap it. Anything else is not module bytes and has no
  // metadata to give -- a guard rather than a crash, matching
  // `elementTableNeeds`.
  if (wasm instanceof ArrayBuffer || (typeof SharedArrayBuffer !== 'undefined' && wasm instanceof SharedArrayBuffer)) {
    wasm = new Uint8Array(wasm);
  }
  if (!(wasm instanceof Uint8Array)) return null;
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

/** A stateful provider's `provider_throw`, caught by its `provider_try`. */
class ProviderThrow extends Error {
  constructor(code) {
    super(`gex.provider_throw(${code}) with no gex.provider_try to catch it`);
    this.code = code;
  }
}

export class Gex {
  #memory; #shim; #guest; #env; #u8; #imports; #net = null; #gl = null; #wl = null;
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
  // How many "table index out of bounds" dispatch traps were repaired by
  // catching this Worker's tables up to the shared claimed high-water. Capped
  // in the run loop so a genuine over-run still fails.
  #tableRetries = 0;
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
  /// Compiled `WebAssembly.Module`s for the process's registry, by index,
  /// shared across the sibling Workers of one process. A thread `#catchUp`s the
  /// same modules its siblings already compiled; recompiling each from bytes
  /// per Worker cost seconds (16.9s of a 45s gtk3-demo run, thousands of
  /// modules re-`compile`d). Populated from `sharedModule` messages the host
  /// relays; a `WebAssembly.Module` crosses `postMessage` by reference, so this
  /// carries no byte copies. Only the compile is shared -- each Worker still
  /// instantiates against its own tables.
  #sharedModules = new Map();
  /// A spawned thread that registers the process's modules lazily (directory +
  /// tables now, instance on the branch that wants it) instead of installing
  /// them all up front. Set in `adoptThread` from `GEX_LAZY_THREAD`. See
  /// `#catchUpLazy` and gtk-startup-is-per-thread-install.
  #lazyCatchUp = false;
  /** Whether this process numbers its blocks densely (gex_emit::dense): a
   *  dense table's slots live in the process's slot maps, and this Worker's
   *  only table for them is `blocks`, one entry per block. */
  #dense = false;
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
  /// Bytes of the parent's image this child wrote into its own memory, and
  /// bytes it was told to leave alone by `?forkskip`. Reported on exit: with no
  /// copy-on-write, the first number is what a fork costs the tab.
  #forkCopied = 0;
  #forkSkipped = 0;
  #forkZeros = 0;
  get forkCopiedBytes() { return this.#forkCopied; }
  get forkSkippedBytes() { return this.#forkSkipped; }
  get forkZeroBytes() { return this.#forkZeros; }
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
      memory ??
      (m64()
        ? new WebAssembly.Memory({ address: 'i64', initial: BigInt(INITIAL_PAGES), maximum: BigInt(LIMIT64_BYTES / 65536), shared })
        : new WebAssembly.Memory({ initial: INITIAL_PAGES, maximum: PAGES, shared }));
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
      // `gex_worker_init` runs on a stack too, and until it answers this
      // instance's is wasm-ld's: the top of the guest's first 8 MiB, which is
      // a Windows program's image (noita.exe's .text at 0x7ff000..0x800000
      // was overwritten by every thread that started). So it runs on a
      // bootstrap slice of the shim's own stack band below SHIM_STACK_TOP,
      // one of 32 handed out round-robin through a counter in the reserved
      // low region -- Workers starting together get different ones.
      if (typeof this.#shim.exports.gex_worker_init?.call === 'function' && this.#memory.buffer instanceof SharedArrayBuffer) {
        const k = Atomics.add(new Int32Array(this.#memory.buffer), BOOT_STACK_COUNTER >> 2, 1) & 31;
        this.#shim.exports.__stack_pointer.value = addrVal(SHIM_STACK_TOP - k * 0x40000);
      }
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
    this.#memory.grow(m64() ? BigInt(pages) : pages);
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
   * Every guest block PC translated in this process so far, as a BigUint64Array
   * (a fresh copy, safe to keep). The profile a sandbox run dumps to seed a
   * reachability-trimmed closure build. Null if the shim lacks the export.
   */
  coverage() {
    const e = this.#shim?.exports;
    if (!e?.gex_coverage_snapshot) return null;
    const n = e.gex_coverage_snapshot() >>> 0;
    const ptr = e.gex_coverage_ptr() >>> 0;
    // Copied out: the snapshot lives in linear memory that a later host call
    // may reallocate, and the caller writes it to disk asynchronously.
    return new BigUint64Array(this.#memory.buffer.slice(ptr, ptr + n * 8));
  }

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
      blen: this.#guest?.exports?.blocks?.length ?? -1,
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
        debug: (ptr, len) => debugLine(
          utf8(
            this.#bytes().subarray(ptr >>> 0, (ptr >>> 0) + (len >>> 0)),
          ).replace(/\n$/, ''),
        ),
        warn: (ptr, len) => {
          const text = utf8(
            this.#bytes().subarray(ptr >>> 0, (ptr >>> 0) + (len >>> 0)),
          ).replace(/\n$/, '');
          // `[gex-stack]`: the shim wants to be told who called it (the wasm
          // frames are named).
          if (text.startsWith('[gex-stack]')) {
            // The run loop turns stack traces off around guest code.
            const limit = Error.stackTraceLimit;
            Error.stackTraceLimit = 40;
            const stack = new Error().stack;
            Error.stackTraceLimit = limit;
            console.error(`${text}\n${stack}`);
          } else {
            console.error(text);
          }
        },
        now: () => this.#env.now(),
        // The NT layer's monotonic clock: `now` is `Date.now()`, a
        // millisecond wall clock that can step.
        // An f64, not an i64: a BigInt per call was most of an NT syscall's cost.
        mono_ns: () => performance.now() * 1e6,
        // A Windows program's user-mode callback (docs/nt-layer.md §5): the
        // guest runs nested, inside the `syscall` import, until it returns.
        nt_callback: (ctx, pc) => this.#ntCallback(ctx >>> 0, pc),
        // The function table's fallback (cgen.rs `gex_ftab_fallback`): tier-1
        // runs nested until the guest returns to the function body that
        // called it.
        call_out: (ctx, target, ret, spWant) => this.#callOut(ctx >>> 0, target, ret, spWant),
        // An AOT-only fallback whose target this Worker has just installed
        // (cgen.rs `gex_ftab_fallback`): called as a function body.
        ftab_call: (idx, ctx, depth, need, ret) => this.#cgen.ftab.get(idx >>> 0)(ctx, depth, need, ret),
        // A dispatch miss installs in place (lib.rs `in_place`).
        jit_install: (_ctx) => this.#jitInstall(),
        // An exception dispatched nested (nt.rs `raise_nested`).
        exc_dispatch: (ctx, target, level) => this.#excDispatch(ctx >>> 0, target, level),
        // An AOT-only run serves units and new threads inside the syscall
        // (nt.rs `serve_in_place`), as the NT callback loop does.
        serve_in_place: () => {
          this.#cgenServe();
          this.#drainThread().catch((err) => console.error(`[gex-nt] starting a thread: ${err.message ?? err}`));
        },
        drain_output: () => this.#env.drainOutput?.(),
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
        // The token as well as the ring: the host keeps a GL context per
        // process, because the object names in the stream are this process's
        // own and another process hands out the same ones.
        gl_submit: (ring) =>
          (this.#gl
            ? this.#gl.submit(this.#memory, ring >>> 0, this.wlToken(), false, this.#glWaitNote)
            : -1),
        // The same, not waited for (a frame's last batch): the copy is taken
        // and the guest draws on; the next submit waits for this one, and
        // says how long through `gex_gl_note_wait_ns`.
        gl_submit_async: (ring) =>
          (this.#gl
            ? this.#gl.submit(this.#memory, ring >>> 0, this.wlToken(), true, this.#glWaitNote)
            : -1),
        // This pump's scene: `count` surface records at `ptr` in this memory,
        // saying where each surface's pixels are rather than carrying them. The
        // compositor reads those addresses out of this same memory, so a frame
        // is never copied on its way to the screen.
        wl_publish: (ptr, count) =>
          this.#wl?.publish(this.#memory, ptr >>> 0, count >>> 0),
        // The reverse: drain this process's input and control ring into `ptr`,
        // so the compositor pump can deliver each event to the client and
        // surface the compositor's own hit test picked.
        wl_poll_input: (ptr, max) =>
          this.#wl?.pollInput(this.#memory, ptr >>> 0, max >>> 0) ?? 0,
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
    this.#shim.exports.__stack_pointer.value = addrVal(this.#workerStack());
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
    // And `useCgen`, before the process boots: the exe and ntdll are mapped
    // there, and their units are looked up then.
    if (this.#cgenMode) this.#shim.exports.gex_cgen_set_mode?.(this.#cgenMode);
    if (this.#noElfProviders) this.#shim.exports.gex_elfprov_off?.();
    this.#shim.exports.gex_set_wl?.(this.#wl ? 1 : 0);
    // `kernel.net` is decided inside `gex_load` from this flag, so it must be
    // set before it (not merely in `useNetwork`, before the shim exists).
    this.#shim.exports.gex_set_network?.(this.#net ? 1 : 0);
    // Narrate one page's directory writes, in every process. See #136.
    if (Gex.watchDir >= 0) this.#shim.exports.gex_watch_dir?.(BigInt(Gex.watchDir));
    // wasm-ld puts the shim's stack at address zero (the guest's image); move
    // it into the shim's own region, above everything the guest maps.
    this.#shim.exports.__stack_pointer.value = addrVal(this.#workerStack());
    // Carried into every fork child's Worker (see the fork record's `opts`), so
    // a debugging switch set for the run holds for the processes it spawns --
    // the block trace of a child that crashes is otherwise silent.
    this.#opts = {
      strace: opts.strace,
      // Which single process to narrate, if any. Inherited like `strace` --
      // the interesting process is usually a child, and a whole session's
      // syscalls is more output than a page survives.
      stracePid: opts.stracePid,
      tty: opts.tty, uid: opts.uid, gid: opts.gid,
      traceBlocks: opts.traceBlocks, traceMisses: opts.traceMisses, checkSp: opts.checkSp,
      watchMem: opts.watchMem,
      noIntrinsics: opts.noIntrinsics, intrinsicMask: opts.intrinsicMask,
    };
    syncSlotShift(this.#shim.exports);
    if (opts.strace) this.#shim.exports.gex_set_trace(1);
    // An ejected project without a JIT fallback: a miss on code its build
    // never saw fails loudly instead of translating.
    if (opts.jit === false) {
      if (!this.#shim.exports.gex_set_jit) throw new Error('this gex runtime cannot turn translation off');
      this.#shim.exports.gex_set_jit(0);
    }
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
    // A large file goes over in pieces through one buffer: the shim's heap
    // never gives a span back, so a file-sized buffer would be held for the
    // process's life (and a Windows process loses that address space).
    const PART = 16 << 20;
    let partBuf = 0;
    for (const [path, contents] of Object.entries(files)) {
      const p = enc.encode(path);
      const d = contents instanceof Uint8Array ? contents : enc.encode(contents);
      if (d.length > PART && this.#shim.exports.gex_add_file_part) {
        const pp = this.#shim.exports.gex_alloc(p.length) >>> 0;
        this.#bytes().set(p, pp);
        if (!partBuf) partBuf = this.#shim.exports.gex_alloc(PART) >>> 0;
        for (let off = 0; off < d.length; off += PART) {
          const piece = d.subarray(off, Math.min(off + PART, d.length));
          this.#bytes().set(piece, partBuf);
          this.#shim.exports.gex_add_file_part(pp, p.length, BigInt(off), partBuf, piece.length);
        }
        if (modes[path] !== undefined) this.#shim.exports.gex_chmod?.(pp, p.length, modes[path]);
        free?.(pp, p.length);
        continue;
      }
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
    if (partBuf) free?.(partBuf, PART);
    // Hard links (`web/src/tar.js`), after their targets: one inode with two
    // names, as the host had it. A shim that cannot link gets a copy.
    for (const [path, target] of Object.entries(opts.hard ?? {})) {
      const p = enc.encode(path);
      const t = enc.encode(target);
      const pp = this.#shim.exports.gex_alloc(p.length) >>> 0;
      const tp = this.#shim.exports.gex_alloc(t.length) >>> 0;
      this.#bytes().set(p, pp);
      this.#bytes().set(t, tp);
      const linked = this.#shim.exports.gex_add_hardlink?.(tp, t.length, pp, p.length) ?? 0;
      const d = files[target];
      if (!linked && d) {
        const dp = this.#shim.exports.gex_alloc(d.length) >>> 0;
        this.#bytes().set(d, dp);
        this.#shim.exports.gex_add_file(pp, p.length, dp, d.length);
        free?.(dp, d.length);
      }
      if (modes[target] !== undefined) this.#shim.exports.gex_chmod?.(pp, p.length, modes[target]);
      free?.(pp, p.length);
      free?.(tp, t.length);
    }
    // The bundle's file times (seconds since the epoch), as the host had them:
    // a program that records one -- gzip's header, make's rebuild test --
    // sees what it would natively rather than 1970.
    for (const [path, secs] of Object.entries(opts.mtimes ?? {})) {
      if (!this.#shim.exports.gex_set_mtime) break;
      const p = enc.encode(path);
      const pp = this.#shim.exports.gex_alloc(p.length) >>> 0;
      this.#bytes().set(p, pp);
      this.#shim.exports.gex_set_mtime(pp, p.length, BigInt(Math.trunc(secs)));
      free?.(pp, p.length);
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
    // The program's own path, when the embedder knows it (an ejected
    // project): what `/proc/self/exe` says, whatever argv[0] is.
    if (opts.exe) {
      if (!this.#shim.exports.gex_set_exe) throw new Error('this gex runtime cannot be told the program path');
      const b = enc.encode(opts.exe);
      const bp = this.#shim.exports.gex_alloc(b.length) >>> 0;
      this.#bytes().set(b, bp);
      this.#shim.exports.gex_set_exe(bp, b.length);
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
      // An embedder may vet each system call first (an ejected project's
      // allow-list); it throws to stop the run.
      syscall: opts.syscallGuard
        ? (ctx, pc) => {
          opts.syscallGuard(ctx >>> 0, pc, this.#memory);
          return e.gex_syscall(ctx, pc);
        }
        : e.gex_syscall,
      dispatch_miss: e.gex_dispatch_miss,
      illegal: e.gex_illegal,
      breakpoint: e.gex_breakpoint,
      sysreg_read: e.gex_sysreg_read,
      // An ejected project's library replacements (`Intrinsic::Replace`,
      // sysreg -200) are served by the embedder; everything else by the shim.
      sysreg_write: opts.replace
        ? (ctx, pc, reg, v) => (reg === -200
          ? opts.replace(ctx >>> 0, pc, Number(v), this.#memory, this.#imports)
          : e.gex_sysreg_write(ctx, pc, reg, v))
        : e.gex_sysreg_write,
      cache_op: e.gex_cache_op,
      safepoint: e.gex_safepoint,
      fma: e.gex_fma,
      x87: e.gex_x87,
    };
    const tGuest = performance.now();
    this.#guest = await WebAssembly.instantiate(
      guestWasm instanceof WebAssembly.Module ? guestWasm : await WebAssembly.compile(guestWasm),
      { gex: this.#imports },
    );
    this.#guestInstantiateMs = performance.now() - tGuest;
    this.#tnAt('boot');
    // Dense block tables when the image module has `blocks` (the seed does;
    // a closure image, whose libraries dispatch classically, does not).
    this.#dense = opts.dense !== false && !!this.#guest.exports.blocks && !!e.gex_set_dense;
    this.#opts.dense = this.#dense ? (opts.dense === 2 ? 2 : 1) : false;
    e.gex_set_dense?.(this.#dense ? (opts.dense === 2 ? 2 : 1) : 0);
    // The function JIT (fjit.rs): its translations reach the function table,
    // which the function tier's plumbing drives (units where any exist).
    if (opts.fjit && e.gex_set_fjit) {
      e.gex_set_fjit(opts.fjit === 2 ? 2 : 1);
      if (opts.fjitMax != null || opts.fjitOnly) e.gex_fjit_limit?.(opts.fjitMax ?? 0xffffffff, BigInt(opts.fjitOnly ?? 0));
      if (!this.#cgenMode) this.useCgen(1);
    }
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
    // Its dense blocks keep the parent's numbering: point this process's
    // slots at them where nothing newer does.
    const ds = this.#dense ? denseSection(m.mod) : null;
    if (ds && e.gex_dense_adopt) {
      const dp = e.gex_alloc(ds.length) >>> 0;
      this.#bytes().set(ds, dp);
      e.gex_dense_adopt(dp, ds.length);
      e.gex_free?.(dp, ds.length);
    }
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
    const _at = (s) => { if (globalThis.GEX_ATTACH_TRACE) console.error(`[attach] ${s}`); };
    _at('start');
    await this.#startKernel();
    _at('kernel started');
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
    _at('shim instantiated');
    this.#u8 = null;
    // The stack pointer is a per-instance global, so every Worker has its own
    // and must be told where it goes.
    this.#shim.exports.__stack_pointer.value = addrVal(this.#workerStack());
    // The devices this process has -- process-wide flags in shared memory, so
    // only a *process* sets them. A thread answering for itself would turn a
    // GPU or network off underneath the threads using it. Before the kernel is
    // built, which is when the device nodes are made and the vtest socket bound.
    if (devices) {
      this.#shim.exports.gex_set_gl?.(this.#gl ? 1 : 0);
      this.#shim.exports.gex_set_wl?.(this.#wl ? 1 : 0);
      this.#shim.exports.gex_set_network?.(this.#net ? 1 : 0);
      // The unit cache and ELF export providers, as `load` gives a process its
      // own: a forked child's table starts empty while its GOT holds what
      // its parent's providers made code (cgen/elfprov.rs).
      if (this.#cgenMode) this.#shim.exports.gex_cgen_set_mode?.(this.#cgenMode);
      if (this.#noElfProviders) this.#shim.exports.gex_elfprov_off?.();
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
      x87: e.gex_x87,
    };
    const tGuest = performance.now();
    this.#guest = await WebAssembly.instantiate(
      guestWasm instanceof WebAssembly.Module ? guestWasm : await WebAssembly.compile(guestWasm),
      { gex: this.#imports },
    );
    this.#guestInstantiateMs = performance.now() - tGuest;
    _at('guest instantiated');
    // A thread joins its process's numbering; a forked child decides once its
    // kernel exists (`adoptProcess`).
    this.#dense = !!e.gex_dense_on?.() && !!this.#guest.exports.blocks;
    this.#offerToFill(this.#guest);
    _at('offerToFill done, catchUp');
    // Everything the process translated before this thread existed.
    await this.#catchUp();
    _at('attach catchUp done');
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
    const _k = (s) => { if (globalThis.GEX_ATTACH_TRACE) console.error(`[kernel] ${s}`); };
    if (!this.#kernelSpec || this.#kernel) { _k(`skip (spec=${!!this.#kernelSpec} kernel=${!!this.#kernel})`); return; }
    const { module, memory, stackTop } = this.#kernelSpec;
    _k(`compile (isModule=${module instanceof WebAssembly.Module} stackTop=0x${(stackTop>>>0).toString(16)})`);
    const mod = module instanceof WebAssembly.Module
      ? module
      : await WebAssembly.compile(module);
    _k('instantiate');
    const inst = await WebAssembly.instantiate(mod, {
      env: { memory, process: this.#memory },
    });
    _k('instantiated');
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
    // Some of these return an i64, which JS has to answer with a BigInt.
    const BIG = new Set(['k_wait', 'k_take_signals', 'k_take_kill_signals', 'k_tty_winsize']);
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
    // `?forkskip=N`: leave any region larger than N MiB uncopied.
    //
    // There is no copy-on-write here -- one guest process is one linear memory,
    // so a fork writes the parent's mappings into the child and every byte of
    // that is a byte the tab pays for. Whether that copy is *needed* is a
    // question about the child's future, and this is how to ask it: skip the
    // big regions, leave them zero, and see whether the run still passes. A
    // child that execs immediately (nearly all of them) never looks at them.
    //
    // Destructive on purpose -- if the guest reads a skipped page it reads
    // zeros -- so it is a measurement, not a mode.
    const skipOver = typeof globalThis.GEX_FORK_SKIP_MIB === 'number'
      ? globalThis.GEX_FORK_SKIP_MIB * (1 << 20)
      : Infinity;
    let copied = 0;
    let skipped = 0;
    let zeros = 0;
    for (const r of child.image) {
      const n = r.bytes.byteLength;
      if (n > skipOver) {
        skipped += n;
        continue;
      }
      const [wrote, blank] = writeSparse(u8, new Uint8Array(r.bytes), r.at);
      copied += wrote;
      zeros += blank;
    }
    this.#forkCopied = copied;
    this.#forkSkipped = skipped;
    this.#forkZeros = zeros;
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
    if (child.compat) e.gex_set_compat?.(1);
    if (child.wasm32) e.gex_wasmprog_adopt?.(BigInt(child.wasm32));
    // The parent's block numbering carries on in the child (its low region),
    // over slot maps of its own (`dense::child`).
    this.#dense = !!(child.opts ?? {}).dense && !!this.#guest.exports.blocks && !!e.gex_set_dense;
    e.gex_set_dense?.(this.#dense ? ((child.opts ?? {}).dense === 2 ? 2 : 1) : 0);
    step(`kernel built, entry 0x${entry.toString(16)}, ${live()} non-zero bytes in the context`);
    // The kernel exists now, so everything that configures one can be said.
    const o = child.opts ?? {};
    this.#opts = o;
    syncSlotShift(e);
    // `?strace=1` narrates every process; `?stracepid=N` only that one, which
    // is what a run that must stay alive can afford.
    if (o.strace || (o.stracePid && o.stracePid === child.pid)) e.gex_set_trace(1);
    if (o.traceBlocks) e.gex_set_trace_blocks?.(1);
    if (o.watchMem) e.gex_set_watch_mem?.(BigInt(o.watchMem));
    if (o.traceMisses) e.gex_set_trace_misses?.(1);
    if (o.checkSp) e.gex_set_check_sp?.(1);
    if (o.noIntrinsics) e.gex_set_intrinsics?.(0);
    if (o.intrinsicMask !== undefined) e.gex_set_intrinsic_mask?.(o.intrinsicMask);
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
    const thread = {
      tid,
      ctx: e.gex_thread_ctx(),
      pc: e.gex_thread_pc(),
      memory: this.#memory,
      // A thread of a 32-bit process decodes in compatibility mode too.
      compat: e.gex_compat?.() ?? 0,
    };
    if (globalThis.GEX_ATTACH_TRACE) console.error(`[spawn] #drainThread posting tid ${tid} (t=${Date.now()})`);
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
    const _t = (s) => { if (globalThis.GEX_ATTACH_TRACE) console.error(`[thr ${thread.tid}] ${s}`); };
    _t('attached, adopting');
    const entry = this.#shim.exports.gex_adopt_thread(thread.tid);
    if (!entry) throw new Error(`this process has no thread ${thread.tid}`);
    if (thread.compat) this.#shim.exports.gex_set_compat?.(1);
    _t(`adopted, catchUp (${(this.#shim.exports.gex_module_count?.()>>>0)} modules)`);
    // A spawned thread runs almost none of the process's modules, so registering
    // them lazily (instance on the branch that wants it) skips compiling and
    // instantiating the rest -- the dominant startup cost for a threaded app.
    // Opt-in while it proves out; the eager path below is the safe default.
    this.#lazyCatchUp = !!globalThis.GEX_LAZY_THREAD;
    // Install every already-translated module into this Worker's own tables
    // before it runs an instruction. The funcref tables are per-thread (a wasm
    // table is not shareable) while the directory naming their slots is shared
    // and already written, so a fresh thread branches into a slot its own table
    // has never filled. Catching up on the next trap raced the branch and
    // deadlocked; doing it here once makes the first run deterministic.
    await this.#catchUp();
    // And the function table's installs (a wasm32 program's module, units):
    // its entry is a bridge into one.
    this.#cgenCatchUp();
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
      // A 32-bit process forks a 32-bit child; its Worker's shim must decode
      // in compatibility mode from the first block.
      compat: e.gex_compat?.() ?? 0,
      // A wasm32 program's entry: the child installs the same module.
      wasm32: e.gex_wasmprog_entry?.() ?? 0n,
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
    // A spawned thread branches into almost none of the process's modules (a
    // GLib pool worker runs `start_thread` and then sits in a condvar), yet
    // eager catch-up compiles+instantiates every one -- ~48% of an idle helper
    // thread's whole startup, on code it never runs (see gtk-startup profile).
    // The lazy path registers each module's directory the same way a fork's
    // inherited modules are (`gex_add_lazy_module`) but defers the instance to
    // the branch that wants it (`fillSlot`). Off unless the thread opted in.
    if (this.#lazyCatchUp && e.gex_add_lazy_module) {
      return this.#catchUpLazy(have, need);
    }
    const t0 = globalThis.GEX_ATTACH_TRACE ? performance.now() : 0;
    const comp0 = this.#compileMs;
    let n = 0, reused = 0;
    for (; this.#installed < have; this.#installed++) {
      const i = this.#installed;
      const ptr = e.gex_module_ptr(i) >>> 0;
      const len = e.gex_module_len(i) >>> 0;
      if (!len) continue;
      // Copied out before compiling: the registry is in the guest's memory,
      // and another thread allocating can move it.
      const bytes = this.#bytes().slice(ptr, ptr + len);
      // A sibling Worker may already have compiled this module: reuse its
      // `WebAssembly.Module` (same registry bytes -> same module) rather than
      // paying `WebAssembly.compile` again. The bytes are still needed for the
      // metadata and table-need computation the install does regardless.
      const key = moduleContentKey(bytes);
      const cached = key ? this.#sharedModules.get(key) : undefined;
      let who = 'jit2049';
      if (globalThis.GEX_DUMP_WHERE ?? true) who = `jit2049-${i}-at-${ptr.toString(16)}`;
      try {
        await this.#installModule(bytes, need, 64, null, who, cached);
      } catch (err) {
        if (err instanceof WebAssembly.CompileError) {
          e.gex_nt_where?.(ptr | 0, len | 0);
          // And whether the registry's copy is still what was read.
          const now = this.#bytes().slice(ptr, ptr + len);
          let diff = 0;
          for (let j = 0; j < len; j++) if (now[j] !== bytes[j]) diff++;
          console.error(`[gex] module ${i} at 0x${ptr.toString(16)}+0x${len.toString(16)}: ${diff} bytes differ from the copy compiled`);
        }
        throw err;
      }
      if (cached) { reused++; continue; }
      // Newly compiled here: hand it to the host to relay to siblings, and keep
      // it so this Worker does not compile it twice either.
      const mod = this.#compiled[this.#compiled.length - 1]?.mod;
      if (mod && key) {
        this.#sharedModules.set(key, mod);
        this.#env.shareModule?.(key, mod);
      }
      n++;
    }
    if (globalThis.GEX_ATTACH_TRACE && (n || reused)) {
      console.error(`[catchup] installed ${n + reused} modules in ${Math.round(performance.now() - t0)}ms`
        + ` (${Math.round(this.#compileMs - comp0)}ms compile, ${reused} reused from siblings)`);
    }
  }

  /**
   * A `WebAssembly.Module` a sibling Worker compiled, relayed by the host. Kept
   * by registry index so this Worker's `#catchUp` reuses it instead of
   * recompiling the same bytes. See `#sharedModules`.
   */
  addSharedModule(key, mod) {
    if (typeof key === 'string' && mod instanceof WebAssembly.Module && !this.#sharedModules.has(key)) {
      this.#sharedModules.set(key, mod);
    }
  }

  /**
   * Catch up on the process's modules the lazy way: register each one's
   * directory and grow the tables, but leave the instance for the branch that
   * wants it -- exactly how a fork's inherited modules and closure libraries
   * are handled (`#adoptModule`, `#lazyClosure`), and through the same
   * `gex_add_lazy_module`/`gex_lazy_owner`/`fillSlot` path. A spawned thread
   * runs a handful of the process's ~thousands of modules, so this skips
   * compiling and instantiating the rest. The registry bytes carry the same
   * directory the main thread already published, so re-registering is
   * idempotent; it only fills in *this* Worker's own `LAZY_OWNER` map so its
   * miss path can find the module.
   */
  async #catchUpLazy(have, need) {
    const e = this.#shim.exports;
    // Grow the funcref tables once to the whole claimed range, so any branch
    // resolves to a slot (its instance is written when the branch faults).
    if (need) this.#growTables(need, 'lazy-catchup');
    let lazy = 0, eager = 0;
    for (; this.#installed < have; this.#installed++) {
      const i = this.#installed;
      const ptr = e.gex_module_ptr(i) >>> 0;
      const len = e.gex_module_len(i) >>> 0;
      if (!len) continue;
      const bytes = this.#bytes().slice(ptr, ptr + len);
      const meta = metadataSection(bytes);
      // No metadata means nothing to key a later fill on -- install it eagerly,
      // as the fork path does (`#adoptModule`), reusing a sibling's compile.
      if (!meta || !meta.length) {
        await this.#installModule(bytes, need, 64, null, 'lazy-nometa', this.#sharedModules.get(moduleContentKey(bytes)));
        eager++;
        continue;
      }
      const gp = e.gex_alloc(meta.length) >>> 0;
      this.#bytes().set(meta, gp);
      const got = e.gex_add_lazy_module(gp, meta.length) >>> 0;
      e.gex_free?.(gp, meta.length);
      if (!got) {
        await this.#installModule(bytes, need, 64, meta, 'lazy-noneed', this.#sharedModules.get(moduleContentKey(bytes)));
        eager++;
        continue;
      }
      // Keep a sibling's compiled Module when we have it, so the on-demand
      // instantiate skips the compile too; otherwise the raw bytes. The push
      // order matches `gex_add_lazy_module`'s ordinal (both count from 0 here).
      this.#lazyInherited.push({ mod: this.#sharedModules.get(moduleContentKey(bytes)) ?? bytes, need, inst: null, meta });
      lazy++;
    }
    if (globalThis.GEX_ATTACH_TRACE && (lazy || eager)) {
      console.error(`[catchup] lazy-registered ${lazy} modules (${eager} eager, deferred instantiation)`);
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
   * Give the guest a graphics device. `gl` is a `MesaGlDevice` from web/src/mesa-gl.js.
   * Off by default, like the network. Best set before `load`, since the device
   * node is made while the filesystem is built; a later call still works.
   */
  /** How long a GL submit waited for the unwaited batch before it, to the
   *  shim's per-thread time split. */
  #glWaitNote = (ms) => this.#shim?.exports?.gex_gl_note_wait_ns?.(ms * 1e6);

  /** The function tier's whole-image units (`cgen/shims/browser/src/cgen.rs`):
   *  1 use one where it exists, 2 also require one for a program image. The
   *  Env's `loadUnit(key)` returns `{ wasm, entries }` (bytes, or a compiled
   *  `WebAssembly.Module` for `wasm`) or null. */
  useCgen(mode) {
    this.#cgenMode = mode >>> 0;
    if (this.#shim) this.#shim.exports.gex_cgen_set_mode?.(this.#cgenMode);
  }

  #cgenMode = 0;
  /** No ELF export providers (cgen/elfprov.rs): A/B runs, every process of
   *  the run (a forked child's Worker is told too). Before the shim exists. */
  noElfProviders() {
    this.#noElfProviders = true;
    this.#shim?.exports.gex_elfprov_off?.();
  }
  #noElfProviders = false;
  /** This Worker's half of the function table (docs/merge-plan.md §9.8.1):
   *  its `gex.ftab`, the runtime module's `resolve` and `bridge`, and how far
   *  through the table's log it is. Made at the first unit. */
  #cgen = null;

  /** A string the shim writes for unit `u` through `fn(u, buf, cap)`. */
  #cgenStr(fn, u) {
    const e = this.#shim.exports;
    const cap = 1024;
    const buf = e.gex_alloc(cap) >>> 0;
    try {
      const n = Math.min(fn(u, buf, cap) >>> 0, cap);
      return new TextDecoder().decode(this.#bytes().slice(buf, buf + n));
    } finally {
      e.gex_free?.(buf, cap);
    }
  }

  /** The unit module for `key`: a sibling's, or loaded and compiled here and
   *  offered to the siblings. Synchronous, so the nested callback loop can
   *  install too. */
  #cgenModule(key, got = null) {
    const name = `cgen:${key}`;
    let mod = this.#sharedModules.get(name);
    if (mod) return mod;
    const tl = performance.now();
    got ??= this.#env.loadUnit?.(key) ?? null;
    if (!got?.wasm) throw new Error(`gex-cgen: unit ${key} was published but cannot be loaded here`);
    const t0 = performance.now();
    mod = got.wasm instanceof WebAssembly.Module ? got.wasm : new WebAssembly.Module(got.wasm);
    this.#compileMs += performance.now() - t0;
    // Start-up's largest costs, one line per unit (a page loads about 40).
    console.warn(`[unit] ${key.slice(0, 16)} ${((got.wasm.byteLength ?? 0) / 1048576).toFixed(1)} MiB: `
      + `load ${Math.round(t0 - tl)} ms, compile ${Math.round(performance.now() - t0)} ms, at ${Math.round(performance.timeOrigin + performance.now()) % 1e7}`);
    this.#sharedModules.set(name, mod);
    this.#env.shareModule?.(name, mod);
    return mod;
  }

  /** This Worker's function table and runtime instance, made at the first
   *  unit: every slot holds `resolve` until something is installed there,
   *  index 1 the shim's fallback. `gex.lookup` catches this Worker up on
   *  the table's log before asking the shim. */
  #cgenStore() {
    if (this.#cgen) return this.#cgen;
    const e = this.#shim.exports;
    const tables = this.#dispatchTables();
    let rm = this.#sharedModules.get('ftab-runtime');
    if (!rm) {
      const ptr = e.gex_ftab_runtime_ptr() >>> 0;
      rm = new WebAssembly.Module(this.#bytes().slice(ptr, ptr + (e.gex_ftab_runtime_len() >>> 0)));
      this.#sharedModules.set('ftab-runtime', rm);
    }
    const ftab = new WebAssembly.Table({ element: 'anyfunc', initial: 0 });
    const lookup = (ctx, addr) => {
      if (e.gex_ftab_behind()) this.#cgenCatchUp();
      const idx = e.gex_ftab_lookup(ctx, addr);
      // A translation the function JIT just published: in this Worker's
      // table before `resolve` calls it.
      if (e.gex_ftab_behind()) this.#cgenCatchUp();
      return idx;
    };
    const gex = {
      memory: this.#memory,
      ftab,
      lookup,
      safepoint: this.#imports.safepoint,
      cache_op: this.#imports.cache_op,
      dispatch_miss: this.#imports.dispatch_miss,
    };
    for (let i = 0; i < TABLE_NAMES.length; i++) {
      gex[TABLE_NAMES[i]] = tables[i] ?? new WebAssembly.Table({ element: 'anyfunc', initial: 0 });
    }
    // Dense: the shim's runtime module dispatches through `blocks` too.
    if (this.#dense) gex.blocks = this.#guest.exports.blocks;
    const ri = new WebAssembly.Instance(rm, { gex });
    this.#cgen = { ftab, resolve: ri.exports.resolve, bridge: ri.exports.bridge, lookup, have: 0, instances: [ri] };
    this.#cgenGrow();
    ftab.set(1, e.gex_ftab_fallback);
    return this.#cgen;
  }

  /** Grow this Worker's function table past every index handed out, with
   *  room over, so an index another Worker published to the probe cache is
   *  in range here (holding `resolve`) before this one replays it. */
  #cgenGrow() {
    const st = this.#cgen;
    const want = (this.#shim.exports.gex_ftab_size() >>> 0) + 65536;
    if (st.ftab.length < want) st.ftab.grow(want - st.ftab.length, st.resolve);
  }

  /** Install unit `u` into this Worker: instantiate it with `__table_base` =
   *  `base` (its element segment fills that range of the function table),
   *  then point tier-1's dispatch slots for its entries at the bridge. */
  #cgenInstall(u, base) {
    const e = this.#shim.exports;
    const st = this.#cgenStore();
    // An export provider's module (`PROVIDER | number`, cgen.rs) installs as
    // a unit does, from its file in the unit cache.
    const prov = u >= 0x80000000 ? u - 0x80000000 : -1;
    // A function JIT translation (`FJIT | n`, fjit.rs): its bytes are the shim's.
    const jit = prov < 0 && u >= 0x40000000 ? u - 0x40000000 : -1;
    // Wine's PE module (native wasm32 Wine, cgen/wasmdll.rs): its bytes are
    // in the shim's heap, read from the prefix.
    const wasmdll = prov >= 0 && (e.gex_cgen_pmod_wasm_len?.(prov) >>> 0) > 0;
    // A native wasm32 program (cgen/wasmprog.rs): its bytes come the same way.
    const wasmprog = wasmdll && !!e.gex_cgen_pmod_wasmprog?.(prov);
    const mod = wasmdll
      ? this.#cgenFileModule(this.#cgenStr(e.gex_cgen_pmod_file, prov),
        new Uint8Array(this.#memory.buffer, e.gex_cgen_pmod_wasm_ptr(prov) >>> 0, e.gex_cgen_pmod_wasm_len(prov) >>> 0).slice())
      : prov >= 0
      ? this.#cgenFileModule(this.#cgenStr(e.gex_cgen_pmod_file, prov))
      : jit >= 0 ? this.#fjitModule(jit)
      : this.#cgenModule(this.#cgenStr(e.gex_cgen_key, u));
    // A stateful provider (docs/eject-pe.md §5.1): its data in the guest's
    // memory, this Worker's provider stack, and the provider_* imports.
    const memBase = prov >= 0 ? (e.gex_cgen_pmod_memory_base?.(prov) >>> 0) : 0;
    const regsOf = prov >= 0 ? (b, c) => e.gex_cgen_pmod_regs(prov, b, c)
      : jit >= 0 ? (b, c) => e.gex_fjit_regs(jit, b, c)
      : (b, c) => e.gex_cgen_regs(u, b, c);
    const tables = this.#growTables((prov >= 0 ? e.gex_cgen_pmod_slots(prov) : jit >= 0 ? e.gex_fjit_slots(jit) : e.gex_cgen_slots(u)) >>> 0, 'cgen');
    const inst = new WebAssembly.Instance(mod, {
      env: {
        memory: this.#memory,
        __indirect_function_table: st.ftab,
        __table_base: new WebAssembly.Global({ value: 'i32', mutable: false }, base | 0),
        __memory_base: new WebAssembly.Global({ value: 'i32', mutable: false }, memBase),
        // A unit's functions never use clang's shadow stack (those are stubs),
        // but a position-independent module imports its pointer all the same.
        // A stateful provider's is this Worker's (one C stack per thread).
        // A wasm32 program sets its own at each thread's entry.
        __stack_pointer: memBase && !wasmprog ? this.#providerStack() : new WebAssembly.Global({ value: 'i32', mutable: true }, 0),
      },
      gex: {
        ...this.#imports, lookup: st.lookup, ftab: st.ftab,
        // Built-in providers' host functions (gex-cgen/providers): nt_qpc's
        // clock, crt_heap's pages.
        ...(prov >= 0 ? {
          // The shim's exports themselves: a wasm-to-wasm call, no JS.
          nt_qpc: e.gex_nt_qpc,
          provider_alloc_top: e.gex_provider_alloc_top,
          // Wine's Unix side (W3.1): its system-call dispatcher and fault
          // hook, process-wide in the shim (the page's delivery of faults
          // is still to come).
          unix_dispatcher: (op, idx) => e.gex_unix_dispatcher?.(op, idx) ?? 0,
          fault_hook: (idx) => e.gex_fault_hook_set?.(idx),
          signal_hook: (idx) => e.gex_signal_hook_set?.(idx),
          code_changed: (addr, len) => e.gex_code_changed?.(addr, len),
          wine_pe_slot: (ptr, len) => e.gex_wine_pe_slot?.(ptr, len) ?? 0,
        } : {}),
        ...(memBase && !wasmprog ? this.#providerImports() : {}),
        ...(wasmprog ? this.#wasmprogImports(() => inst) : wasmdll ? { syscall: this.#nativeSyscall } : {}),
      },
    });
    if (wasmprog ? e.gex_wasmprog_relocate(prov) : memBase && e.gex_provider_relocate(memBase)) inst.exports.__wasm_apply_data_relocs?.();
    const n = regsOf(0, 0) >>> 0;
    const buf = e.gex_alloc(4 * Math.max(1, n)) >>> 0;
    let regs;
    try {
      regsOf(buf, n);
      regs = new Uint32Array(this.#memory.buffer.slice(buf, buf + 4 * n));
    } finally {
      e.gex_free?.(buf, 4 * Math.max(1, n));
    }
    // Table 8 (`gex_emit::dense::TABLE_INDEX`) is `blocks`: a dense slot's
    // bridge has an index there of its own.
    const blocks = this.#guest.exports.blocks;
    if (blocks) this.#growBlocks();
    for (let j = 0; j + 1 < regs.length; j += 2) {
      (regs[j] === TABLE_NAMES.length ? blocks : tables[regs[j]]).set(regs[j + 1], st.bridge);
    }
    st.instances.push(inst);
    // The first Worker to install a JIT module writes the directory for the
    // pages it numbered (idempotent).
    if (jit >= 0) e.gex_fjit_installed(jit);
    return regs.length / 2;
  }

  /** This Worker's stack for stateful providers (one per guest thread, as
   *  natively), made at the first one's instantiation. */
  #provStack = null;
  #providerStack() {
    if (!this.#provStack) {
      const size = 1 << 20;
      // Top down unless an ejected project's providers are set (cgen.rs).
      const e = this.#shim.exports;
      const base = (e.gex_provider_stack ? e.gex_provider_stack(size) : e.gex_provider_alloc(size)) >>> 0;
      if (!base) throw new Error('gex-nt: no memory for a provider stack');
      this.#provStack = new WebAssembly.Global({ value: 'i32', mutable: true }, base + size - 16);
    }
    return this.#provStack;
  }

  /** What a stateful provider imports from `gex` (the native `libprov`). */
  #providerImports() {
    const e = this.#shim.exports;
    const ftab = this.#cgen.ftab;
    return {
      // A protected call: `fn(arg)` from the function table, 0, or the code
      // a provider_throw inside it threw (every frame between discarded, as
      // a C longjmp).
      provider_try: (f, arg) => {
        try {
          ftab.get(f >>> 0)(arg);
          e.gex_provider_note(0);
          return 0;
        } catch (err) {
          if (err instanceof ProviderThrow) {
            e.gex_provider_note(1);
            return err.code;
          }
          throw err;
        }
      },
      provider_throw: (code) => {
        throw new ProviderThrow(code);
      },
      provider_ctx: () => e.gex_provider_ctx(),
      provider_time: (which) => (which === 0 ? Date.now() / 1000 : performance.now() / 1000),
      provider_missing: (addr) => e.gex_provider_missing(addr),
      provider_alloc: (size) => e.gex_provider_alloc(size),
      provider_alloc_top: (size) => e.gex_provider_alloc_top(size),
      provider_log: (ptr, len) => {
        console.error(new TextDecoder().decode(this.#bytes().slice(ptr >>> 0, (ptr >>> 0) + (len >>> 0))));
      },
    };
  }

  /** A module file from the unit cache (an export provider's), compiled
   *  once per process and offered to the siblings. Synchronous. */
  #cgenFileModule(file, bytes = null) {
    const name = `cgenfile:${file}`;
    let mod = this.#sharedModules.get(name);
    if (mod) return mod;
    bytes ??= this.#env.loadFile?.(file) ?? null;
    if (!bytes) throw new Error(`gex-cgen: ${file} was published but cannot be loaded here`);
    mod = new WebAssembly.Module(bytes);
    this.#sharedModules.set(name, mod);
    this.#env.shareModule?.(name, mod);
    return mod;
  }

  /** Bind unit `u`'s export providers (§9.9), after the unit, whether or
   *  not it had one: each provider's module from the unit cache, published
   *  by the shim at its exports' addresses. */
  #cgenProviders(u) {
    const e = this.#shim.exports;
    const n = e.gex_cgen_provider_count?.(u) >>> 0;
    for (let i = 0; i < n; i++) {
      if (e.gex_cgen_provider_wasmdll?.(u, i)) {
        const number = e.gex_wasmdll_publish(u, i);
        this.#cgenCatchUp();
        if (number >= 0) {
          e.gex_cgen_pmod_installed(number);
          e.gex_wasmdll_installed();
        }
        this.#wasmdllCatchUp();
        continue;
      }
      const file = this.#cgenStr((uu, b, c) => e.gex_cgen_provider_file(uu, i, b, c), u);
      const bytes = this.#env.loadFile?.(file) ?? null;
      if (!bytes) {
        console.error(`gex-cgen: no provider module ${file} in the unit cache (build it natively: a run of the program, or gexc cgen-prefix)`);
        continue;
      }
      const b = bytes instanceof Uint8Array ? bytes : new Uint8Array(bytes);
      this.#cgenFileModule(file, b);
      const gp = e.gex_alloc(Math.max(8, b.byteLength)) >>> 0;
      this.#bytes().set(b, gp);
      const number = e.gex_cgen_provider_publish(u, i, gp, b.byteLength);
      e.gex_free?.(gp, Math.max(8, b.byteLength));
      if (number < 0) continue;
      this.#cgenCatchUp();
      e.gex_cgen_pmod_installed(number);
    }
  }

  /** An ejected project's region entries for unit `u` (docs/eject-pe.md
   *  §3.3): `<cache>/regions/<key>/`, published after the unit as a unit of
   *  their own, when the build made them. */
  #cgenRegions(u, path) {
    const e = this.#shim.exports;
    if (!e.gex_cgen_region) return;
    const r = e.gex_cgen_region(u);
    if (r < 0) return;
    const key = this.#cgenStr(e.gex_cgen_key, r);
    const got = this.#env.loadUnit?.(key) ?? null;
    if (!got) return;
    this.#cgenModule(key, got);
    const ent = got.entries instanceof Uint8Array ? got.entries : new Uint8Array(got.entries);
    const gp = e.gex_alloc(Math.max(8, ent.byteLength)) >>> 0;
    this.#bytes().set(ent, gp);
    const res = e.gex_cgen_publish(r, gp, ent.byteLength / 8);
    e.gex_free?.(gp, Math.max(8, ent.byteLength));
    if (res < 0) throw new Error(`gex-cgen: publishing ${path}'s region entries failed`);
    this.#cgenCatchUp();
    e.gex_cgen_installed(r);
    let n = 0;
    for (let i = 0; i + 8 <= ent.byteLength; i += 8) if (ent[i] | ent[i + 1] | ent[i + 2] | ent[i + 3] | ent[i + 4] | ent[i + 5] | ent[i + 6] | ent[i + 7]) n++;
    console.error(`gex-nt: region entries for ${path}: ${n}`);
  }

  /** Function JIT module `n` (fjit.rs), compiled once per process from the
   *  shim's bytes and offered to the siblings. Synchronous (a Worker). */
  #fjitModule(n) {
    const name = `fjit:${n}`;
    let mod = this.#sharedModules.get(name);
    if (mod) return mod;
    const e = this.#shim.exports;
    const ptr = e.gex_fjit_ptr(n) >>> 0;
    const bytes = this.#bytes().slice(ptr, ptr + (e.gex_fjit_len(n) >>> 0));
    const dump = globalThis.process?.env?.GEX_FJIT_DUMP;
    if (dump) globalThis.process.getBuiltinModule('fs').writeFileSync(`${dump}/fjit-${n}.wasm`, bytes);
    const t0 = performance.now();
    mod = new WebAssembly.Module(bytes);
    this.#compileMs += performance.now() - t0;
    this.#sharedModules.set(name, mod);
    this.#env.shareModule?.(name, mod);
    return mod;
  }

  /** Replay the function table's log this Worker has not seen: installs,
   *  and retirements (back to `resolve`). */
  #cgenCatchUp() {
    const e = this.#shim.exports;
    if (!e.gex_ftab_log_len) return;
    const len = e.gex_ftab_log_len() >>> 0;
    if (!len && !this.#cgen) return;
    const st = this.#cgenStore();
    if (st.have >= len) {
      this.#wasmdllCatchUp();
      return;
    }
    this.#cgenGrow();
    while (st.have < len) {
      const ev = BigInt.asUintN(64, BigInt(e.gex_ftab_event(st.have)));
      const kind = Number(ev >> 32n), v = Number(ev & 0xffffffffn);
      if (kind === 1) this.#cgenInstall(e.gex_ftab_install_unit(v) >>> 0, e.gex_ftab_install_base(v) >>> 0);
      else if (kind === 2) st.ftab.set(v, st.resolve);
      st.have++;
      e.gex_cgen_have(st.have);
    }
    this.#wasmdllCatchUp();
  }

  /** The bridges of exports bound into Wine's PE module after its install
   *  (a later skeleton, the entry thunks: cgen/wasmdll.rs), each once. */
  #wasmdllHave = 0;
  #wasmdllCatchUp() {
    const e = this.#shim.exports;
    const len = (e.gex_wasmdll_binds_len?.() ?? 0) >>> 0;
    if (this.#wasmdllHave >= len) return;
    const blocks = this.#guest.exports.blocks;
    while (this.#wasmdllHave < len) {
      const i = this.#wasmdllHave;
      const tables = this.#growTables(e.gex_wasmdll_bind_slots(i) >>> 0, 'cgen');
      if (blocks) this.#growBlocks();
      const n = e.gex_wasmdll_bind_regs(i, 0, 0) >>> 0;
      const buf = e.gex_alloc(4 * Math.max(1, n)) >>> 0;
      let regs;
      try {
        e.gex_wasmdll_bind_regs(i, buf, n);
        regs = new Uint32Array(this.#memory.buffer.slice(buf, buf + 4 * n));
      } finally {
        e.gex_free?.(buf, 4 * Math.max(1, n));
      }
      const st = this.#cgenStore();
      for (let j = 0; j + 1 < regs.length; j += 2) {
        (regs[j] === TABLE_NAMES.length ? blocks : tables[regs[j]]).set(regs[j + 1], st.bridge);
      }
      e.gex_wasmdll_bind_done(i);
      this.#wasmdllHave++;
    }
  }

  /** What a native wasm32 program imports from `gex` (cgen/wasmprog.rs):
   *  its system calls, which return unless the program's frames end (a
   *  thread or a child they staged is handed to the host here), and fork's
   *  asyncify operations on its own instance (the musl fork's
   *  src/process/wasm32/__gex_fork.c). */
  #wasmprogImports(inst) {
    const e = this.#shim.exports;
    const ops = ['asyncify_get_state', 'asyncify_start_unwind', 'asyncify_stop_unwind', 'asyncify_start_rewind', 'asyncify_stop_rewind'];
    const asyncify = (op, arg) => {
      const f = inst().exports[ops[op]];
      if (!f) {
        if (op === 0) return 0;
        throw new Error(`gex.asyncify: the program has no ${ops[op]} (fork needs wcc's asyncify)`);
      }
      if (op === 0) return f();
      if (op === 1 || op === 3) f(arg);
      else f();
      return 0;
    };
    return {
      syscall: (ctx, pc) => {
        e.gex_syscall(ctx, pc);
        if (e.gex_wasmprog_staged()) {
          this.#drainFork().catch((err) => console.error(`[gex] starting a process: ${err.message ?? err}`));
          this.#drainThread().catch((err) => console.error(`[gex] starting a thread: ${err.message ?? err}`));
        }
      },
      asyncify,
      fork_point: (rec) => {
        const st = asyncify(0, 0);
        if (st === 0) asyncify(1, rec);
        else if (st === 2) asyncify(4, 0);
        else throw new Error(`gex.fork_point in asyncify state ${st}`);
      },
    };
  }

  /** A native wasm32 program the shim staged (a load, an exec, a fork's
   *  child): published and installed here before the guest runs. */
  #wasmprogServe() {
    const e = this.#shim.exports;
    const n = e.gex_wasmprog_serve?.() ?? -1;
    if (n < 0) return;
    this.#cgenCatchUp();
    e.gex_cgen_pmod_installed(n);
  }

  /** Wine's PE module's `gex.syscall` (cgen/wasmdll.rs): a system call from
   *  native code (gexrt marks its pc, bit 61) that traps to JS -- a unit to
   *  install, a thread to start -- is served here and returns, so the native
   *  frames below it survive, as natively. Anything else (the process or
   *  thread ending, a callback's return) goes on up. */
  #nativeSyscall = (ctx, pc) => {
    const e = this.#shim.exports;
    const p = BigInt.asUintN(64, BigInt(pc));
    if (!(p & (1n << 61n))) return e.gex_syscall(ctx, pc);
    try {
      return e.gex_syscall(ctx, pc);
    } catch (err) {
      if (err?.gexHandoff || !e.gex_handoff()) throw err;
      err.gexHandoff = true;
      if (e.gex_exited?.()) throw err;
      // gex_nr::NATIVE_LEFT: the call-out it is nested in decides.
      if (e.gex_nt_native_left?.()) {
        err.gexNativeLeft = true;
        throw err;
      }
      const stop = p & ~(1n << 61n);
      const next = BigInt.asUintN(64, BigInt(e.gex_resume_pc()));
      const rc = e.gex_resume_ctx() >>> 0;
      if (next !== stop || (rc && rc !== (ctx >>> 0))) throw err;
      if (e.gex_jit_pending_len() >>> 0) {
        const need = (e.gex_jit_pending_slots?.() ?? 0) >>> 0;
        if (need) this.#growTables(need, 'call-out');
      }
      while (e.gex_jit_pending_len() >>> 0) this.#installTranslationSync();
      this.#catchUpSync();
      this.#cgenServe();
      this.#drainThread().catch((err2) => console.error(`[gex] starting a thread: ${err2.message ?? err2}`));
    }
  };

  /** Load and publish the units the shim placed (images mapped since the last
   *  trap), then catch up. A unit's bytes never pass through the shim: only
   *  its entry list does. */
  #cgenServe() {
    const e = this.#shim.exports;
    this.#wasmprogServe();
    // ELF libraries' export providers (cgen/elfprov.rs): provider-only units,
    // bound before the guest goes on (and, in a forked child, before it
    // first runs: its table starts empty while its GOT is its parent's).
    if (e.gex_elfprov_wanted) {
      let any = false;
      for (let u = e.gex_elfprov_wanted(); u >= 0; u = e.gex_elfprov_wanted()) {
        this.#cgenProviders(u);
        any = true;
      }
      if (any) this.#cgenCatchUp();
      // Slots of provided addresses that lost their bridge (a forked child
      // instantiating a module it inherited): the bridge again.
      const n = e.gex_elfprov_rebridge_len() >>> 0;
      if (n) {
        this.#cgenCatchUp();
        const st = this.#cgenStore();
        const buf = e.gex_alloc(4 * n) >>> 0;
        let regs;
        try {
          const k = e.gex_elfprov_rebridge(buf, n) >>> 0;
          regs = new Uint32Array(this.#memory.buffer.slice(buf, buf + 4 * k));
        } finally {
          e.gex_free?.(buf, 4 * n);
        }
        const tables = this.#growTables(e.gex_elfprov_rebridge_need() >>> 0, 'elfprov');
        const blocks = this.#guest.exports.blocks;
        if (blocks) this.#growBlocks();
        for (let j = 0; j + 1 < regs.length; j += 2) {
          (regs[j] === TABLE_NAMES.length ? blocks : tables[regs[j]]).set(regs[j + 1], st.bridge);
        }
      }
    }
    // A Worker with the function JIT on but no units of its own still
    // replays the function table's log.
    if (!this.#cgenMode && e.gex_fjit_on?.()) {
      this.#cgenCatchUp();
      return;
    }
    if (!this.#cgenMode || !e.gex_cgen_wanted) return;
    for (let u = e.gex_cgen_wanted(); u >= 0; u = e.gex_cgen_wanted()) {
      const t0 = performance.now();
      // The image's indirect-call profile is part of its unit's key, as
      // natively: read it from the unit cache first.
      if (e.gex_cgen_icall_path) {
        const prof = this.#env.loadFile?.(this.#cgenStr(e.gex_cgen_icall_path, u)) ?? null;
        const b = prof ? (prof instanceof Uint8Array ? prof : new Uint8Array(prof)) : null;
        const gp = b ? e.gex_alloc(Math.max(8, b.byteLength)) >>> 0 : 0;
        if (b) this.#bytes().set(b, gp);
        e.gex_cgen_set_icall(u, gp, b ? b.byteLength : 0);
        if (b) e.gex_free?.(gp, Math.max(8, b.byteLength));
      }
      const key = this.#cgenStr(e.gex_cgen_key, u);
      const path = this.#cgenStr(e.gex_cgen_path, u);
      // A link group's member (docs/eject-pe.md §6a): held until every
      // member is placed, then the group's module stands for them all.
      const grp = e.gex_cgen_group ? this.#cgenGroupOf(key) : null;
      if (grp) {
        e.gex_cgen_hold(u);
        grp.held.set(key, { u, path });
        console.error(`gex-nt: ${path}: held for its link group`);
        if (grp.held.size === grp.members.length) this.#cgenGroup(grp, t0);
        continue;
      }
      this.#cgenUnit(u, key, path, t0);
    }
    this.#cgenCatchUp();
  }

  /** Load and publish unit `u` (`key`), then its providers and region
   *  entries. */
  #cgenUnit(u, key, path, t0) {
    const e = this.#shim.exports;
    const got = this.#env.loadUnit?.(key) ?? null;
    if (!got) {
      if (e.gex_cgen_missing(u)) throw new Error(`gex-cgen: --cgen: ${path} has no function-tier unit (${key})`);
      this.#cgenProviders(u);
      return;
    }
    this.#cgenPublish(u, key, path, got);
    this.#cgenProviders(u);
    console.error(`gex-nt: function tier for ${path}: ${got.entries.byteLength / 8} entries, placed in ${((performance.now() - t0) / 1000).toFixed(1)}s`);
    this.#cgenRegions(u, path);
  }

  /** Publish unit `u`'s module (`got`, under `key`) and install it here. */
  #cgenPublish(u, key, path, got) {
    const e = this.#shim.exports;
    this.#cgenModule(key, got);
    const ent = got.entries instanceof Uint8Array ? got.entries : new Uint8Array(got.entries);
    const gp = e.gex_alloc(Math.max(8, ent.byteLength)) >>> 0;
    this.#bytes().set(ent, gp);
    const r = e.gex_cgen_publish(u, gp, ent.byteLength / 8);
    e.gex_free?.(gp, Math.max(8, ent.byteLength));
    if (r < 0) throw new Error(`gex-cgen: publishing ${path}'s unit failed`);
    this.#cgenCatchUp();
    e.gex_cgen_installed(u);
  }

  /** The unit cache's link groups (`groups/index.txt`: a group key, then its
   *  members' unit keys, per line), by member key; read once. */
  #cgenGroups = null;
  #cgenGroupOf(key) {
    if (!this.#cgenGroups) {
      this.#cgenGroups = new Map();
      const b = this.#env.loadFile?.('groups/index.txt') ?? null;
      const text = b ? new TextDecoder().decode(b) : '';
      for (const line of text.split('\n')) {
        const [gkey, ...members] = line.trim().split(/\s+/);
        if (!gkey || !members.length) continue;
        const g = { key: gkey, members, held: new Map(), done: false };
        for (const m of members) this.#cgenGroups.set(m, g);
      }
    }
    const g = this.#cgenGroups.get(key);
    return g && !g.done ? g : null;
  }

  /** Every member of `grp` is placed: publish the group's module in place
   *  of their units (or, without one, each member's own), then each
   *  member's providers and region entries. */
  #cgenGroup(grp, t0) {
    const e = this.#shim.exports;
    grp.done = true;
    const ms = grp.members.map((k) => ({ key: k, ...grp.held.get(k) }));
    const got = this.#env.loadUnit?.(`groups/${grp.key}`) ?? null;
    if (!got) {
      console.error(`gex-nt: link group ${grp.key} is not in the unit cache; its members' own units`);
      for (const m of ms) this.#cgenUnit(m.u, m.key, m.path, t0);
      return;
    }
    const us = new Uint32Array(ms.map((m) => m.u));
    const kb = new TextEncoder().encode(grp.key);
    const up = e.gex_alloc(Math.max(8, us.byteLength)) >>> 0;
    const kp = e.gex_alloc(Math.max(8, kb.byteLength)) >>> 0;
    this.#bytes().set(new Uint8Array(us.buffer), up);
    this.#bytes().set(kb, kp);
    const g = e.gex_cgen_group(up, us.length, kp, kb.byteLength);
    e.gex_free?.(up, Math.max(8, us.byteLength));
    e.gex_free?.(kp, Math.max(8, kb.byteLength));
    if (g < 0) throw new Error(`gex-cgen: link group ${grp.key}: no unit`);
    const path = this.#cgenStr(e.gex_cgen_path, g);
    this.#cgenPublish(g, `groups/${grp.key}`, path, got);
    console.error(`gex-nt: ${path} (${grp.key}): ${got.entries.byteLength / 8} entries, placed in ${((performance.now() - t0) / 1000).toFixed(1)}s`);
    for (const m of ms) {
      this.#cgenProviders(m.u);
      this.#cgenRegions(m.u, m.path);
    }
  }

  useGl(gl) {
    this.#gl = gl;
    this.#shim?.exports?.gex_set_gl?.(gl ? 1 : 0);
  }

  /**
   * Give the guest a Wayland compositor. `wl` is a `WlProxy` (wl-bridge.mjs),
   * this process's window into the shared scene arena the compositor worker
   * draws from. Like `useGl`, best set before `load` -- the socket is bound
   * while the filesystem is built.
   */
  useWl(wl) {
    this.#wl = wl;
    this.#shim?.exports?.gex_set_wl?.(wl ? 1 : 0);
  }

  /** Say how large the compositor's output is, before the guest runs. */
  wlOutput(w, h) {
    this.#shim?.exports?.gex_set_wl_output?.(w | 0, h | 0);
  }

  /**
   * This process's name in the compositor's scene arena: its pid, which every
   * Worker of the process agrees on and no other process shares. Zero before
   * the process has one, so callers ask again rather than caching it.
   */
  wlToken() {
    return this.#shim?.exports?.gex_wl_token?.() ?? 0;
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
    // When the last trap came out, for GEX_NT_STATS' time split: what the
    // glue costs between a trap and the guest again.
    let trappedAt = 0;
    // Units for what booting mapped (the exe, ntdll), before the guest runs.
    this.#cgenServe();
    for (;;) {
      if (trappedAt) {
        this.#shim.exports.gex_nt_note_js_ns?.(ctx, (performance.now() - trappedAt) * 1e6);
        trappedAt = 0;
      }
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
        trappedAt = performance.now();
        // First, before asking the shim anything: was this a handoff or a
        // crash? Every way out is the same `unreachable` to the engine, and
        // treating a crash as a handoff reported a crashed runtime as a clean
        // exit 0. `trap()` sets a flag; nothing else does. First because the
        // recovery below calls into the shim, whose kernel a crash may leave
        // borrowed -- a second call would bury the first error.
        if (!err?.gexHandoff && this.#shim.exports.gex_handoff && !this.#shim.exports.gex_handoff()) {
          // RECOVERABLE first: a "table index is out of bounds" during dispatch
          // is usually just this Worker's funcref table lagging the shared
          // directory. Another Worker numbered a page (bumping the process-wide
          // CLAIM_NEXT and growing ITS table) while ours had not caught up, so
          // the directory names a slot past the end of our table and
          // `return_call_indirect` trapped. Grow to the claimed high-water and
          // retry the same dispatch: the slot then holds the block (if we filled
          // it) or the miss stub (a normal miss that fills it). Only retry when
          // the grow made progress and only so many times, so a genuine over-run
          // still fails loudly. This is the concurrent-JIT table-growth race that
          // stranded threads under heavy multithread translation (gtk3-demo, and
          // the jit-storm repro). See gex_claimed_slots.
          const eR = this.#shim.exports;
          if (/table index is out of bounds/.test(err.message) && eR.gex_last_target) {
            // This thread's last target, read from its own context: the
            // shim's answer was once another thread's.
            let tgt = 0n;
            try { tgt = new DataView(this.#memory.buffer).getBigUint64(ctx + CTX_LAST_TARGET, true); } catch { tgt = 0n; }
            const grew = this.#growTablesToClaimed();
            if (grew && (globalThis.GEX_TABLE_DEBUG || (typeof process !== 'undefined' && process.env?.GEX_TABLE_DEBUG)))
              console.error(`[tblrec] caught up tables for a sibling-numbered page (retry ${this.#tableRetries})`);
            if (tgt && this.#tableRetries < 100000 && grew) {
              this.#tableRetries++;
              pc = tgt < 0n ? tgt + (1n << 64n) : tgt;
              continue;
            }
          }
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
          const lr = ask(e.gex_current_lr, (lr) =>
            `, lr=0x${(lr < 0n ? lr + (1n << 64n) : lr).toString(16)}`);
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
          // Physical size of the linear memory at the trap: a "memory access out
          // of bounds" fires only past this, so it is the ceiling the faulting
          // address crossed -- a stack whose mmap did not grow the memory, or a
          // wild pointer beyond it.
          try {
            console.error(`[trap] linear memory grown to ${(this.#memory.buffer.byteLength / (1024 * 1024)) | 0} MiB (0x${this.#memory.buffer.byteLength.toString(16)})`);
          } catch {}
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
            const at = slot + ((Number(guest) & 0xfff) >> SLOT_SHIFT);
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
              if (t < tables.length && b + SLOTS_PER_PAGE <= cap) continue;
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
            `the gex runtime trapped inside the shim (${err.message}${at}${lr}${nr}` +
              `${code}${heap}${where}). That is a crash in the runtime, not a guest exit; ` +
              'anything printed above is all the guest managed to do first.',
          );
        }
        // A trap abandons the engine's frames without running the epilogues
        // that restore the shim's shadow-stack global, leaking a few dozen
        // bytes each. Nothing is live below here, so reset it exactly -- left
        // alone it slid into the guest's stack and overwrote saved registers.
        this.#shim.exports.__stack_pointer.value = addrVal(this.#workerStack());
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
        this.#denseCatchUp();
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
        this.#cgenServe();
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
        // `exit_group` marks every guest thread exited and deliberately leaves
        // no resume PC. Do not inspect the wait state that thread had before
        // the exit: a network/futex flag can still be set, and waiting on it
        // here strands an already-dead process so its parent can never reap it.
        if (e.gex_exited?.()) return e.gex_exit_code();
        // The guest gave the stack back unfinished: every thread waits on
        // something only the page can produce. Waiting happens out here because
        // a guest holding the one wasm stack is a guest nothing can reach. The
        // loop matters: one event-loop turn is no promise the wait is over.
        if (e.gex_paused_for_net() || e.gex_paused_for_input()) {
          let woke = 0;
          while (e.gex_paused_for_net() || e.gex_paused_for_input()) {
            // Another Worker can execute exit_group while this one is parked.
            // The global-kernel wake it generates is not a resume request for
            // this dead thread, so observe the process flag before consuming it.
            if (e.gex_exited?.()) return e.gex_exit_code();
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
            if (e.gex_exited?.()) return e.gex_exit_code();
            woke = e.gex_wake();
            if (woke === 1) break;
          }
          if (e.gex_exited?.()) return e.gex_exit_code();
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
              // Dump every thread's wait-state + pc so a give-up says WHICH
              // thread is stuck on WHAT (futex vs socket read vs exited), not
              // just that nothing moved. Prints via the shim; safe if borrowed.
              try { e.gex_dump_threads?.(); } catch {}
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
          // A handoff can race a sibling wake: the first scheduling pass sees
          // no owned thread ready, then a sibling makes this Worker's thread
          // runnable before JS observes the pause mask. Do one final scheduler
          // pass before declaring the process abandoned. Without this, a
          // perfectly valid runnable thread is discarded with pc=0.
          if (!e.gex_exited?.() && e.gex_wake && e.gex_resume_pc) {
            const scheduled = e.gex_wake();
            const resume = e.gex_resume_pc();
            if (scheduled === 1 && resume !== 0n) {
              pc = resume < 0n ? resume + (1n << 64n) : resume;
              const resumeCtx = e.gex_resume_ctx?.() >>> 0;
              if (resumeCtx) ctx = resumeCtx;
              continue;
            }
            if (globalThis.GEX_REPORT_ABANDONED) {
              console.error(
                `[gex] final handoff recovery found scheduler=${scheduled}, resume=0x${
                  (resume < 0n ? resume + (1n << 64n) : resume).toString(16)
                }`,
              );
            }
          }
          // The ordinary end: the shim set an exit status and abandoned the
          // stack. `enter` never returns then, so this is the path a finished
          // program takes.
          // An abandoned translated stack is always a crash and must explain
          // itself. Keep successful exits quiet unless full debug was asked
          // for, but never hide the only useful PC/wasm-stack diagnostics.
          if ((!e.gex_exited?.() && globalThis.GEX_REPORT_ABANDONED) || globalThis.GEX_DEBUG_EXIT) {
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
              try { e.gex_dump_threads?.(); } catch {}
              try { e.gex_dump_state?.(); } catch {}
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
      if (this.#denseTable(i)) continue;
      const need_i = need_of(i);
      if (tables[i].length >= need_i) continue;
      // Geometrically, not exactly: `Table.grow` reallocates and copies, so
      // growing by exactly what each module wants copies an ever-larger table
      // once per module. Doubling makes total copying linear in the final size.
      // Bounded: doubling a table of millions asks V8 for one FixedArray of
      // tens of megabytes per table per Worker, and at Noita's world load
      // that request killed the renderer (V8 OOM with an 87 MB heap). A
      // million entries (8 MB) at most beyond what is needed.
      const len = tables[i].length;
      const want = Math.min(Math.max(need_i, Math.min(len * 2, len + TABLE_STEP)), per);
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
        growTable(tables[i], want - tables[i].length, stub, i);
      } catch (err) {
        throw new Error(
          `dispatch table ${i} would not grow from ${tables[i].length} to ${want}: ${err.message}`,
        );
      }
    }
    return tables;
  }

  /**
   * Grow this Worker's funcref tables to the process-wide claimed high-water,
   * so a page a sibling Worker numbered (and grew ITS table for) can be
   * dispatched here without `return_call_indirect` running off the end. Returns
   * true if anything grew -- the run loop only retries a "table index out of
   * bounds" when this made progress, so a genuine over-run still fails loudly.
   * Cheap when nothing changed: a size read and a compare per table.
   */
  #growTablesToClaimed() {
    const e = this.#shim.exports;
    if (!e.gex_claimed_slots) return false;
    const tables = this.#dispatchTables();
    const stub = tables[0].get(0);
    let grew = this.#growBlocks();
    for (let i = 0; i < tables.length; i++) {
      if (this.#denseTable(i)) continue;
      const need = e.gex_claimed_slots(i) >>> 0;
      if (need <= tables[i].length) continue;
      try {
        growTable(tables[i], need - tables[i].length, stub, i);
        grew = true;
      } catch {
        /* a table that will not grow is reported by the caller's retry cap */
      }
    }
    return grew;
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
  /**
   * A user-mode callback of a Windows program (docs/nt-layer.md §5): run the
   * guest from `pc` on this Worker, nested inside the `syscall` import that
   * asked, until the thread's `NtCallbackReturn`. Synchronous throughout --
   * there is a shim frame below this one, so nothing may yield -- which is
   * why translations are compiled with `new WebAssembly.Module` here (legal
   * in a Worker at any size). Returns 1 when the callback returned, 0 when
   * the run failed or the process ended.
   */
  #ntCallback(ctx, pc) {
    this.#nest.push('callback');
    try {
      return this.#ntCallbackLoop(ctx, pc);
    } finally {
      this.#nest.pop();
    }
  }

  #ntCallbackLoop(ctx, pc) {
    const e = this.#shim.exports;
    // The shim's frames below this import are live: every trap out of the
    // nested guest abandons only the frames above here, so the shadow stack
    // goes back to exactly this, never to the Worker's base.
    const sp = e.__stack_pointer.value;
    pc = pc < 0n ? pc + (1n << 64n) : pc;
    let bypass = null;
    for (;;) {
      try {
        if (bypass) {
          const f = bypass;
          bypass = null;
          f(ctx);
        } else {
          this.#guest.exports.enter(ctx, pc);
        }
        console.error('[gex-nt] guest code returned out of a user-mode callback');
        return 0;
      } catch (err) {
        e.__stack_pointer.value = sp;
        // A call-out's watched entry reached inside the callback: what the
        // entry held runs (`#watchHit` never ends a call-out from here).
        const sig = this.#calloutSignal;
        this.#calloutSignal = null;
        if (sig?.kind === 'bypass' && sig.f) {
          bypass = sig.f;
          continue;
        }
        // This callback's NtCallbackReturn, reached inside a call-out it
        // made (up through the call-out's frames).
        if (err?.gexCallbackDone && e.gex_nt_callback_done(ctx)) return 1;
        if (!err?.gexHandoff && !e.gex_handoff()) {
          // The sibling-numbered-page race, as in the outer loop.
          if (/table index is out of bounds/.test(err.message)) {
            const tgt = new DataView(this.#memory.buffer).getBigUint64(Number(ctx) + CTX_LAST_TARGET, true);
            if (tgt && this.#growTablesToClaimed() && this.#tableRetries++ < 100000) {
              pc = tgt < 0n ? tgt + (1n << 64n) : tgt;
              continue;
            }
          }
          console.error(`[gex-nt] a user-mode callback crashed: ${err.stack ?? err}`);
          return 0;
        }
        if (e.gex_nt_callback_done(ctx)) return 1;
        if (e.gex_exited?.()) return 0;
        const want = e.gex_fill_wanted ? Number(e.gex_fill_wanted()) : -1;
        if (want >= 0) e.gex_fill_done(this.fillSlotNow(want >>> 0) ? 1 : 0);
        this.#denseCatchUp();
        if (e.gex_jit_pending_len() >>> 0) {
          const need = (e.gex_jit_pending_slots?.() ?? 0) >>> 0;
          if (need) this.#growTables(need, 'nt-callback');
        }
        while (e.gex_jit_pending_len() >>> 0) this.#installTranslationSync();
        this.#catchUpSync();
        this.#cgenServe();
        // A thread created inside the callback: posting it is synchronous,
        // only the answer would need the event loop.
        this.#drainThread().catch((err) => console.error(`[gex-nt] starting a thread: ${err.message ?? err}`));
        const next = e.gex_resume_pc();
        if (next === 0n) {
          console.error('[gex-nt] a user-mode callback stopped with nowhere to resume');
          return 0;
        }
        pc = next < 0n ? next + (1n << 64n) : next;
        const c = e.gex_resume_ctx() >>> 0;
        if (c) ctx = c;
      }
    }
  }

  /** This Worker's call-outs in progress, innermost last: the return each
   *  waits for, the stack it comes back at, and the table entry watching
   *  for a translated return (`{ table, index, old }`). */
  #callouts = [];
  /** What the watch stub found (`{ kind: 'return', i }` or `{ kind:
   *  'bypass', f }`), for the nested loop to act on. */
  #calloutSignal = null;
  /** Which nested loops this Worker is in, innermost last ('callout' or
   *  'callback'): a watch stub reached inside an NT callback is not its
   *  call-out's business. */
  #nest = [];
  #watchStub = null;

  /** The function a watched table entry holds: it tells JS (`hit`) and
   *  traps, so the nested loop in `#callOut` sees why. */
  #watch() {
    if (this.#watchStub) return this.#watchStub;
    // (module (type (func (param i32))) (import "w" "hit" (func (type 0)))
    //   (func (export "c") (type 0) local.get 0 call 0 unreachable))
    const bytes = new Uint8Array([
      0, 97, 115, 109, 1, 0, 0, 0,
      1, 5, 1, 96, 1, 127, 0,
      2, 9, 1, 1, 119, 3, 104, 105, 116, 0, 0,
      3, 2, 1, 0,
      7, 5, 1, 1, 99, 0, 1,
      10, 9, 1, 7, 0, 32, 0, 16, 0, 0, 11,
    ]);
    const inst = new WebAssembly.Instance(new WebAssembly.Module(bytes), { w: { hit: (ctx) => this.#watchHit(ctx >>> 0) } });
    this.#watchStub = inst.exports.c;
    return this.#watchStub;
  }

  /** A dispatch reached a watched entry: the awaited return of a call-out
   *  (the innermost one waiting for it, as natively), or something else
   *  that shares the entry (another address of an x86 slot, a deeper frame
   *  returning to the same place), which runs what the entry held. */
  #watchHit(ctx) {
    const dv = new DataView(this.#memory.buffer);
    const target = dv.getBigUint64(ctx + CTX_LAST_TARGET, true);
    const sp = dv.getBigUint64(ctx + CTX_SP, true);
    let i = -1;
    // Only a call-out with no NT callback nested inside it: an outer one's
    // return reached from inside a callback is not that call-out's (its
    // caller's frame is below the callback's), and runs as a bypass.
    const lastCallback = Math.max(this.#nest.lastIndexOf('callback'), this.#nest.lastIndexOf('exc'));
    for (let k = this.#callouts.length - 1; k >= 0; k--) {
      const c = this.#callouts[k];
      if (c.nestAt < lastCallback) break;
      if (c.ret === target && sp >= c.spWant && sp <= c.spWant + CALLOUT_UNWOUND) { i = k; break; }
    }
    dv.setBigUint64(ctx + CTX_PC, target, true);
    if (i >= 0) {
      this.#calloutSignal = { kind: 'return', i };
      return;
    }
    const ref = this.#shim.exports.gex_dispatch_ref(target);
    const table = Number(BigInt.asIntN(64, ref) >> 32n), index = Number(BigInt.asUintN(64, ref) & 0xffffffffn);
    const w = this.#callouts.find((c) => c.w && c.w.table === table && c.w.index === index)?.w;
    this.#calloutSignal = { kind: 'bypass', f: w?.old ?? null };
  }

  /** The table `table` of `gex_dispatch_ref` names. */
  #refTable(table) {
    return table === TABLE_NAMES.length ? this.#guest.exports.blocks : this.#dispatchTables()[table];
  }

  /** Point every call-out's return entry at the watch stub, wherever
   *  dispatch now reaches its return (an install or a retirement may have
   *  moved or overwritten it); an untranslated return is caught by the miss
   *  path (cgen.rs `returned`). */
  #armCallouts() {
    const e = this.#shim.exports;
    const stub = this.#watch();
    for (const c of this.#callouts) {
      const ref = e.gex_dispatch_ref(c.ret);
      const table = ref < 0n ? -1 : Number(ref >> 32n), index = ref < 0n ? -1 : Number(ref & 0xffffffffn);
      if (c.w && (c.w.table !== table || c.w.index !== index)) this.#unwatch(c);
      if (table < 0) continue;
      const t = this.#refTable(table);
      if (!t || index >= t.length) continue;
      if (c.w) {
        const now = t.get(index);
        if (now !== stub) c.w.old = now;
        if (now !== stub) t.set(index, stub);
        continue;
      }
      const shared = this.#callouts.find((o) => o !== c && o.w && o.w.table === table && o.w.index === index);
      const old = shared ? shared.w.old : t.get(index);
      c.w = { table, index, old };
      t.set(index, stub);
    }
  }

  /** Put a call-out's watched entry back, unless another still watches it. */
  #unwatch(c) {
    const w = c.w;
    c.w = null;
    if (!w || this.#callouts.some((o) => o !== c && o.w && o.w.table === w.table && o.w.index === w.index)) return;
    const t = this.#refTable(w.table);
    if (t && w.index < t.length && t.get(w.index) === this.#watchStub) t.set(w.index, w.old);
  }

  /**
   * The call-out (cgen.rs `gex_ftab_fallback`): run the guest from `target`
   * in a nested loop on this Worker until it returns to `ret` with the stack
   * at `spWant` (or what the callee popped above it). 1 then, and the
   * function body that called the fallback carries on; 0 when it cannot
   * (the process ended, the stack unwound past its caller), with `ctx::PC`
   * where the guest is, and the body leaves as before. Synchronous, as
   * `#ntCallback`: a shim frame is below this one.
   */
  #callOut(ctx, target, ret, spWant) {
    const e = this.#shim.exports;
    const sp0 = e.__stack_pointer.value;
    const me = this.#callouts.length;
    const c = { ret: BigInt.asUintN(64, ret), spWant: BigInt.asUintN(64, spWant), w: null, nestAt: this.#nest.length };
    // A guest call from native Wine (its return is gex-kernel's
    // NATIVE_RETURN_PAD): its native caller's frames are just above the
    // callee's arguments (cgen.rs NATIVE_UNWOUND, as natively).
    c.unwound = c.ret === NATIVE_RETURN_PAD ? NATIVE_UNWOUND : CALLOUT_UNWOUND;
    this.#callouts.push(c);
    this.#nest.push('callout');
    this.#calloutsMade++;
    if (globalThis.GEX_CALLOUT_TRACE || (globalThis.GEX_DENSE_DEBUG && ((this.#calloutsMade & (this.#calloutsMade - 1)) === 0 || this.#calloutsMade % 4096 === 0))) {
      console.error(`[callout] #${this.#calloutsMade} depth ${me}: ${BigInt.asUintN(64, target).toString(16)} returning to ${c.ret.toString(16)}`);
    }
    let pc = BigInt.asUintN(64, target);
    let next = null;
    try {
      this.#armCallouts();
      for (;;) {
        try {
          if (next) {
            const f = next;
            next = null;
            f(ctx);
          } else {
            this.#guest.exports.enter(ctx, pc);
          }
          console.error('[gex] guest code returned out of a call-out');
          return 0;
        } catch (err) {
          e.__stack_pointer.value = sp0;
          const sig = this.#calloutSignal;
          this.#calloutSignal = null;
          if (sig?.kind === 'return') return sig.i === me ? 1 : 0;
          if (sig?.kind === 'bypass') {
            if (!sig.f) throw new Error('gex: a watched dispatch entry with nothing to run');
            next = sig.f;
            continue;
          }
          if (!err?.gexHandoff && !e.gex_handoff()) {
            if (/table index is out of bounds/.test(err.message)) {
              const tgt = new DataView(this.#memory.buffer).getBigUint64(Number(ctx) + CTX_LAST_TARGET, true);
              if (tgt && this.#growTablesToClaimed() && this.#tableRetries++ < 100000) {
                pc = BigInt.asUintN(64, tgt);
                continue;
              }
            }
            throw err;
          }
          const hit = e.gex_callout_hit();
          if (hit >= 0) return hit === me ? 1 : 0;
          // gex_nr::NATIVE_LEFT (gexrt saw the guest leave a native caller's
          // frames): a native caller's call-out leaves, at the context's PC
          // (the entry the guest makes again); anything else carries on.
          if (err?.gexNativeLeft && c.ret === NATIVE_RETURN_PAD
              && new DataView(this.#memory.buffer).getBigUint64(ctx + CTX_SP, true) > c.spWant + c.unwound) return 0;
          // The process ended: down through every frame to the run loop
          // (returning would run the guest on).
          if (e.gex_exited?.()) {
            err.gexHandoff = true;
            throw err;
          }
          // Control left an exception dispatched nested further out: this
          // call-out's caller leaves, and the dispatch's loop sees the escape
          // at the next check (nt.rs `exc_escape`).
          if ((e.gex_exc_escaped?.(0) ?? -1) >= 0) return 0;
          // The NtCallbackReturn of a callback further out (the function
          // this call-out ran was the callback's dispatcher): its loop's, so
          // up through every frame between, as natively.
          if (this.#nest.lastIndexOf('callback') >= 0 && e.gex_nt_callback_done?.(ctx)) {
            const up = new Error('gex: a user-mode callback returned inside a call-out');
            up.gexCallbackDone = true;
            throw up;
          }
          const want = e.gex_fill_wanted ? Number(e.gex_fill_wanted()) : -1;
          if (want >= 0) e.gex_fill_done(this.fillSlotNow(want >>> 0) ? 1 : 0);
          this.#denseCatchUp();
          if (e.gex_jit_pending_len() >>> 0) {
            const need = (e.gex_jit_pending_slots?.() ?? 0) >>> 0;
            if (need) this.#growTables(need, 'call-out');
          }
          while (e.gex_jit_pending_len() >>> 0) this.#installTranslationSync();
          this.#catchUpSync();
          this.#cgenServe();
          this.#drainThread().catch((err2) => console.error(`[gex] starting a thread: ${err2.message ?? err2}`));
          const nextPc = e.gex_resume_pc();
          // Nowhere to resume (the thread ended, or waits): the run loop's
          // business, down through every frame.
          if (nextPc === 0n) {
            err.gexHandoff = true;
            throw err;
          }
          pc = BigInt.asUintN(64, nextPc);
          const rc = e.gex_resume_ctx() >>> 0;
          if (rc && rc !== ctx) {
            // A resume on another thread's context is not this call-out's.
            return 0;
          }
          // The stack unwound past the caller (longjmp, an exception
          // handled further out): its frame is gone, so it leaves.
          const sp = new DataView(this.#memory.buffer).getBigUint64(ctx + CTX_SP, true);
          if (sp > c.spWant + c.unwound) {
            new DataView(this.#memory.buffer).setBigUint64(ctx + CTX_PC, pc, true);
            return 0;
          }
          this.#armCallouts();
        }
      }
    } finally {
      this.#unwatch(c);
      this.#callouts.pop();
      this.#nest.pop();
      // Left by an exception, the shim's half has not popped its entry.
      e.gex_callout_unwind?.(me);
      if (globalThis.GEX_CALLOUT_TRACE) {
        const dv = new DataView(this.#memory.buffer);
        console.error(`[callout] end depth ${me}: pc ${dv.getBigUint64(ctx + CTX_PC, true).toString(16)} sp ${dv.getBigUint64(ctx + CTX_SP, true).toString(16)} want ${c.spWant.toString(16)}`);
      }
    }
  }

  #calloutsMade = 0;

  /**
   * An exception dispatched nested (nt.rs `raise_nested`, ffa2cec9's browser
   * half): run the guest from `target` (KiUserExceptionDispatcher) in a loop
   * on this Worker until control goes back above the raise (the escape for
   * depth `level`): 1; 0 when it could not, and the syscall traps out as
   * before. Synchronous, as `#ntCallback`.
   */
  #excDispatch(ctx, target, level) {
    const e = this.#shim.exports;
    const sp0 = e.__stack_pointer.value;
    this.#nest.push('exc');
    if (globalThis.GEX_CALLOUT_TRACE) console.error(`[exc] nested dispatch depth ${level} at ${BigInt.asUintN(64, target).toString(16)}`);
    let pc = BigInt.asUintN(64, target);
    let bypass = null;
    try {
      for (;;) {
        try {
          if (bypass) {
            const f = bypass;
            bypass = null;
            f(ctx);
          } else {
            this.#guest.exports.enter(ctx, pc);
          }
          console.error('[gex] guest code returned out of exception dispatch');
          return 0;
        } catch (err) {
          e.__stack_pointer.value = sp0;
          const sig = this.#calloutSignal;
          this.#calloutSignal = null;
          if (sig?.kind === 'bypass' && sig.f) {
            bypass = sig.f;
            continue;
          }
          if (!err?.gexHandoff && !e.gex_handoff()) {
            if (/table index is out of bounds/.test(err.message)) {
              const tgt = new DataView(this.#memory.buffer).getBigUint64(Number(ctx) + CTX_LAST_TARGET, true);
              if (tgt && this.#growTablesToClaimed() && this.#tableRetries++ < 100000) {
                pc = BigInt.asUintN(64, tgt);
                continue;
              }
            }
            throw err;
          }
          if (e.gex_exc_escaped(0) === level) {
            e.gex_exc_escaped(1);
            if (globalThis.GEX_CALLOUT_TRACE) console.error(`[exc] depth ${level} escaped`);
            return 1;
          }
          if (e.gex_exited?.()) {
            err.gexHandoff = true;
            throw err;
          }
          const want = e.gex_fill_wanted ? Number(e.gex_fill_wanted()) : -1;
          if (want >= 0) e.gex_fill_done(this.fillSlotNow(want >>> 0) ? 1 : 0);
          this.#denseCatchUp();
          if (e.gex_jit_pending_len() >>> 0) {
            const need = (e.gex_jit_pending_slots?.() ?? 0) >>> 0;
            if (need) this.#growTables(need, 'exception');
          }
          while (e.gex_jit_pending_len() >>> 0) this.#installTranslationSync();
          this.#catchUpSync();
          this.#cgenServe();
          this.#drainThread().catch((err2) => console.error(`[gex] starting a thread: ${err2.message ?? err2}`));
          const nextPc = e.gex_resume_pc();
          // Nowhere to resume (the thread ended, or waits): the run loop's
          // business, down through every frame.
          if (nextPc === 0n) {
            err.gexHandoff = true;
            throw err;
          }
          pc = BigInt.asUintN(64, nextPc);
          const rc = e.gex_resume_ctx() >>> 0;
          if (rc && rc !== ctx) return 0;
          this.#armCallouts();
        }
      }
    } finally {
      this.#nest.pop();
      e.gex_exc_unwind?.(level);
    }
  }

  /** What a dispatch miss needs, installed synchronously on this Worker
   *  (lib.rs `in_place`): 1, or 0 when it cannot be done here (a page's
   *  main thread may not compile synchronously) and the miss traps out. */
  #jitInstall() {
    if (typeof window !== 'undefined') return 0;
    const e = this.#shim.exports;
    try {
      this.#denseCatchUp();
      if (e.gex_jit_pending_len() >>> 0) {
        const need = (e.gex_jit_pending_slots?.() ?? 0) >>> 0;
        if (need) this.#growTables(need, 'in-place');
      }
      while (e.gex_jit_pending_len() >>> 0) this.#installTranslationSync();
      this.#catchUpSync();
      this.#armCallouts();
      return 1;
    } catch (err) {
      console.error(`[gex] an in-place install failed, trapping instead: ${err.message ?? err}`);
      return 0;
    }
  }

  /** `#installTranslation` without yielding, for `#ntCallback`. */
  #installTranslationSync() {
    const e = this.#shim.exports;
    const ptr = e.gex_jit_pending_ptr() >>> 0;
    const len = e.gex_jit_pending_len() >>> 0;
    if (len === 0) return;
    const bytes = this.#bytes().slice(ptr, ptr + len);
    const key = moduleContentKey(bytes);
    this.#installModule(bytes, e.gex_jit_slots() >>> 0, 64, null, 'nt-callback', null, true);
    e.gex_jit_installed();
    const count = e.gex_module_count ? e.gex_module_count() >>> 0 : this.#installed;
    this.#installed = count;
    if (count) {
      const mod = this.#compiled[this.#compiled.length - 1]?.mod;
      if (mod && key && !this.#sharedModules.has(key)) {
        this.#sharedModules.set(key, mod);
        this.#env.shareModule?.(key, mod);
      }
    }
  }

  /** `#catchUp` (eager) without yielding, for `#ntCallback`. */
  #catchUpSync() {
    const e = this.#shim.exports;
    if (!e.gex_module_count) return;
    const have = e.gex_module_count() >>> 0;
    const need = (e.gex_jit_slots_total?.() ?? 0) >>> 0;
    for (; this.#installed < have; this.#installed++) {
      const i = this.#installed;
      const ptr = e.gex_module_ptr(i) >>> 0;
      const len = e.gex_module_len(i) >>> 0;
      if (!len) continue;
      const bytes = this.#bytes().slice(ptr, ptr + len);
      const key = moduleContentKey(bytes);
      const cached = key ? this.#sharedModules.get(key) : undefined;
      this.#installModule(bytes, need, 64, null, 'nt-catchup', cached, true);
      if (cached) continue;
      const mod = this.#compiled[this.#compiled.length - 1]?.mod;
      if (mod && key) {
        this.#sharedModules.set(key, mod);
        this.#env.shareModule?.(key, mod);
      }
    }
  }

  async #installTranslation() {
    const e = this.#shim.exports;
    const ptr = e.gex_jit_pending_ptr() >>> 0;
    const len = e.gex_jit_pending_len() >>> 0;
    // A zero-length published view means the shared pending queue was drained
    // (by a sibling Worker) between the caller's `gex_jit_pending_len()` guard
    // and this read -- the queue is process-wide, the view is per-Worker.
    // Compiling a zero-length slice traps ("BufferSource argument is empty" --
    // Qt's heavy multi-thread JIT hit exactly this); there is nothing to install,
    // so resync the view and stop rather than crash.
    if (len === 0) {
      if (globalThis.GEX_JIT_TRACE) console.error('[jit] empty pending view (sibling drained the queue), skipping compile');
      return;
    }
    // GEX_CHECK_INSTALL: once the pending queue is drained, is the slot the
    // missed pc dispatches through written? A unit whose blocks land elsewhere
    // makes the resume run whatever was there before -- a wild branch with no
    // trace. Checked only when nothing is left to install: the queue is FIFO
    // and a served module ahead of the unit legitimately lacks the block.
    const check = globalThis.GEX_CHECK_INSTALL || (typeof process !== 'undefined' && process.env?.GEX_CHECK_INSTALL);
    const bytes = this.#bytes().slice(ptr, ptr + len);
    const key = moduleContentKey(bytes);
    await this.#installModule(bytes, e.gex_jit_slots() >>> 0, 64, null, 'jit2878');
    e.gex_jit_installed();
    if (check && e.gex_miss_pc && e.gex_dispatch_slot && (e.gex_jit_pending_len() >>> 0) === 0) {
      try {
        const raw = e.gex_miss_pc();
        const mpc = raw < 0n ? raw + (1n << 64n) : raw;
        const packed = e.gex_dispatch_slot(mpc);
        const tabs = this.#dispatchTables();
        const stub = tabs[0].get(0);
        let what;
        if (packed < 0) what = 'no page-table entry';
        else {
          const t = packed >>> 24, sl = packed & 0xffffff;
          const f = sl < (tabs[t]?.length ?? 0) ? tabs[t].get(sl) : 'past-end';
          what = f === stub ? `STUB at table ${t} slot ${sl} (len ${tabs[t]?.length})` : f === 'past-end' ? `PAST END table ${t} slot ${sl} len ${tabs[t]?.length}` : null;
        }
        const dv = new DataView(this.#memory.buffer);
        const dir = dv.getUint32(DISPATCH_DIR + Number(mpc >> 12n) * 4, true) >>> 0;
        if (what) console.error(`[install-check] pid ${e.gex_my_pid?.() ?? '?'} miss pc 0x${mpc.toString(16)}: ${what}; dir entry t${dir >>> 24} s${dir & 0xffffff}, pt ${packed >= 0 ? `t${packed >>> 24} s${(packed & 0xffffff) - Number(mpc & 0xfffn)}` : '-'}`);
      } catch (err) { console.error(`[install-check] failed: ${err}`); }
    }
    // This Worker translated it, so the entry `gex_jit_installed` just added is
    // the newest here and must not be replayed on top of itself.
    const count = e.gex_module_count ? e.gex_module_count() >>> 0 : this.#installed;
    this.#installed = count;
    // Hand the module this Worker just translated to the sibling cache -- the
    // main thread translates almost everything, and a thread that spawns later
    // then reuses it (from its start message) instead of recompiling the whole
    // registry. Its index is the newest entry.
    if (count) {
      const mod = this.#compiled[this.#compiled.length - 1]?.mod;
      if (mod && key && !this.#sharedModules.has(key)) {
        this.#sharedModules.set(key, mod);
        this.#env.shareModule?.(key, mod);
      }
    }
  }

  /**
   * Put one translated module's blocks into this Worker's dispatch tables.
   * One function for the two ways a module arrives (this Worker translated it,
   * or another did and this one catches up): a thread that installed them
   * differently would dispatch to different code.
   */
  async #installModule(bytes, need, segments = 64, meta = null, who = '?', precompiled = null, sync = false) {
    const t0 = performance.now();
    this.#installs++;
    try {
      // With `sync` the inner call never awaits, so the install is complete
      // when this returns its promise -- what a nested loop inside an import
      // needs, since it cannot yield.
      return await this.#installModuleInner(bytes, need, segments, meta, who, precompiled, sync);
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
  async #installModuleInner(bytes, need, segments = 64, meta = null, who = '?', precompiled = null, sync = false) {
    const t0 = performance.now();
    const e = this.#shim.exports;
    const tg = performance.now();
    const tables = this.#growTables(need, 'install');
    this.#growMs += performance.now() - tg;
    this.#tnAt('pre-inst');
    const tc = performance.now();
    // Nothing to install: a zero-length buffer is not a module, and
    // `WebAssembly.compile` on one traps ("BufferSource argument is empty").
    // It reaches here when a Worker's view of the shared pending queue, or a
    // registry slot, is drained/unwritten under concurrency -- an empty module
    // has no blocks, so skipping it is exactly correct. Logged (with `who`) so
    // the racing producer can be found.
    if (!(precompiled instanceof WebAssembly.Module)
        && !(bytes instanceof WebAssembly.Module)
        && (bytes?.byteLength ?? 0) === 0) {
      if (globalThis.GEX_JIT_TRACE) console.error(`[jit] empty module bytes, skipping install (who=${who})`);
      return null;
    }
    // A `precompiled` Module (a sibling Worker's, relayed by the host) skips the
    // compile entirely; otherwise compile the bytes (or use them if already a
    // Module). Same registry bytes either way, so the Module is interchangeable.
    let mod;
    try {
      mod = precompiled instanceof WebAssembly.Module
        ? precompiled
        : bytes instanceof WebAssembly.Module ? bytes
          : sync ? new WebAssembly.Module(bytes) : await WebAssembly.compile(bytes);
    } catch (err) {
      // The engine refused a translation: hand its bytes to whoever serves
      // the page (tests/nt's harness keeps them), synchronously, since the
      // Worker may be about to die. Best effort.
      if (bytes instanceof Uint8Array && err instanceof WebAssembly.CompileError) {
        // Under Node, `GEX_DUMP_BAD_WASM=DIR` keeps it, as gexc does natively.
        const dir = globalThis.process?.env?.GEX_DUMP_BAD_WASM;
        if (dir) try { globalThis.process.getBuiltinModule('fs').writeFileSync(`${dir}/bad-${who}-${Date.now()}.wasm`, bytes); } catch {}
        try {
          const x = new XMLHttpRequest();
          x.open('POST', `/__gex_dump?name=${encodeURIComponent(who)}`, false);
          x.send(bytes.slice());
        } catch {}
      }
      throw err;
    }
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
    // Dense: the module's blocks go to `blocks` at the base its bytes name,
    // within what the process has numbered.
    if (this.#guest?.exports?.blocks) {
      imports.blocks = this.#guest.exports.blocks;
      this.#growBlocks();
      const ds = denseSection(bytes instanceof Uint8Array ? bytes : mod);
      if (ds) {
        const dv = new DataView(ds.buffer, ds.byteOffset, ds.byteLength);
        const base = dv.getUint32(0, true);
        if (globalThis.GEX_DENSE_DEBUG) {
          console.error(`[dense] ${who} base=${base} n=${dv.getUint32(4, true)} blocks=${imports.blocks.length} size=${e.gex_dense_size()}`);
        }
        // Base 0 is a module nobody numbered: its blocks would land on the
        // miss stub and its neighbours.
        if (base === 0) throw new Error(`gex: a translation with unnumbered dense blocks reached the install (${who})`);
      }
    }
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
        if (this.#denseTable(i)) continue;
        const want = Math.min(Math.max(need - i * per, 0), per);
        if (tables[i].length < want) {
          grew += want - tables[i].length;
          growTable(tables[i], want - tables[i].length, stub, i);
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
    if (t < 0 || t >= tables.length || tables[t].length >= end || this.#denseTable(t)) return;
    const per = (e.gex_slots_per_table?.() ?? 0) >>> 0 || Number.MAX_SAFE_INTEGER;
    const CHUNK = SLOTS_PER_PAGE * 16;
    const want = Math.min(Math.ceil(end / CHUNK) * CHUNK, per);
    const stub = tables[0].get(0);
    const delta = want - tables[t].length;
    if (delta <= 0) return;
    const g0 = performance.now();
    growTable(tables[t], delta, stub, t);
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
        const slot = base + ((pc & 0xfff) >>> SLOT_SHIFT);
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
            const mine = ((pc & 0xfff) >>> SLOT_SHIFT) & ~31;
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
    const want = at + ((mpc & 0xfff) >>> SLOT_SHIFT);
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
          const want = at + ((mpc & 0xfff) >>> SLOT_SHIFT);
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
            + ` (base ${at} + ${(mpc & 0xfff) >>> SLOT_SHIFT}) t=${tn2}: ${set}`);
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
    if (page * 4096 < 0x04a0_0000) return null;
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
    if (slot * 4096 >= 0x04a0_0000) {
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

  /** A dispatch missed on a block of this process this Worker never
   *  instantiated (`gex_dense_want`): instantiate the registry module that
   *  holds it, whatever order this Worker caught up in. Synchronous. */
  #denseCatchUp() {
    const e = this.#shim.exports;
    const d = e.gex_dense_want ? e.gex_dense_want() >>> 0 : 0;
    if (!d) return;
    const i = e.gex_dense_owner(d);
    if (i < 0) return;
    const ptr = e.gex_module_ptr(i) >>> 0;
    const len = e.gex_module_len(i) >>> 0;
    if (!len) return;
    const bytes = this.#bytes().slice(ptr, ptr + len);
    const key = moduleContentKey(bytes);
    const cached = key ? this.#sharedModules.get(key) : undefined;
    this.#installModule(bytes, 0, 64, null, `dense-${i}`, cached, true);
    this.#denseCatchUps++;
    if (cached) return;
    const mod = this.#compiled[this.#compiled.length - 1]?.mod;
    if (mod && key) {
      this.#sharedModules.set(key, mod);
      this.#env.shareModule?.(key, mod);
    }
  }

  #denseCatchUps = 0;

  /** Whether dispatch table `t` is dense here (gex_emit::dense::FIRST_TABLE
   *  and up): its slots are the process's slot maps, never this table. */
  #denseTable(t) {
    return this.#dense && t >= DENSE_FIRST_TABLE;
  }

  /** Grow this Worker's `blocks` to every index the process has numbered
   *  (`gex_dense_size`), the miss stub (its entry 0) in the new entries.
   *  Whether it grew. */
  #growBlocks() {
    const b = this.#guest?.exports?.blocks;
    if (!this.#dense || !b) return false;
    const need = this.#shim.exports.gex_dense_size() >>> 0;
    const len = b.length;
    if (len >= need) return false;
    const want = Math.max(need, Math.min(len * 2, len + (1 << 18)));
    growTable(b, want - len, b.get(0), 'blocks');
    return true;
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
