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
import { lazyFetch } from "./lazy-fetch.mjs";
import { xcRead, xcWrite } from "./xcache.mjs";
import { single, sched } from "./single.mjs";

// Read when used, not at import: a Worker learns it from its first message.
const m64 = () => !!globalThis.GEX_MEM64;
const LIMIT64_BYTES = 16 * 2 ** 30;
/** A value for an address-typed global: i64 (BigInt) in a wasm64 module. */
const addrVal = (v) => (m64() ? BigInt(v >>> 0) : v | 0);
/** The global imports of a wasm module, as `module.name` -> WebAssembly.Global descriptor
 *  (JS cannot ask a Module for its imports' types). */
function importedGlobals(bytes) {
  const out = new Map();
  let p = 8;
  const leb = () => {
    let r = 0, sh = 0, b;
    do { b = bytes[p++]; r |= (b & 0x7f) * 2 ** sh; sh += 7; } while (b & 0x80);
    return r;
  };
  const str = () => { const n = leb(); const s = new TextDecoder().decode(bytes.subarray(p, p + n)); p += n; return s; };
  while (p < bytes.length) {
    const id = bytes[p++];
    const size = leb();
    const end = p + size;
    if (id === 2) {
      const count = leb();
      for (let i = 0; i < count; i++) {
        const m = str(), n = str(), kind = bytes[p++];
        if (kind === 0) leb();
        else if (kind === 1) { p++; const f = leb(); leb(); if (f & 1) leb(); }
        else if (kind === 2) { const f = leb(); leb(); if (f & 1) leb(); }
        else if (kind === 3) {
          const t = bytes[p++], mut = bytes[p++];
          out.set(`${m}.${n}`, { value: { 0x7f: 'i32', 0x7e: 'i64', 0x7d: 'f32', 0x7c: 'f64' }[t] ?? 'i64', mutable: !!(mut & 1) });
        } else if (kind === 4) { p++; leb(); }
      }
      return out;
    }
    p = end;
  }
  return out;
}

/** For each element of a PIC module's (first active) element segment, its function's type
 *  packed as the provider's JIT asks for it (`provider_jit_table` SIG, the native `wasmjit`):
 *  bits 0..4 the parameter count, 5..7 the result (0 none, 1 i32, 2 i64, 3 f32, 4 f64), then two
 *  bits per parameter from bit 8 (0 i32, 1 i64, 2 f32, 3 f64); -1 when not representable. */
function elementSignatures(bytes) {
  let p = 8;
  const leb = () => {
    let r = 0, sh = 0, b;
    do { b = bytes[p++]; r += (b & 0x7f) * 2 ** sh; sh += 7; } while (b & 0x80);
    return r;
  };
  const skipStr = () => { const n = leb(); p += n; };
  const vcode = { 0x7f: 0, 0x7e: 1, 0x7d: 2, 0x7c: 3 };
  const types = [];
  let importedFuncs = 0;
  const funcTypes = [];
  let elems = null;
  while (p < bytes.length) {
    const id = bytes[p++];
    const size = leb();
    const end = p + size;
    if (id === 1) {
      const n = leb();
      for (let i = 0; i < n; i++) {
        if (bytes[p] === 0x4e || bytes[p] === 0x4f || bytes[p] === 0x50) { types.push(-1); p = end; break; } // rec/sub: not ours
        p++; // 0x60
        const np = leb();
        let packed = np > 28 ? -1 : np, ok = np <= 28;
        for (let k = 0; k < np; k++) {
          const c = vcode[bytes[p++]];
          if (c === undefined) ok = false; else if (ok) packed += c * 2 ** (8 + 2 * k);
        }
        const nr = leb();
        if (nr > 1) ok = false;
        for (let k = 0; k < nr; k++) {
          const c = vcode[bytes[p++]];
          if (c === undefined) ok = false; else if (ok) packed += (c + 1) * 32;
        }
        types.push(ok ? packed : -1);
      }
    } else if (id === 2) {
      const n = leb();
      for (let i = 0; i < n; i++) {
        skipStr(); skipStr();
        const kind = bytes[p++];
        if (kind === 0) { leb(); importedFuncs++; funcTypes.push(-2); }
        else if (kind === 1) { p++; const f = leb(); leb(); if (f & 1) leb(); }
        else if (kind === 2) { const f = leb(); leb(); if (f & 1) leb(); }
        else if (kind === 3) { p += 2; }
        else if (kind === 4) { p++; leb(); }
      }
    } else if (id === 3) {
      const n = leb();
      for (let i = 0; i < n; i++) funcTypes.push(leb());
    } else if (id === 9 && !elems) {
      const n = leb();
      for (let i = 0; i < n && !elems; i++) {
        const flags = leb();
        if (flags === 0 || flags === 2) {
          if (flags === 2) leb();
          while (bytes[p] !== 0x0b) p++; // the offset expression
          p++;
          if (flags === 2) p++; // elemkind
          const m = leb();
          elems = [];
          for (let k = 0; k < m; k++) elems.push(leb());
        } else {
          p = end; // other forms: not what a PIC module writes
        }
      }
    }
    p = end;
  }
  // Imported functions' types are not known here (their entries are -2 above): -1.
  return (elems ?? []).map((fi) => {
    const ti = funcTypes[fi];
    return ti === undefined || ti < 0 ? -1 : (types[ti] ?? -1);
  });
}

/** An index into the function table: a table64 (BigInt indices) beside a 64-bit memory. */
const tIdx = (v) => (m64() ? BigInt(v) : v);

/**
 * Decode UTF-8 out of guest memory.
 *
 * `TextDecoder.decode` refuses a `SharedArrayBuffer`-backed view, so a shared
 * memory needs the `slice` copy; it is only paid when the memory is shared.
 */
const DECODER = new TextDecoder();
export function utf8(view) {
  return DECODER.decode(isShared(view.buffer) ? view.slice() : view);
}

/** Whether `buf` is a SharedArrayBuffer -- asked without naming the global,
 *  which a page that is not cross-origin isolated does not have. */
const SAB = globalThis.SharedArrayBuffer;
function isShared(buf) {
  return typeof SAB === 'function' && buf instanceof SAB;
}

// Registry indices describe a shared queue, not module identity: two Workers
// can drain and publish that queue in different turns. A compiled Module may
// only be reused for byte-identical input, or its element segments install
// unrelated functions into otherwise-correct dispatch slots. Two independent
// 32-bit accumulators plus the exact length make a cheap content key; this is
// validation against an index race, not a persistent content-addressed cache.
/** The most the module store (`Gex#moduleStore`) may grow to, tried in turn:
 *  the first a runtime accepts. 4 GiB filled up in 22 minutes of Hollow Knight
 *  on Unity's own Mono (the translations of Mono's JIT output), after which
 *  the registry kept its bytes in the shim's heap again and that ran out.
 *  A growable SharedArrayBuffer only commits what it has grown to. */
const MODULE_STORE_MAX = [32, 16, 8, 4].map((g) => g * 1024 * 1024 * 1024);
/** A new module store: a growable SharedArrayBuffer whose first eight bytes
 *  are the bump cursor. Null where SharedArrayBuffer cannot grow. */
function makeModuleStore() {
  if (typeof SharedArrayBuffer === 'undefined' || typeof SharedArrayBuffer.prototype.grow !== 'function') return null;
  // Off, for comparing against the registry keeping its own bytes.
  if (globalThis.GEX_NO_MODULE_STORE || globalThis.process?.env?.GEX_NO_MODULE_STORE) return null;
  for (const max of MODULE_STORE_MAX) {
    try {
      const sab = new SharedArrayBuffer(1 << 20, { maxByteLength: max });
      Atomics.store(new BigInt64Array(sab, 0, 1), 0, 8n);
      if (globalThis.GEX_REGISTRY_TRACE || globalThis.process?.env?.GEX_REGISTRY_TRACE) {
        globalThis.process?.getBuiltinModule?.('fs')?.writeSync(2, `[gex] module store max ${max / 2 ** 30} GiB\n`);
      }
      return sab;
    } catch {
      // Too big for this runtime: the next.
    }
  }
  return null;
}
let storeFullSaid = false;
/** Append `bytes` to a module store: their offset, or -1 when it is full.
 *  Any Worker of the process may append at once: the cursor is atomic and a
 *  grow that loses a race to a bigger one is simply retried. */
function storeAppend(sab, bytes) {
  const len = bytes.length;
  const off = Number(Atomics.add(new BigInt64Array(sab, 0, 1), 0, BigInt((len + 7) & ~7)));
  const end = off + len;
  if (end > sab.maxByteLength) {
    // Said once: from here the registry keeps bytes in the shim's heap, which
    // shares the guest's 2 GiB arena and will run out in a long run.
    if (!storeFullSaid) {
      storeFullSaid = true;
      console.error(`[gex] module store full at ${(sab.maxByteLength / 2 ** 30).toFixed(0)} GiB: the translated-module registry keeps its bytes in the shim heap from here`);
    }
    return -1;
  }
  while (sab.byteLength < end) {
    try {
      sab.grow(Math.min(sab.maxByteLength, Math.max(end, sab.byteLength * 2)));
    } catch {
      // A sibling grew past what this asked for meanwhile; the loop rechecks.
    }
  }
  new Uint8Array(sab, off, len).set(bytes);
  return off;
}
function moduleContentKey(bytes) {
  if (!(bytes instanceof Uint8Array)) return null;
  // A little-endian word at a time, then the tail bytes (the shim's
  // `content_key` is the same function): a byte at a time was 0.65 s of a
  // node start-up per Worker.
  let a = 0x811c9dc5;
  let b = 0x9e3779b9;
  const n = bytes.length >>> 2;
  const words = (bytes.byteOffset & 3) === 0 && !isShared(bytes.buffer)
    ? new Uint32Array(bytes.buffer, bytes.byteOffset, n)
    : null;
  for (let i = 0; i < n; i++) {
    const x = words ? words[i]
      : (bytes[4 * i] | (bytes[4 * i + 1] << 8) | (bytes[4 * i + 2] << 16) | (bytes[4 * i + 3] << 24)) >>> 0;
    a = Math.imul(a ^ x, 0x01000193);
    b = Math.imul(b ^ ((x + i) | 0), 0x85ebca6b);
    b ^= b >>> 13;
  }
  for (let i = 4 * n; i < bytes.length; i++) {
    const x = bytes[i];
    a = Math.imul(a ^ x, 0x01000193);
    b = Math.imul(b ^ (x + (n + i - 4 * n)), 0x85ebca6b);
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
  narrate(text);
}

// Narration under node goes to stderr: node's console.debug is stdout, which
// is the guest's own output there (an ejected program's, a test's compared
// output). In a page it stays on the debug console.
export function narrate(text) {
  // Synchronously where there is a file descriptor to write to. A node
  // Worker's `process.stderr` is flushed by its event loop. A Worker parked
  // in wasm on its wait slot until its process ends never turns that loop, so
  // the lines were lost when it was terminated: item3's "libsqlite3.so.0
  // replaced by" lines were all lost after the first child process.
  const fs = typeof process !== 'undefined' ? process.getBuiltinModule?.('fs') : null;
  if (fs?.writeSync) {
    try {
      fs.writeSync(2, `${text}\n`);
      return;
    } catch {}
  }
  const stderr = typeof process !== 'undefined' ? process.stderr : null;
  if (stderr?.write) stderr.write(`${text}\n`);
  else console.debug(text);
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
    ? new Uint32Array(src.buffer, src.byteOffset, src.byteLength / 4)
    : null;
  // Division, not `>> 2`: a region can pass 2 GiB (see `#processImage`).
  const blank = (from, to) => {
    if (words) {
      for (let i = from / 4; i < to / 4; i++) if (words[i] !== 0) return false;
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
/// How many bytes a fork image entry carries: its `bytes`, or, staged in the
/// global kernel's memory (`#processImage`), its `len`.
function imageLen(r) {
  return r.len ?? r.bytes?.byteLength ?? 0;
}

function sum32(buf) {
  const u = new Uint32Array(buf.buffer ?? buf, buf.byteOffset ?? 0, Math.floor(buf.byteLength / 4));
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


/**
 * Whether `elf` is a wasm32 program flagged as running i386 code (native
 * Wine's loader, W3): its GEXWASM header sits 0x40 bytes before the entry
 * point (cgen/wasmprog.rs), with the flags at offset 36.
 */
/** clang's names for a C main (see the PE module's instantiation). */
const MAIN_NAMES = ['main', '__main_argc_argv', '__original_main'];

function isW3Program(elf) {
  if (!(elf instanceof Uint8Array) || elf.length < 52 || elf[0] !== 0x7f || elf[4] !== 1) return false;
  const dv = new DataView(elf.buffer, elf.byteOffset, elf.byteLength);
  const at = dv.getUint32(24, true) - 0x40;
  const phoff = dv.getUint32(28, true), phent = dv.getUint16(42, true), phnum = dv.getUint16(44, true);
  for (let i = 0; i < phnum; i++) {
    const p = phoff + i * phent;
    if (p + 32 > elf.length || dv.getUint32(p, true) !== 1) continue;
    const off = dv.getUint32(p + 4, true), vaddr = dv.getUint32(p + 8, true), filesz = dv.getUint32(p + 16, true);
    if (at < vaddr || at + 40 > vaddr + filesz) continue;
    const h = off + (at - vaddr);
    const magic = String.fromCharCode(...elf.subarray(h, h + 7));
    return magic === 'GEXWASM' && (dv.getUint32(h + 36, true) & 1) !== 0;
  }
  return false;
}

/** The epoch every Worker's high-resolution clock shares, in nanoseconds:
 * `performance.timeOrigin` is when this realm started, on the wall clock, so
 * `timeOrigin + now()` reads the same in every Worker. Whole microseconds,
 * exact in a double. */
const ORIGIN_NS = BigInt(Math.round(performance.timeOrigin * 1000)) * 1000n;

/**
 * Nanoseconds since the Unix epoch, as the guest's clocks and deadlines read
 * them: monotonic and fine-grained (about 5 us with cross-origin isolation),
 * where `Date.now()` steps by a millisecond and follows the wall clock
 * backwards. A delta-time loop saw 0 ms frames, and Wine's
 * QueryPerformanceCounter, which sits on CLOCK_MONOTONIC, ticked by 1 ms.
 * The same base in every Worker: park deadlines, futex timeouts and the
 * lock-free clock calls compare one Worker's reading with another's.
 */
export function hostNowNs() {
  return ORIGIN_NS + BigInt(Math.round(performance.now() * 1e6));
}

export class Env {
  /** Called with the bytes of a guest write to fd 1 or 2. */
  write(fd, bytes) {
    const text = utf8(bytes);
    if (fd === 2) console.error(text);
    else console.log(text);
  }
  /** Nanoseconds since an arbitrary epoch. */
  now() { return hostNowNs(); }
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

/** Where a module's `gex.dense` section payload (its base word) starts, or -1. */
function denseBaseAt(wasm) {
  let i = 8;
  const leb = () => {
    let n = 0, shift = 0, b;
    do { b = wasm[i++]; n |= (b & 0x7f) << shift; shift += 7; } while (b & 0x80);
    return n >>> 0;
  };
  while (i < wasm.length) {
    const id = wasm[i++];
    const size = leb();
    const end = i + size;
    if (id === 0) {
      const n = leb();
      if (n === 9 && utf8(wasm.subarray(i, i + n)) === 'gex.dense') return i + n;
    }
    i = end;
  }
  return -1;
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

/** posix_spawn children run in their parent's Worker, narrated
 *  (GEX_VFORK_TRACE, `?vforktrace=1`): to the console, or appended to the
 *  file it names under Node (a Worker's console output is lost when the run
 *  ends first). */
function vforkTrace(line) {
  const to = globalThis.GEX_VFORK_TRACE || globalThis.process?.env?.GEX_VFORK_TRACE;
  if (!to) return;
  if (typeof to === 'string' && to.includes('/') && globalThis.process?.getBuiltinModule) {
    globalThis.process.getBuiltinModule('fs').appendFileSync(to, `${line}\n`);
  } else {
    console.error(line);
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
  /** `#fjitModule`'s compile time, and when it next reports (`[gex] fjit so far`). */
  #fjitCompileMs = 0;
  #fjitNextReport = 0;
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
  /** A W3 process's Windows address-space line: a forked child's from its
   *  parent, -1 for a fresh process to split at once (see `#workerStack`). */
  #windowsSpace = 0;
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
  /** The single-thread build: where this fiber's JS last was, for the census
   *  (a fiber that waits in JS is otherwise invisible to it). */
  #where = '';
  #kernel = null;
  // This Worker's shim stack, claimed on the way in. See `#workerStack`.
  #stackTop = null;
  /** This instance is a forked child's first: its slot bits are its parent's. */
  #forkChild = false;
  /** A forked child's inherited regions' ends, for its first arena claim. */
  #inheritedEnds = [];

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
  waits = { park: 0, turn: 0, nap: 0, net: 0 };

  /** Resolves the network wait in progress (`#netWait`) on this Worker's own
   *  network's activity. */
  #netWake = null;
  #kernelWords = null;

  /** What ends a network wait, sampled before the guest is given another
   *  chance (`gex_wake`): the page network's event count. (This Worker's wait
   *  slot is sampled by the shim itself, at the start of each scheduler turn:
   *  `gex_wait_seq`.) */
  #netSnap() {
    return { n: this.#net?.activitySeq?.() ?? null };
  }

  /**
   * Sleep, without blocking this Worker's event loop, until the network has
   * something (this Worker's own network's `onActivity`, or the page's
   * through the bridge's activity word), something the thread waits for is
   * notified (`Atomics.waitAsync` on this Worker's wait slot), or the waiting
   * thread's own deadline is due (`gex_wait_bound_ms`, at most 50 ms). It
   * replaces a MessageChannel turn loop that spun every network-waiting
   * Worker at full speed, each turn taking the host lock four times (node's
   * idle Workers in the page: ~2.2 s of KLock each over a 13 s run).
   */
  #netWait(e, snap) {
    const ms = e.gex_wait_bound_ms ? e.gex_wait_bound_ms(50) : 1;
    const waits = [new Promise((r) => { this.#netWake = r; })];
    const word = e.gex_wait_word?.() >>> 0;
    if (word) {
      this.#kernelWords ??= new Int32Array(this.#kernelSpec.memory.buffer);
      const w = Atomics.waitAsync(this.#kernelWords, word >>> 2, e.gex_wait_seq(), ms);
      if (!w.async) return Promise.resolve();
      waits.push(w.value);
    } else {
      waits.push(new Promise((r) => setTimeout(r, Math.min(ms, 4))));
    }
    if (snap.n !== null) waits.push(this.#net.activityWait(snap.n, ms));
    return Promise.race(waits).finally(() => { this.#netWake = null; });
  }

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

  constructor(env = new Env(), { shared = false, memory = null, kernel = null, moduleStore = null } = {}) {
    this.#env = env;
    // A thread joins its process's module store (`#moduleStore`) too.
    this.#moduleStore = moduleStore;
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
      // The single-thread build runs the shared build's shim on a plain
      // memory: one memory, several instances (threads), so the same claim.
      if (typeof this.#shim.exports.gex_worker_init?.call === 'function' && (isShared(this.#memory.buffer) || single.on)) {
        const k = Atomics.add(new Int32Array(this.#memory.buffer), BOOT_STACK_COUNTER >> 2, 1) & 31;
        this.#shim.exports.__stack_pointer.value = addrVal(SHIM_STACK_TOP - k * 0x40000);
      }
      // A forked child of a W3 process splits its memory at its parent's
      // line before anything claims arena -- this stack is the first claim,
      // and a fresh instance's would otherwise go above the inherited image,
      // which there is the top of the 2 GiB.
      if (this.#windowsSpace > 0) this.#shim.exports.gex_adopt_windows_space?.(this.#windowsSpace);
      else if (this.#windowsSpace < 0) this.#shim.exports.gex_split_windows_space?.();
      // A forked child's memory carries its parent's slot bits (`adoptProcess`).
      if (this.#forkChild) {
        this.#forkChild = false;
        this.#shim.exports.gex_worker_slots_reset?.();
        // Where its inherited mappings end, before the claim below picks its
        // arena floor (`gex_note_inherited`).
        for (const end of this.#inheritedEnds) this.#shim.exports.gex_note_inherited?.(BigInt(end));
        this.#inheritedEnds = [];
      }
      const at = this.#shim.exports.gex_worker_init?.(); 
      // No slot: every one of the process's 64 is held by a live instance.
      // Running on would put this instance's thread-locals at address zero
      // (`__tls_base` unset), over the guest's memory.
      if (at === 0 && (isShared(this.#memory.buffer) || single.on)) {
        throw new Error('gex: no shim stack slot left for this Worker (64 instances of this process are live)');
      }
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
  /** `soname=path,...` (the page's `?wasmdlls=`): libraries replaced by wasm64 providers. */
  static wasmDlls = '';
  /** `name,...` (the page's `?elfunits=`): shared objects that get function-tier units. */
  static elfUnits = '';
  /** `export,...` (the page's `?provskip=`): exports no provider binds (`GEX_PROVIDER_SKIP`). */
  static providerSkip = '';
  /** `+name,-name` (the page's `?fjitlibs=`): objects the function JIT is limited to or kept from. */
  static fjitLibs = '';
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

  /** The global kernel's memory, this process's wait queue's address in it
   *  and where the wait slots' words start, or null: what a compositor walks
   *  to wake this process's parked threads after an input event
   *  (`wl-bridge.mjs` `sendEvent`). */
  kernelWake() {
    const e = this.#shim?.exports;
    if (!this.#kernel || !e?.gex_proc_queue_addr || !this.#kernelSpec?.memory) return null;
    return { memory: this.#kernelSpec.memory, addr: e.gex_proc_queue_addr() >>> 0, slots: e.gex_slots_addr() >>> 0 };
  }

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
    // The census timer too: a fiber (the single-thread build) is not a
    // Worker whose end takes its timers with it.
    if (this.#census) {
      clearInterval(this.#census);
      this.#census = null;
    }
    if (this.#tick) {
      this.#tick.port1.close();
      this.#tick.port2.close();
      this.#tick = null;
    }
  }

  get shim() { return this.#shim; }

  /**
   * Give this instance's stack and thread-local slot back to the process
   * (`gex_worker_release`). The last call into this instance: the slot's next
   * holder runs on the same stack.
   */
  /** This process's memory, every non-zero page zeroed, for another
   *  process to start in; null where that cannot be done (the memory is not
   *  shared, mem64, no global kernel). Only after the process is gone and
   *  nothing will run in this instance again. The non-zero runs are found by
   *  the shim's scan (`gex_image_runs`) and cleared by the kernel's
   *  `memory.fill` (`k_proc_zero`), so only pages the process wrote are
   *  written again. */
  recycleMemory() {
    const e = this.#shim?.exports;
    const k = this.#kernel;
    if (!e?.gex_image_runs || !k?.k_proc_zero || m64() || single.on) return null;
    if (!(this.#memory.buffer instanceof SharedArrayBuffer)) return null;
    const size = this.#memory.buffer.byteLength;
    let cap = 4096;
    let at = e.gex_alloc(cap * 8) >>> 0;
    const runs = [];
    const CHUNK = 64 << 20;
    for (let s = 0; s < size; ) {
      const end = Math.min(size, s + CHUNK);
      const n = e.gex_image_runs(s, end, at, cap);
      if (n < 0) {
        e.gex_free?.(at, cap * 8);
        cap *= 4;
        at = e.gex_alloc(cap * 8) >>> 0;
        continue;
      }
      const pairs = new Uint32Array(this.#memory.buffer, at, n * 2);
      for (let j = 0; j < n; j++) runs.push(pairs[j * 2], pairs[j * 2 + 1]);
      s = end;
    }
    for (let j = 0; j < runs.length; j += 2) {
      if (k.k_proc_zero(runs[j], runs[j + 1]) !== 0) return null;
    }
    return this.#memory;
  }

  releaseWorker() {
    if (this.#stackTop === null) return;
    try { this.#shim?.exports?.gex_worker_release?.(); } catch {}
    this.#stackTop = null;
  }

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
  /// The program's argv as the shim has it (after any `execve`).
  get cmdline() {
    const e = this.#shim?.exports;
    if (!e?.gex_cmdline) return '';
    const max = 160;
    const p = e.gex_alloc(max) >>> 0;
    const n = e.gex_cmdline(p, max) >>> 0;
    const s = utf8(this.#bytes().subarray(p, p + n));
    e.gex_free?.(p, max);
    return s;
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
        // `ptr` is an i64 (a BigInt) from the wasm64 shim, whose thread
        // stacks sit above 4 GiB: cut to 32 bits, a thread's stderr printed
        // as NUL bytes from the wrong place.
        write: (fd, ptr, len) => {
          const at = typeof ptr === 'bigint' ? Number(ptr) : ptr >>> 0;
          this.#env.write(fd, this.#bytes().subarray(at, at + (len >>> 0)));
        },
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
          // `[gex-note]`: worth a line, but nothing went wrong (a large
          // allocation that succeeded): a warning, not an error.
          if (text.startsWith('[gex-note] ')) {
            console.warn(text.slice(11));
            return;
          }
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
        // The clock's hot path (lib.rs `host_now_ns`): an f64 per call, the
        // BigInt origin once per thread. `hostNowNs` is the same sum.
        perf_now: () => performance.now(),
        clock_origin: () => ORIGIN_NS,
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
        call_out: single.on
          ? new WebAssembly.Suspending((ctx, target, ret, spWant) => (this.#hardNested()
            ? this.#callOut(ctx >>> 0, target, ret, spWant)
            : this.#callOutAsync(ctx >>> 0, target, ret, spWant)))
          : (ctx, target, ret, spWant) => this.#callOut(ctx >>> 0, target, ret, spWant),
        // An AOT-only fallback whose target this Worker has just installed
        // (cgen.rs `gex_ftab_fallback`): called as a function body.
        // In the single build, on a stack of its own, so the body (and guest
        // code it calls) can block.
        ftab_call: single.on
          ? new WebAssembly.Suspending((idx, ctx, depth, need, ret) => {
            const f = this.#cgen.ftab.get(tIdx(idx >>> 0));
            return this.#hardNested() ? f(ctx, depth, need, ret) : this.#promisingOf(f)(ctx, depth, need, ret);
          })
          : (idx, ctx, depth, need, ret) => this.#cgen.ftab.get(tIdx(idx >>> 0))(ctx, depth, need, ret),
        // W3: ntdll.so's handler for a guest's Unix call (cgen.rs).
        ftab_call_vi: (idx, arg) => { this.#cgen.ftab.get(tIdx(idx >>> 0))(arg); },
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
        // In the single build a promise (the compositor's Mesa runs in this
        // realm), so the import suspends; never from a nested call-out.
        gl_submit: single.on ? new WebAssembly.Suspending((ring) =>
          (this.#gl
            ? this.#gl.submit(this.#memory, ring >>> 0, this.wlToken(), false, this.#glWaitNote)
            : -1))
          : (ring) =>
            (this.#gl
              ? this.#gl.submit(this.#memory, ring >>> 0, this.wlToken(), false, this.#glWaitNote)
              : -1),
        // The same, not waited for (a frame's last batch): the copy is taken
        // and the guest draws on; the next submit waits for this one, and
        // says how long through `gex_gl_note_wait_ns`.
        gl_submit_async: single.on ? new WebAssembly.Suspending((ring) =>
          (this.#gl
            ? this.#gl.submit(this.#memory, ring >>> 0, this.wlToken(), true, this.#glWaitNote)
            : -1))
          : (ring) =>
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
        // Whether that ring holds anything, without draining it.
        wl_input_pending: () => (this.#wl?.inputPending() ? 1 : 0),
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
    // A W3 program (native Wine's loader) splits the memory with Windows
    // before the runtime's startup heap is taken, or that heap sits where
    // Windows DLLs load (0x10000000).
    if (isW3Program(elf)) this.#windowsSpace = -1;
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
      'gex_chmod', 'gex_add_dir', 'gex_add_env', 'gex_add_symlink',
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
    this.#providerSkipApply();
    if (this.#noElfProviders) this.#shim.exports.gex_elfprov_off?.();
    this.#libprovConfig();
    this.#shim.exports.gex_set_wl?.(this.#wl ? 1 : 0);
    if (this.#wlOut) this.#shim.exports.gex_set_wl_output?.(...this.#wlOut);
    if (this.#wlOut) this.#shim.exports.gex_set_wl_scale?.(this.#wlScale);
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
      // The translator's knobs, for every process of the run: set only here,
      // a forked child (node under `bash -c`) ran with the defaults.
      perMiss: opts.perMiss, liftMax: opts.liftMax, inlineDispatch: opts.inlineDispatch,
      xlateReport: opts.xlateReport, discoverSkip: opts.discoverSkip, t1Fold: opts.t1Fold,
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
    // How many blocks one dispatch miss lifts (default 2048).
    if (opts.liftMax) this.#shim.exports.gex_set_lift_max?.(opts.liftMax);
    if (opts.inlineDispatch) this.#shim.exports.gex_set_inline_dispatch?.(1);
    if (opts.xlateReport) this.#shim.exports.gex_set_xlate_report?.(opts.xlateReport);
    if (opts.discoverSkip) this.#shim.exports.gex_set_discover_skip?.(opts.discoverSkip);
    if (opts.t1Fold) this.#shim.exports.gex_set_t1_fold?.(1);
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
    // Files fetched as they are read (`gex_add_lazy`): [path, size, url],
    // after the bundle's, which win.
    for (const [path, size, url] of opts.lazy ?? []) {
      if (!this.#shim.exports.gex_add_lazy) break;
      if (files[path] !== undefined) continue;
      const p = enc.encode(path);
      const u = enc.encode(url);
      const pp = this.#shim.exports.gex_alloc(p.length) >>> 0;
      const up = this.#shim.exports.gex_alloc(u.length) >>> 0;
      this.#bytes().set(p, pp);
      this.#bytes().set(u, up);
      this.#shim.exports.gex_add_lazy(pp, p.length, up, u.length, BigInt(size));
      free?.(pp, p.length);
      free?.(up, u.length);
    }
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
    // Symbolic links (`web/src/tar.js`'s `links`), after files and hard links:
    // path -> target text. Only with the global kernel (a page or run-node):
    // a process that is the whole system has no filesystem yet to link in,
    // so a caller without one passes copies as files instead.
    for (const [path, target] of Object.entries(opts.symlinks ?? {})) {
      const p = enc.encode(path);
      const t = enc.encode(target);
      const pp = this.#shim.exports.gex_alloc(p.length) >>> 0;
      const tp = this.#shim.exports.gex_alloc(t.length) >>> 0;
      this.#bytes().set(p, pp);
      this.#bytes().set(t, tp);
      if (!this.#shim.exports.gex_add_symlink(tp, t.length, pp, p.length)) {
        console.warn(`gex: symlink ${path} -> ${target} not made (no global kernel)`);
      }
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
    // `?xcache=`: the store the last load saved, laid in before anything runs
    // (docs/gexcc.md §8). After `gex_load`, which unpacked the bundle the
    // saved paths are checked against.
    if (opts.xcache) await this.#xcacheRestore(String(opts.xcache));
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
        : this.#guestSyscall(e),
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
      safepoint: single.on ? this.#singleSafepoint(e) : e.gex_safepoint,
      fma: e.gex_fma,
      x87: e.gex_x87,
      // A counting unit's table calls (cgen.rs `gex_icall_note`).
      icall_note: e.gex_icall_note,
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
    // A test switch (tests/x86-programs/page-cross-entry.c): a retired page
    // left numbered with no directory entry.
    if (globalThis.GEX_TEST_UNPUBLISHED_RETIRE || globalThis.process?.env?.GEX_TEST_UNPUBLISHED_RETIRE) e.gex_test_unpublished_retire?.(1);
    // Another (tests/x86-programs/served-entries.c): served modules are
    // installed one at a time, as when the merger declines them.
    if (globalThis.GEX_TEST_SERVE_UNMERGED || globalThis.process?.env?.GEX_TEST_SERVE_UNMERGED) e.gex_test_serve_unmerged?.(1);
    // The function JIT (fjit.rs): its translations reach the function table,
    // which the function tier's plumbing drives (units where any exist).
    if (opts.fjit && e.gex_set_fjit) {
      e.gex_set_fjit(opts.fjit === 2 ? 2 : 1);
      if (opts.fjitMax != null || opts.fjitOnly) e.gex_fjit_limit?.(opts.fjitMax ?? 0xffffffff, BigInt(opts.fjitOnly ?? 0));
      if (!(this.#cgenMode & 3)) this.useCgen(this.#cgenMode | 1);
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
    // `m.dense`: the parent's own numbering (`#compiled`); the Module's
    // section may be another process's (the unit cache).
    const ds = this.#dense ? (m.dense ?? denseSection(m.mod)) : null;
    if (globalThis.GEX_DENSE_DEBUG && ds && m.dense) {
      const own = denseSection(m.mod);
      if (own && new DataView(own.buffer, own.byteOffset).getUint32(0, true) !== new DataView(ds.buffer, ds.byteOffset).getUint32(0, true)) {
        console.error(`[dense] adopt: a module compiled elsewhere (base ${new DataView(own.buffer, own.byteOffset).getUint32(0, true)}) at this process's base ${new DataView(ds.buffer, ds.byteOffset).getUint32(0, true)}`);
      }
    }
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
      await this.#installModule(m.mod, truly, 64, m.meta, 'adopt-nometa', null, false, m.dense);
      return;
    }
    const gp = e.gex_alloc(m.meta.length) >>> 0;
    this.#bytes().set(m.meta, gp);
    const need = e.gex_add_lazy_module(gp, m.meta.length) >>> 0;
    e.gex_free?.(gp, m.meta.length);
    if (!need) {
      await this.#installModule(m.mod, truly, 64, m.meta, 'adopt-noneed', null, false, m.dense);
      return;
    }
    this.#growTables(Math.max(need, truly), 'adopt');
    this.#lazyInherited.push({ mod: m.mod, need: truly, inst: null, meta: m.meta, dense: m.dense });
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
      if (this.#wlOut) this.#shim.exports.gex_set_wl_output?.(...this.#wlOut);
      if (this.#wlOut) this.#shim.exports.gex_set_wl_scale?.(this.#wlScale);
      this.#shim.exports.gex_set_network?.(this.#net ? 1 : 0);
      // The unit cache and ELF export providers, as `load` gives a process its
      // own: a forked child's table starts empty while its GOT holds what
      // its parent's providers made code (cgen/elfprov.rs).
      if (this.#cgenMode) this.#shim.exports.gex_cgen_set_mode?.(this.#cgenMode);
      this.#providerSkipApply();
      if (this.#noElfProviders) this.#shim.exports.gex_elfprov_off?.();
      this.#libprovConfig();
    // Narrate one page's directory writes, in every process. See #136.
    if (Gex.watchDir >= 0) this.#shim.exports.gex_watch_dir?.(BigInt(Gex.watchDir));
    }
    const e = this.#shim.exports;
    this.#imports = {
      memory: this.#memory,
      syscall: this.#guestSyscall(e),
      dispatch_miss: e.gex_dispatch_miss,
      illegal: e.gex_illegal,
      breakpoint: e.gex_breakpoint,
      sysreg_read: e.gex_sysreg_read,
      sysreg_write: e.gex_sysreg_write,
      cache_op: e.gex_cache_op,
      safepoint: single.on ? this.#singleSafepoint(e) : e.gex_safepoint,
      fma: e.gex_fma,
      x87: e.gex_x87,
      // A counting unit's table calls (cgen.rs `gex_icall_note`).
      icall_note: e.gex_icall_note,
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
  /** Copy the runs `#processImage` left as `len` into one staging buffer,
   *  noting each one's offset (`stage`) and the buffer (`#imageStage`, sent
   *  as the record's `imageStage`); the child copies them out and frees it.
   *  Sliced as before when the kernel cannot hold them. */
  #stageImage(out) {
    const k = this.#kernel;
    const staged = out.filter((r) => r.len !== undefined);
    const total = staged.reduce((n, r) => n + r.len, 0);
    let id = total ? k.k_stage_take(total) : 0;
    let off = 0;
    for (const r of id > 0 ? staged : []) {
      if (k.k_stage_pull(id, off, r.at, r.len) !== 0) {
        k.k_stage_drop(id);
        id = 0;
        break;
      }
      r.stage = off;
      off += r.len;
    }
    if (id > 0) {
      this.#imageStage = id;
      return;
    }
    const mem = this.#bytes();
    for (const r of staged) {
      r.bytes = mem.slice(r.at, r.at + r.len);
      delete r.len;
      delete r.stage;
    }
  }

  /// The staging buffer of the image `#processImage` built last (0: none).
  #imageStage = 0;

  #processImage() {
    const t0 = performance.now();
    const e = this.#shim.exports;
    // Only the runs of pages that are not all zero, scanned in place: a
    // child's memory starts zeroed, so a zero page needs no bytes, and
    // slicing it whole first (637 MiB of zeros for a node child) was most of
    // the cost of a fork, paid again by the child scanning the copy for
    // zeros (`writeSparse`). Each mapped region also goes as an entry of its
    // own with no bytes (`end`, `prot`, and `zeros`, how much of it was
    // left out); runs are `dense`, written without a scan.
    const u8 = this.#bytes();
    // Division, not `>> 2` or `& ~3`: those are int32 operations, and a W3
    // process's memory reaches past 2 GiB (Wine's Windows address space), where
    // they turn negative -- `b & ~3` then started the tail loop near -2^31, and
    // a fork (Wine starting wineserver) scanned for hours.
    const words = new Int32Array(this.#memory.buffer, 0, Math.floor(u8.length / 4));
    const blank = (a, b) => {
      const tail = b - (b % 4);
      for (let i = Math.floor(a / 4); i < tail / 4; i++) if (words[i] !== 0) return false;
      for (let i = tail; i < b; i++) if (u8[i] !== 0) return false;
      return true;
    };
    const out = [];
    // The runs go through a staging buffer in the global kernel's memory where
    // there is one to share (`k_stage_*`): `memory.copy` both ways, into pages
    // a previous fork already touched, where a fresh ArrayBuffer per run cost
    // ~50 ms here and ~100 ms in the child for apt-get's 62.6 MiB. Not with
    // `GEX_DEBUG_EXIT`, whose checksums read the bytes; not in a build whose
    // memories are not shared (the single-thread page), which transfers them.
    const k = this.#kernel;
    const staging = !globalThis.GEX_DEBUG_EXIT && !m64() && !!k?.k_stage_take
      && this.#memory.buffer instanceof SharedArrayBuffer
      && this.#kernelSpec?.memory?.buffer instanceof SharedArrayBuffer;
    this.#imageStage = 0;
    // The scan in the shim (`gex_image_runs`, 16 bytes a load) when it has
    // one; the pairs it writes are the runs of pages that are not all zero.
    let runsCap = 0;
    let runsAt = 0;
    const runsWasm = e.gex_image_runs
      ? (start, end) => {
        for (;;) {
          if (!runsCap) {
            runsCap = 1024;
            runsAt = e.gex_alloc(runsCap * 8) >>> 0;
          }
          const n = e.gex_image_runs(start, end, runsAt, runsCap);
          if (n >= 0) {
            // Views made now: the allocation above may have grown memory.
            const pairs = new Uint32Array(this.#memory.buffer, runsAt, n * 2);
            const mem = this.#bytes();
            let dense = 0;
            for (let j = 0; j < n; j++) {
              const at = pairs[j * 2];
              const len = pairs[j * 2 + 1];
              out.push(staging ? { at, len, dense: true } : { at, bytes: mem.slice(at, at + len), dense: true });
              dense += len;
            }
            return end - start - dense;
          }
          e.gex_free?.(runsAt, runsCap * 8);
          runsCap *= 4;
          runsAt = e.gex_alloc(runsCap * 8) >>> 0;
        }
      }
      : null;
    const runs = runsWasm ?? ((start, end) => {
      const PAGE = 4096;
      let zeros = 0;
      let i = start;
      while (i < end) {
        const pe = Math.min(i + PAGE, end);
        if (blank(i, pe)) {
          zeros += pe - i;
          i = pe;
          continue;
        }
        let run = pe;
        while (run < end) {
          const next = Math.min(run + PAGE, end);
          if (blank(run, next)) break;
          run = next;
        }
        out.push({ at: i, bytes: u8.slice(i, run), dense: true });
        i = run;
      }
      return zeros;
    });
    out.push({ at: 0, bytes: new Uint8Array(0), zeros: runs(0, LOW_END) });
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
      if (end > start) {
        const r = { at: start, end, prot, bytes: new Uint8Array(0), zeros: 0 };
        out.push(r);
        r.zeros = runs(start, end);
      }
    }
    e.gex_free?.(scratch, 24);
    if (runsCap) e.gex_free?.(runsAt, runsCap * 8);
    if (staging) this.#stageImage(out);
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
      env: { memory, process: this.#memory, lazy_fetch: lazyFetch(memory) },
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
        if (text === last) narrate(`[gex] quiet: ${text}${single.on ? ` js=${this.#where}/installs=${this.#installs}/pending=${e.gex_jit_pending_len() >>> 0}` : ''}`);
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

  /** The saved store, into the kernel and checked (docs/gexcc.md §8). */
  async #xcacheRestore(name) {
    const e = this.#shim.exports;
    const k = this.#kernel;
    if (!k?.k_store_restore_buf || !e.gex_xcache_restore) return;
    k.k_xcache_enable();
    const t0 = performance.now();
    const bytes = await xcRead(name);
    if (!bytes) {
      console.log(`[gex] xcache ${name}: nothing saved`);
      return;
    }
    const t1 = performance.now();
    const at = k.k_store_restore_buf(bytes.length) >>> 0;
    new Uint8Array(this.#kernelSpec.memory.buffer).set(bytes, at);
    const r = e.gex_xcache_restore();
    const t2 = performance.now();
    console.log(`[gex] xcache ${name}: ${(bytes.length / 1048576).toFixed(1)} MiB read in ${Math.round(t1 - t0)} ms, `
      + `restored ${r} modules in ${Math.round(t2 - t1)} ms`);
  }

  /**
   * Save the kernel's store for the next load if it grew by `minBytes` since
   * the last save (docs/gexcc.md §8). The kernel lets one save run at a time.
   */
  async xcacheSave(name, minBytes = 1 << 20) {
    const k = this.#kernel;
    if (!name || !k?.k_store_save) return;
    const t0 = performance.now();
    const n = k.k_store_save(minBytes) >>> 0;
    if (!n) return;
    try {
      const at = k.k_store_save_ptr() >>> 0;
      const bytes = new Uint8Array(this.#kernelSpec.memory.buffer, at, n).slice();
      k.k_store_save_done();
      await xcWrite(String(name), bytes);
      console.log(`[gex] xcache ${name}: saved ${(n / 1048576).toFixed(1)} MiB in ${Math.round(performance.now() - t0)} ms`);
    } catch (err) {
      k.k_store_save_done();
      console.log(`[gex] xcache ${name}: not saved: ${err?.message ?? err}`);
    }
  }

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
      'k_sock_read', 'k_sock_write', 'k_sock_readable', 'k_sock_writable', 'k_sock_hup',
      'k_sock_available', 'k_sock_close', 'k_sock_shutdown', 'k_sock_exists',
      'k_ws_self', 'k_ws_seq', 'k_ws_park', 'k_ws_addr',
      'k_wq_new', 'k_wq_free', 'k_wq_notify', 'k_wq_chain', 'k_wq_unchain',
      'k_wq_add', 'k_wq_del', 'k_wq_addr',
      'k_pipe_queue', 'k_sock_queue', 'k_pty_queue', 'k_proc_queue', 'k_stdin_queue', 'k_futex_queue',
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
    // The single-thread build: the wait words are promises (single.mjs
    // `Sched`). A park that need not wait answers at once, as
    // `memory.atomic.wait32` does when the value has moved; one that must
    // suspends this fiber's stack. Every wake also wakes the fibers.
    if (k && single.on) {
      const nested = () => {
        throw new Error('gex (single-thread build): a blocking call inside a nested guest call '
          + `(${this.#nest.join(' > ')}) cannot suspend`);
      };
      // A wait slot's word is in the kernel's (plain) memory, which can
      // grow: the view is asked for each time.
      const words = () => new Int32Array(this.#kernelSpec.memory.buffer);
      out.k_ws_park = new WebAssembly.Suspending((slot, seq, ns) => {
        if (k.k_ws_seq(slot) !== seq) return 1;
        if (this.#hardNested()) nested();
        this.#where = `park(${ns})`;
        return sched.parkSlot(words, (k.k_ws_addr(slot) >>> 0) >>> 2, seq, ns);
      });
      // A notify moved slot words nobody `Atomics.wait`s on here; the fibers
      // parked on them look (and every park and yield looks too, for the
      // notifies inside the kernel's own calls).
      out.k_wq_notify = (id) => {
        k.k_wq_notify(id);
        sched.checkSlots();
      };
      out.k_futex_wake = (addr) => {
        k.k_futex_wake(addr);
        sched.checkSlots();
      };
    }
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
  /** `bytes` into the shim's heap for the length of `f(ptr, len)`. */
  #putBytes(bytes, f) {
    const e = this.#shim.exports;
    const n = Math.max(1, bytes.length);
    const p = e.gex_alloc(n) >>> 0;
    this.#bytes().set(bytes, p);
    try {
      return f(p, bytes.length);
    } finally {
      e.gex_free?.(p, n);
    }
  }

  /**
   * posix_spawn's child, whose exec was staged while it ran in its parent's
   * Worker (`#vforkChild`): a fresh memory, the kernel a forked child gets
   * with its descriptors and signal state, and the exec run here. The
   * entry, or 0n when the program could not be loaded (the process has
   * then exited).
   */
  async #adoptExec(shimWasm, guestWasm, child, opts) {
    this.#windowsSpace = 0;
    await this.attach(shimWasm, guestWasm);
    const e = this.#shim.exports;
    this.#setMetadata(guestWasm);
    const o = child.opts ?? {};
    const uid = o.uid ?? child.uid;
    if (uid !== undefined) e.gex_set_identity(uid, o.gid ?? child.gid ?? uid);
    if (child.cwd) this.#putBytes(new TextEncoder().encode(child.cwd), e.gex_set_cwd);
    if (child.state?.length) this.#putBytes(child.state, e.gex_set_child_state);
    e.gex_adopt_child(child.pid, child.ctx, 0n, 0n, 0n, 0n);
    this.#dense = !!o.dense && !!this.#guest.exports.blocks && !!e.gex_set_dense;
    e.gex_set_dense?.(this.#dense ? (o.dense === 2 ? 2 : 1) : 0);
    this.#opts = o;
    syncSlotShift(e);
    this.#childOpts(e, o, child.pid);
    if (child.fds?.length) this.#putBytes(child.fds, e.gex_adopt_fds);
    this.#closureModules = opts?.closure ?? this.#closureModules;
    this.#wantLibrary = opts?.wantLibrary ?? this.#wantLibrary;
    await this.#installClosure();
    return this.#putBytes(child.exec, e.gex_adopted_exec);
  }

  /** A child's run options, said to its kernel (`adoptProcess`). */
  #childOpts(e, o, pid) {
    // `?strace=1` narrates every process; `?stracepid=N` only that one, which
    // is what a run that must stay alive can afford.
    if (o.strace || (o.stracePid && o.stracePid === pid)) e.gex_set_trace(1);
    if (o.traceBlocks) e.gex_set_trace_blocks?.(1);
    if (o.watchMem) e.gex_set_watch_mem?.(BigInt(o.watchMem));
    if (o.traceMisses) e.gex_set_trace_misses?.(1);
    if (o.checkSp) e.gex_set_check_sp?.(1);
    if (o.noIntrinsics) e.gex_set_intrinsics?.(0);
    if (o.intrinsicMask !== undefined) e.gex_set_intrinsic_mask?.(o.intrinsicMask);
    if (o.perMiss) e.gex_set_per_miss?.(o.perMiss);
    if (o.liftMax) e.gex_set_lift_max?.(o.liftMax);
    if (o.inlineDispatch) e.gex_set_inline_dispatch?.(1);
    if (o.xlateReport) e.gex_set_xlate_report?.(o.xlateReport);
    if (o.discoverSkip) e.gex_set_discover_skip?.(o.discoverSkip);
    if (o.t1Fold) e.gex_set_t1_fold?.(1);
    if (o.tty) e.gex_set_tty(1);
  }

  async adoptProcess(shimWasm, guestWasm, child, opts = {}) {
    // posix_spawn's child, run in its parent's Worker until its exec: no image.
    if (child.exec) return this.#adoptExec(shimWasm, guestWasm, child, opts);
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
    this.#growTo(child.image.reduce((n, r) => Math.max(n, r.end ?? 0, r.at + imageLen(r)), 0));
    // A staged image (`#stageImage`) is copied out by the global kernel, here
    // before `attach` as the transferred one is: its instance only needs this
    // memory, and starting it again there is a no-op.
    const stage = child.imageStage > 0 ? child.imageStage : 0;
    if (stage) await this.#startKernel();
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
      const n = imageLen(r);
      if (n > skipOver) {
        skipped += n;
        continue;
      }
      zeros += r.zeros ?? 0;
      if (r.stage !== undefined) {
        if (this.#kernel.k_stage_push(stage, r.stage, r.at, n) !== 0) {
          throw new Error(`fork image: staged run at 0x${r.at.toString(16)} (${n} bytes) did not copy`);
        }
        copied += n;
        continue;
      }
      if (r.dense) {
        u8.set(new Uint8Array(r.bytes), r.at);
        copied += n;
        continue;
      }
      const [wrote, blank] = writeSparse(u8, new Uint8Array(r.bytes), r.at);
      copied += wrote;
      zeros += blank;
    }
    if (stage) this.#kernel.k_stage_drop(stage);
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
    this.#windowsSpace = child.windowsSpace ?? 0;
    this.#forkChild = true;
    this.#inheritedEnds = child.image.map((r) => Math.max(r.end ?? 0, r.at + imageLen(r)));
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
    // The parent's program and argv (`/proc/self/exe`, `cmdline`), likewise.
    if (child.exe && e.gex_set_child_exe) {
      const c = new TextEncoder().encode(child.exe);
      const cp = e.gex_alloc(Math.max(1, c.length)) >>> 0;
      this.#bytes().set(c, cp);
      e.gex_set_child_exe(cp, c.length);
      e.gex_free?.(cp, Math.max(1, c.length));
    }
    if (child.cwd && e.gex_set_cwd) {
      const c = new TextEncoder().encode(child.cwd);
      const cp = e.gex_alloc(c.length) >>> 0;
      this.#bytes().set(c, cp);
      e.gex_set_cwd(cp, c.length);
      e.gex_free?.(cp, c.length);
    }
    // And the signal state, umask and limits, which `gex_adopt_child` builds
    // the kernel with.
    if (child.state?.length && e.gex_set_child_state) this.#putBytes(child.state, e.gex_set_child_state);
    const entry = e.gex_adopt_child(
      child.pid,
      child.ctx,
      child.pc,
      child.sp,
      child.tls,
      child.brk ?? 0n,
    );
    if (child.compat) e.gex_set_compat?.(1);
    if (child.wasm32) {
      if (child.wasm32Link && e.gex_wasmprog_adopt_link) {
        const b = new TextEncoder().encode(child.wasm32Link);
        const p = e.gex_alloc(b.length) >>> 0;
        this.#bytes().set(b, p);
        e.gex_wasmprog_adopt_link(p, b.length);
        e.gex_free?.(p, b.length);
      }
      if (child.wasm32Base && e.gex_wasmprog_adopt_at) e.gex_wasmprog_adopt_at(BigInt(child.wasm32), child.wasm32Base);
      else e.gex_wasmprog_adopt?.(BigInt(child.wasm32));
    }
    // The parent's block numbering carries on in the child (its low region),
    // over slot maps of its own (`dense::child`).
    this.#dense = !!(child.opts ?? {}).dense && !!this.#guest.exports.blocks && !!e.gex_set_dense;
    e.gex_set_dense?.(this.#dense ? ((child.opts ?? {}).dense === 2 ? 2 : 1) : 0);
    step(`kernel built, entry 0x${entry.toString(16)}, ${live()} non-zero bytes in the context`);
    // The kernel exists now, so everything that configures one can be said.
    const o = child.opts ?? {};
    this.#opts = o;
    syncSlotShift(e);
    this.#childOpts(e, o, child.pid);
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
          + ` image=${(child.image ?? []).reduce((n, r) => n + imageLen(r), 0)}`
          + ` modules=${(child.modules ?? []).length}`,
      );
    }
    // Only then: the measurement forces a GC in every child, which an
    // ordinary run (apt's hundred forks) pays for and never reads.
    if (globalThis.GEX_DEBUG_EXIT && performance.measureUserAgentSpecificMemory) {
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
    // Made before the first thread exists, so every Worker of the process
    // holds the same one.
    this.#moduleStore ??= makeModuleStore();
    const thread = {
      tid,
      ctx: e.gex_thread_ctx(),
      pc: e.gex_thread_pc(),
      memory: this.#memory,
      moduleStore: this.#moduleStore ?? undefined,
      // A thread of a 32-bit process decodes in compatibility mode too.
      compat: e.gex_compat?.() ?? 0,
      // The process's run options, so a fork from this thread hands its
      // child the same ones (the fork record's `opts`): a thread Worker had
      // none, and a child of a thread lost dense numbering and the rest.
      procOpts: this.#opts,
    };
    if (globalThis.GEX_ATTACH_TRACE) console.error(`[spawn] #drainThread posting tid ${tid} (t=${Date.now()})`);
    e.gex_thread_taken();
    // A thread needs a memory two Workers can both hold. Posting a non-shared
    // memory does not fail -- the structured clone copies it, and the thread
    // runs on a private image nothing else can see -- so it is refused here.
    // In the single-thread build a thread is a fiber of this realm, handed
    // the memory object itself.
    if (!isShared(this.#memory?.buffer) && !single.on) {
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
    if (thread.procOpts) this.#opts = thread.procOpts;
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

  /**
   * posix_spawn's child (a wasm32 program's CLONE_VM | CLONE_VFORK clone),
   * run here before the parent's `clone` returns, as Linux runs it: in the
   * parent's memory, the parent suspended (shims/browser/src/vfork.rs). The
   * child's frames end when it exits or execs, an exec going on in a new
   * Worker; the parent's process state comes back, and so do the program's
   * globals, which the child's entry set. Whether a child ran.
   */
  #vforkChild() {
    const e = this.#shim.exports;
    const saved = {
      shimSp: e.__stack_pointer.value,
      progSp: this.#wasmprogSp?.value,
      async: this.#wasmprogAsync?.map((g) => g.value),
      muts: this.#wasmprogMuts.map((g) => g.value),
    };
    const t0 = performance.now();
    const entry = e.gex_vfork_begin();
    if (!entry) return false;
    const pid = e.gex_vfork_pid();
    const ctx = Number(e.gex_fork_ctx());
    let why = 'exit';
    let crash = null;
    try {
      this.#guest.exports.enter(ctx, entry);
    } catch (err) {
      if (!e.gex_handoff?.()) {
        why = 'crash';
        crash = err;
      } else {
        const w = e.gex_vfork_why();
        why = w === 2 ? 'exec' : w === 3 ? 'crash' : 'exit';
      }
    }
    // A trap abandons the shim's frames without restoring its stack pointer.
    e.__stack_pointer.value = saved.shimSp;
    // Frames that ended with no exit said: nothing will tell the parent.
    if (why === 'exit' && !e.gex_exited?.()) {
      why = 'crash';
      crash = new Error('its frames ended without an exit');
    }
    let exec = null;
    if (why === 'exec') {
      exec = this.#vforkExecRecord(pid, ctx);
      e.gex_vfork_exec_done();
    } else if (why === 'crash') {
      if (crash) console.error(`[gex] pid ${pid} (posix_spawn's child) crashed: ${crash?.message ?? crash}`);
      // As a trapped Worker is reported.
      try { e.gex_process_gone?.(137); } catch {}
    }
    e.gex_vfork_end();
    if (this.#wasmprogSp && saved.progSp !== undefined) this.#wasmprogSp.value = saved.progSp;
    saved.async?.forEach((v, i) => { this.#wasmprogAsync[i].value = v; });
    saved.muts.forEach((v, i) => { this.#wasmprogMuts[i].value = v; });
    if (exec) {
      this.#env.spawnProcess(exec).catch((err) => console.error(`[gex] starting pid ${pid} after its exec: ${err.message ?? err}`));
    }
    this.#vforks++;
    vforkTrace(`[vfork] pid ${pid}: ${why} after ${(performance.now() - t0).toFixed(1)}ms`);
    return true;
  }

  /** The record of an exec a vfork child staged (`vfork::stage_exec`): a
   *  fork record with no image, which the host starts as any child, and
   *  whose Worker runs the exec (`#adoptExec`). */
  #vforkExecRecord(pid, ctx) {
    const e = this.#shim.exports;
    const part = (i) => {
      const p = e.gex_vfork_exec_ptr(i) >>> 0;
      return this.#bytes().slice(p, p + (e.gex_vfork_exec_len(i) >>> 0));
    };
    return {
      pid,
      ctx: BigInt(ctx),
      pc: 0n,
      sp: 0n,
      tls: 0n,
      compat: 0,
      wasm32: 0n,
      brk: 0n,
      windowsSpace: 0,
      image: [],
      vmaBlob: null,
      slots: 0,
      pages: null,
      shared: null,
      modules: [],
      opts: this.#opts,
      exec: part(0),
      fds: part(1),
      cwd: new TextDecoder().decode(part(2)),
      state: part(3),
      exe: null,
    };
  }

  /** How many posix_spawn children ran in this Worker (`#vforkChild`). */
  #vforks = 0;
  get vforks() {
    return this.#vforks;
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
      // And where its module sits in the function table: the child's copy
      // of the data holds function pointers for that base.
      wasm32Base: e.gex_wasmprog_base?.() ?? 0,
      // And its shared libraries: where each module's data and table range are.
      wasm32Link: this.#wasmprogLink(),
      // Where the parent's heap had reached. Without the number the child's
      // kernel thinks it has no heap and grows one from zero, over its contexts.
      brk: e.gex_fork_brk?.() ?? 0n,
      // A W3 process's Windows address space starts here (0: none); the
      // child splits its memory at the same line before its heap takes any.
      windowsSpace: e.gex_windows_space?.() ?? 0,
      image: (() => {
        const t0 = performance.now();
        const img = this.#processImage();
        this.#imageMs = performance.now() - t0;
        this.#imageBytes = img.reduce((n, r) => n + imageLen(r), 0);
        this.#imageRegions = img.length;
        return img;
      })(),
      // Where the image's runs are staged, in the global kernel's memory (0:
      // they travel as `bytes`). The child copies them out and frees it.
      imageStage: this.#imageStage,
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
      // The dispositions, mask, umask and limits the child inherits.
      state: e.gex_fork_state_len?.()
        ? this.#bytes().slice(
            e.gex_fork_state_ptr() >>> 0,
            (e.gex_fork_state_ptr() >>> 0) + (e.gex_fork_state_len() >>> 0),
          )
        : null,
      // The parent's exe path and argv, NUL-separated.
      exe:
        e.gex_fork_exe_len && e.gex_fork_exe_len() > 0
          ? new TextDecoder().decode(
              this.#bytes().slice(
                e.gex_fork_exe_ptr() >>> 0,
                (e.gex_fork_exe_ptr() >>> 0) + (e.gex_fork_exe_len() >>> 0),
              ),
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
      // Copied out before compiling: the registry is in the guest's memory,
      // and another thread allocating can move it.
      const bytes = this.#registryBytes(i);
      if (!bytes) continue;
      const ptr = this.#registryRow(i).ptr, len = bytes.length;
      // A sibling Worker may already have compiled this module: reuse its
      // `WebAssembly.Module` (same registry bytes -> same module) rather than
      // paying `WebAssembly.compile` again. The bytes are still needed for the
      // metadata and table-need computation the install does regardless.
      const key = this.#registryKey(i, bytes);
      // Or one the page's unit cache has (another process's).
      const cached = (key ? this.#sharedModules.get(key) : undefined) ?? this.#unitGet(this.#unitKey(bytes)) ?? undefined;
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
      const bytes = this.#registryBytes(i);
      if (!bytes) continue;
      const meta = metadataSection(bytes);
      // No metadata means nothing to key a later fill on -- install it eagerly,
      // as the fork path does (`#adoptModule`), reusing a sibling's compile.
      if (!meta || !meta.length) {
        await this.#installModule(bytes, need, 64, null, 'lazy-nometa', this.#sharedModules.get(this.#registryKey(i, bytes)));
        eager++;
        continue;
      }
      const gp = e.gex_alloc(meta.length) >>> 0;
      this.#bytes().set(meta, gp);
      const got = e.gex_add_lazy_module(gp, meta.length) >>> 0;
      e.gex_free?.(gp, meta.length);
      if (!got) {
        await this.#installModule(bytes, need, 64, meta, 'lazy-noneed', this.#sharedModules.get(this.#registryKey(i, bytes)));
        eager++;
        continue;
      }
      // Keep a sibling's compiled Module when we have it, so the on-demand
      // instantiate skips the compile too; otherwise the raw bytes. The push
      // order matches `gex_add_lazy_module`'s ordinal (both count from 0 here).
      this.#lazyInherited.push({ mod: this.#sharedModules.get(this.#registryKey(i, bytes)) ?? bytes, need, inst: null, meta });
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
    // Its events end a network wait (`#netWait`). Chained: the single-thread
    // build's scheduler wake is set before this.
    if (net && 'onActivity' in net) {
      const prev = net.onActivity;
      net.onActivity = (...a) => {
        prev?.apply(net, a);
        const r = this.#netWake;
        this.#netWake = null;
        r?.();
      };
    }
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
  /** Exports the built-in providers leave alone (`?provskip=a,b`, as
   *  GEX_PROVIDER_SKIP natively): every process of the run, before the
   *  shim keys its units. */
  providerSkip(list) {
    this.#providerSkip = String(list ?? '');
    if (this.#shim) this.#providerSkipApply();
  }
  #providerSkip = '';
  #providerSkipApply() {
    const e = this.#shim.exports;
    if (!this.#providerSkip || !e.gex_cgen_set_provider_skip) return;
    const b = new TextEncoder().encode(this.#providerSkip);
    const p = e.gex_alloc(Math.max(8, b.length)) >>> 0;
    this.#bytes().set(b, p);
    e.gex_cgen_set_provider_skip(p, b.length);
    e.gex_free?.(p, Math.max(8, b.length));
  }
  /** No ELF export providers (cgen/elfprov.rs): A/B runs, every process of
   *  the run (a forked child's Worker is told too). Before the shim exists. */
  /** `?wasmdlls=soname=path,...`: libraries this process replaces with wasm64
   *  providers (cgen/libprov.rs). */
  #libprovConfig() {
    const e = this.#shim.exports;
    if (Gex.fjitLibs && e.gex_fjit_libs) {
      const b = new TextEncoder().encode(Gex.fjitLibs);
      const p = e.gex_alloc(Math.max(8, b.length)) >>> 0;
      this.#bytes().set(b, p);
      e.gex_fjit_libs(p, b.length);
      e.gex_free?.(p, Math.max(8, b.length));
    }
    if (Gex.providerSkip && e.gex_provider_skip) {
      const b = new TextEncoder().encode(Gex.providerSkip);
      const p = e.gex_alloc(Math.max(8, b.length)) >>> 0;
      this.#bytes().set(b, p);
      e.gex_provider_skip(p, b.length);
      e.gex_free?.(p, Math.max(8, b.length));
    }
    if (Gex.elfUnits && e.gex_cgen_elf_units) {
      const b = new TextEncoder().encode(Gex.elfUnits);
      const p = e.gex_alloc(Math.max(8, b.length)) >>> 0;
      this.#bytes().set(b, p);
      e.gex_cgen_elf_units(p, b.length);
      e.gex_free?.(p, Math.max(8, b.length));
    }
    if (!Gex.wasmDlls || !e.gex_libprov_config) return;
    const b = new TextEncoder().encode(Gex.wasmDlls);
    const p = e.gex_alloc(Math.max(8, b.length)) >>> 0;
    this.#bytes().set(b, p);
    e.gex_libprov_config(p, b.length);
    e.gex_free?.(p, Math.max(8, b.length));
  }

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
    // A table64 beside a 64-bit memory: every module of such a process (units, translations,
    // providers) addresses its function table with i64.
    const ftab = new WebAssembly.Table(m64() ? { element: 'anyfunc', address: 'i64', initial: 0n } : { element: 'anyfunc', initial: 0 });
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
    this.#cgen = { ftab, resolve: ri.exports.resolve, bridge: ri.exports.bridge, lookup, have: 0, instances: [ri], dyl: new Map() };
    this.#cgenGrow();
    ftab.set(tIdx(1), e.gex_ftab_fallback);
    return this.#cgen;
  }

  /** Grow this Worker's function table past every index handed out, with
   *  room over, so an index another Worker published to the probe cache is
   *  in range here (holding `resolve`) before this one replays it. */
  #cgenGrow() {
    const st = this.#cgen;
    const want = (this.#shim.exports.gex_ftab_size() >>> 0) + 65536;
    const have = Number(st.ftab.length);
    if (have < want) st.ftab.grow(tIdx(want - have), st.resolve);
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
    // A replaced Linux library's wasm64 provider (cgen/libprov.rs): the 64-bit provider_* imports.
    const libprov = wasmdll && !!e.gex_cgen_pmod_libprov?.(prov);
    // One built for wasm64 and lowered to this 32-bit memory (an LP64 wasm
    // library under an aarch64 program, docs/gexcc.md §7.8): its globals
    // stay i64, with an i32 copy of the table base for its element segment.
    const lowered = libprov && !m64();
    const w64 = m64() || lowered;
    // A wasm32 program's module, and Wine's PE module, is keyed by its bytes
    // (`wasm:`), not its path: the page keeps those for every process
    // (host.mjs), so a program run again, by any process, is not compiled
    // again, and a file apt replaced is not mistaken for the old one. Keyed
    // by path, winepe.wasm (8.9 MB) was compiled once per Wine process, ten
    // times for the mspaint demo, each copy of its code resident.
    // The key from the shim (`gex_cgen_pmod_wasm_key`) where it makes one,
    // and the bytes copied out only when the page has no module for it.
    const plen = wasmdll ? e.gex_cgen_pmod_wasm_len(prov) >>> 0 : 0;
    const pcopy = () => new Uint8Array(this.#memory.buffer, e.gex_cgen_pmod_wasm_ptr(prov) >>> 0, plen).slice();
    let pbytes = null, pkey = null;
    if (wasmprog && e.gex_cgen_pmod_wasm_key) {
      const lane = (n) => BigInt.asUintN(64, BigInt(e.gex_cgen_pmod_wasm_key(prov, n)));
      pkey = `wasm:${plen}:${lane(0)}:${lane(1)}`;
      if (!this.#sharedModules.has(pkey)) pbytes = pcopy();
    } else if (wasmdll) {
      pbytes = pcopy();
      pkey = `wasm:${moduleContentKey(pbytes)}`;
    }
    // What gex-eject's W3 recorder (tools/eject/record-w3.mjs) collects: the
    // native wasm32 modules a session ran.
    if (wasmdll) console.warn(`[gex-record] module ${this.#cgenStr(e.gex_cgen_pmod_file, prov)} ${pkey.slice(5)}`);
    const mod = wasmdll
      ? this.#cgenFileModule(this.#cgenStr(e.gex_cgen_pmod_file, prov), pbytes, pkey)
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
    const tInst = performance.now();
    let inst;
    const imports = {
      env: {
        memory: this.#memory,
        __indirect_function_table: st.ftab,
        // i64 in a 64-bit memory's process (wasm64 PIC modules).
        __table_base: new WebAssembly.Global({ value: w64 ? 'i64' : 'i32', mutable: false }, w64 ? BigInt(base >>> 0) : base | 0),
        __table_base32: new WebAssembly.Global({ value: 'i32', mutable: false }, base | 0),
        __memory_base: new WebAssembly.Global({ value: w64 ? 'i64' : 'i32', mutable: false }, w64 ? BigInt(memBase) : memBase),
        // A unit's functions never use clang's shadow stack (those are stubs),
        // but a position-independent module imports its pointer all the same.
        // A stateful provider's is this Worker's (one C stack per thread).
        // A wasm32 program sets its own at each thread's entry.
        // and its shared libraries share it (one per thread, as natively).
        __stack_pointer: memBase && !wasmprog ? (libprov ? this.#providerStack64() : this.#providerStack())
          : wasmprog ? this.#wasmprogStack()
          : new WebAssembly.Global(m64() ? { value: 'i64', mutable: true } : { value: 'i32', mutable: true }, m64() ? 0n : 0),
        // clang's three names for a C main: Wine's crt_main references all
        // of them weakly and a position-independent module (W3's PE module)
        // keeps the undefined ones as imports. Null through `GOT.func`, as a
        // static link leaves them, and a trap behind the null check.
        ...(wasmdll ? Object.fromEntries(MAIN_NAMES.map((n) => [n, () => { throw new Error(`${n}: a main the program does not define was called`); }])) : {}),
      },
      ...(wasmdll ? { 'GOT.func': Object.fromEntries(MAIN_NAMES.map((n) => [n, new WebAssembly.Global({ value: 'i32', mutable: true }, 0)])) } : {}),
      gex: {
        ...this.#imports, lookup: st.lookup, ftab: st.ftab,
        // Built-in providers' host functions (gex-cgen/providers): nt_qpc's
        // clock, crt_heap's pages.
        ...(prov >= 0 ? {
          // The shim's exports themselves: a wasm-to-wasm call, no JS.
          nt_qpc: e.gex_nt_qpc,
          clock_ns: e.gex_clock_ns,
          // A provider's fallback to the lifted body (provider.h gp_lifted).
          ftab_twin: e.gex_ftab_twin,
          provider_alloc_top: e.gex_provider_alloc_top,
          // Wine's Unix side (W3.1): its system-call dispatcher and fault
          // hook, process-wide in the shim (the page's delivery of faults
          // is still to come).
          unix_dispatcher: (op, idx) => e.gex_unix_dispatcher?.(op, idx) ?? 0,
          fault_hook: (idx) => e.gex_fault_hook_set?.(idx),
          signal_hook: (idx) => e.gex_signal_hook_set?.(idx),
          code_changed: (addr, len) => e.gex_code_changed?.(addr, len),
          wine_pe_slot: (ptr, len) => e.gex_wine_pe_slot?.(ptr, len) ?? 0,
          // W3.7: Wine's Unix side runs the program's code nested
          // (signal_wasm32.c's `gex_call_out(ctx, pc, 0, 0)`, a call-out
          // that never returns for a thread's loop), and reports each image
          // it maps (natively for unit placement; the page's units path
          // takes it once `gex_image_mapped` exists).
          call_out: (ctx, target, ret, spWant) => this.#callOut(ctx >>> 0, target, ret, spWant),
          image_mapped: (base, fd) => {
            const number = e.gex_image_mapped?.(base, fd) ?? -1;
            if (number === -1) return;
            // An i386 image with a function-tier unit to fetch (cgen.rs
            // `w3_mapped`): served here, before Wine's loader goes on. (-2
            // is a later skeleton bound into the install.)
            if (number === -3) {
              this.#cgenServe();
              return;
            }
            this.#cgenCatchUp();
            if (number >= 0) {
              e.gex_cgen_pmod_installed(number);
              e.gex_wasmdll_installed();
            }
            this.#wasmdllCatchUp();
          },
          // libgex's dlopen past its static registry (Wine's Unix side links
          // it): no shared libraries here
          dlopen: () => 0,
          dlsym: () => 0,
        } : {}),
        ...(memBase && !wasmprog ? (libprov ? this.#providerImports64() : this.#providerImports()) : {}),
        // its runtime's heap: fresh guest pages
        ...(lowered ? { provider_alloc: (size) => e.gex_provider_alloc(size) } : {}),
        ...(wasmprog ? this.#wasmprogImports(() => inst) : libprov ? { syscall: this.#providerSyscall } : wasmdll ? { syscall: this.#nativeSyscall } : {}),
      },
    };
    // A unit split by function (gex_split): its cold clusters' loader,
    // or the recorder's hit map of an instrumented unit.
    if (prov < 0 && jit < 0) this.#splitImports(mod, this.#cgenStr(e.gex_cgen_key, u), imports);
    // A wasm32 program's or shared library's binding list (cgen/dylink.rs).
    const extra = wasmprog ? this.#wasmprogBind(prov, imports, st) : [];
    // What a door instance needs to run this program again on its memory
    // (a wineserver: `#doorPublish`).
    if (wasmprog) this.#wasmprogInfo = { module: mod, base, memBase, binds: this.#wasmprogBindText };
    inst = new WebAssembly.Instance(mod, imports);
    if (wasmprog ? e.gex_wasmprog_relocate(prov) : memBase && e.gex_provider_relocate(memBase)) inst.exports.__wasm_apply_data_relocs?.();
    if (wasmprog) {
      for (const [slot, name] of extra) {
        const f = inst.exports[name];
        if (!f) throw new Error(`wasm32 module ${prov}: ${name} is not exported`);
        st.ftab.set(tIdx(slot), f);
      }
      st.dyl.set(prov, inst);
    }
    // A wasm64 provider's slots' function types, for its JIT's `provider_jit_table` SIG.
    if (libprov && !this.#provSigs.some((x) => x.base === base)) {
      const bytes = new Uint8Array(this.#memory.buffer, e.gex_cgen_pmod_wasm_ptr(prov) >>> 0, e.gex_cgen_pmod_wasm_len(prov) >>> 0);
      this.#provSigs.push({ base, sigs: elementSignatures(bytes) });
    }
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
    // What each Worker pays to install a unit (instantiation fills its slots).
    const ms = performance.now() - tInst;
    if (prov < 0 && jit < 0 && ms >= 20) console.error(`[unit] installed at ${base} in ${Math.round(ms)} ms (${regs.length / 2} bridges)`);
    return regs.length / 2;
  }

  /** Each module's imports, as `module.name`. */
  #modImports = new WeakMap();

  /** The imports a split unit's primary takes beyond a unit's: `gex.cold(c)`,
   *  which instantiates cold cluster `c` here (its start function fills its
   *  slots in this Worker's table, and the stub that called goes on to the
   *  real body); and an instrumented unit's `gexcov.hit(slot)`. */
  #splitImports(mod, key, imports) {
    let names = this.#modImports.get(mod);
    if (!names) {
      names = new Set(WebAssembly.Module.imports(mod).map((i) => `${i.module}.${i.name}`));
      this.#modImports.set(mod, names);
    }
    if (names.has('gex.cold')) {
      const loaded = new Set();
      imports.gex = { ...imports.gex, cold: (c) => {
        c >>>= 0;
        if (loaded.has(c)) throw new Error(`gex-cgen: ${key}: cold cluster ${c} is in, yet its stub ran`);
        loaded.add(c);
        const t0 = performance.now();
        const m = this.#coldModule(key, c);
        new WebAssembly.Instance(m, { env: imports.env, gex: imports.gex });
        this.#coldMs += performance.now() - t0;
      } };
    }
    if (names.has('gexcov.hit')) {
      // A byte per slot: the slot count is the first word of `gex.funcs`.
      const sec = WebAssembly.Module.customSections(mod, 'gex.funcs')[0];
      const n = sec ? new DataView(sec).getUint32(0, true) : 65536;
      imports.gexcov = { hit: this.#covHit(key, n) };
    }
  }

  /** Time this Worker spent bringing in cold clusters, and how many. */
  #coldMs = 0;
  #coldIndex = new Map();

  /** Cold cluster `c` of the unit `key`, compiled: a sibling's, or fetched
   *  (a range of `<key>/cold.bin`, by `<key>/cold.json`) and compiled here. */
  #coldModule(key, c) {
    const name = `cold:${key}:${c}`;
    let mod = this.#sharedModules.get(name);
    if (mod) return mod;
    let ix = this.#coldIndex.get(key);
    if (!ix) {
      // `cold.idx`: u32 unpacked length, then a zstd frame of u32 n, the n
      // clusters' compressed lengths, then their unpacked lengths.
      const b = this.#env.loadFile?.(`${key}/cold.idx`) ?? null;
      const z = this.#env.zstd;
      if (!b) throw new Error(`gex-cgen: ${key}: a split unit with no cold.idx`);
      if (!z) throw new Error(`gex-cgen: ${key}: a split unit, and this Worker has no zstd decoder`);
      const raw = z.decode(b.subarray(4), new DataView(b.buffer, b.byteOffset, 4).getUint32(0, true));
      const r = raw.slice();
      const w = new Uint32Array(r.buffer, 0, r.byteLength >> 2);
      const n = w[0];
      const offs = new Float64Array(n + 1);
      for (let i = 0; i < n; i++) offs[i + 1] = offs[i] + w[1 + i];
      ix = { n, w, offs };
      this.#coldIndex.set(key, ix);
    }
    if (!(c >= 0 && c < ix.n)) throw new Error(`gex-cgen: ${key}: no cold cluster ${c}`);
    const off = ix.offs[c], zlen = ix.w[1 + c], len = ix.w[1 + ix.n + c];
    const t0 = performance.now();
    let bytes = this.#env.loadRange?.(`${key}/cold.bin`, off, zlen) ?? null;
    if (!bytes) throw new Error(`gex-cgen: ${key}: cold cluster ${c} (${zlen} bytes at ${off}) could not be read`);
    const t1 = performance.now();
    bytes = this.#env.zstd.decode(bytes, len);
    const t2 = performance.now();
    mod = new WebAssembly.Module(bytes);
    const t3 = performance.now();
    console.warn(`[cold] ${key.slice(0, 16)} ${c}: ${zlen} bytes (${len} unpacked), fetch ${(t1 - t0).toFixed(1)} ms, `
      + `decode ${(t2 - t1).toFixed(1)} ms, compile ${(t3 - t2).toFixed(1)} ms, at ${Math.round(performance.timeOrigin + performance.now()) % 1e7}`);
    this.#sharedModules.set(name, mod);
    this.#env.shareModule?.(name, mod);
    return mod;
  }

  /** An instrumented unit's hit map in this Worker (a byte per slot), and the
   *  slots already said. */
  #cov = new Map();
  #covTimer = null;
  #covHit(key, slots) {
    let h = this.#cov.get(key);
    if (!h) {
      // One memory per Worker for every unit's map (each Memory reserves
      // address space: one per unit ran a page out of it), at an offset per
      // unit. (module (import "m" "m" (memory 1)) (import "m" "o" (global i32))
      //   (func (export "hit") (param i32) local.get 0 global.get 0 i32.add
      //   i32.const 1 i32.store8))
      const wasm = new Uint8Array([0, 97, 115, 109, 1, 0, 0, 0, 1, 5, 1, 96, 1, 127, 0,
        2, 15, 2, 1, 109, 1, 109, 2, 0, 1, 1, 109, 1, 111, 3, 127, 0, 3, 2, 1, 0, 7, 7, 1, 3, 104, 105, 116, 0, 0,
        10, 14, 1, 12, 0, 32, 0, 35, 0, 106, 65, 1, 58, 0, 0, 11]);
      this.#covMod ??= new WebAssembly.Module(wasm);
      this.#covMem ??= new WebAssembly.Memory({ initial: 1 });
      const off = this.#covUsed;
      this.#covUsed += slots;
      const need = Math.ceil(this.#covUsed / 65536) - this.#covMem.buffer.byteLength / 65536;
      if (need > 0) this.#covMem.grow(need);
      const inst = new WebAssembly.Instance(this.#covMod, { m: { m: this.#covMem, o: new WebAssembly.Global({ value: 'i32' }, off) } });
      h = { off, n: slots, hit: inst.exports.hit, said: new Uint8Array(slots) };
      this.#cov.set(key, h);
      this.#covTimer ??= setInterval(() => this.covFlush(), 2000);
    }
    return h.hit;
  }
  #covMod = null;
  #covAt = 0;
  #covMem = null;
  #covUsed = 0;

  /** Say the slots instrumented units were entered at since the last time:
   *  `[gex-cov] <key> <slot or a-b>,...` (tools/eject/record-w3.mjs). */
  covFlush() {
    for (const [key, h] of this.#cov) {
      const m = new Uint8Array(this.#covMem.buffer, h.off, h.n);
      const runs = [];
      let a = -1;
      for (let k = 0; k <= h.n; k++) {
        const fresh = k < h.n && m[k] && !h.said[k];
        if (fresh && a < 0) a = k;
        if (!fresh && a >= 0) {
          runs.push(a === k - 1 ? `${a}` : `${a}-${k - 1}`);
          h.said.fill(1, a, k);
          a = -1;
        }
      }
      if (runs.length) console.warn(`[gex-cov] ${key} ${runs.join(',')}`);
    }
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

  /** This Worker's stack for a wasm64 provider (i64 `__stack_pointer`), and its lowest address. */
  #provStack64 = null;
  #provStackLo = 0n;
  #providerStack64() {
    if (!this.#provStack64) {
      const size = 1 << 20;
      const e = this.#shim.exports;
      const base = e.gex_provider_stack(size) >>> 0;
      if (!base) throw new Error('gex: no memory for a provider stack');
      this.#provStackLo = BigInt(base);
      this.#provStack64 = new WebAssembly.Global({ value: 'i64', mutable: true }, BigInt(base + size - 16));
    }
    return this.#provStack64;
  }

  /** This Worker's exception tag for a wasm64 provider's `provider_throw`: a wasm
   *  exception, so code the provider publishes at run time (its JIT, which imports it)
   *  can catch it with `try_table`, and `provider_try` takes it when nothing did. */
  #provTag = null;
  /** Traps seen unwinding provider guest calls (reported up to 16). */
  #provLeaks = 0;
  #providerTag() {
    this.#provTag ??= new WebAssembly.Tag({ parameters: ['i32'] });
    return this.#provTag;
  }

  /** What a replaced Linux library's wasm64 provider imports from `gex` (the native
   *  `libprov::register64`; docs/unity-dotnet.md P5 B1). Pointers and table indices
   *  are i64 (BigInt). */
  #providerImports64() {
    const e = this.#shim.exports;
    const ftab = this.#cgen.ftab;
    const tag = this.#providerTag();
    return {
      provider_try: (f, arg) => {
        // The guest calls a raise unwinds are no longer open.
        const calls = e.gex_libprov_guest_calls();
        try {
          ftab.get(f)(arg);
          return 0;
        } catch (err) {
          const open = e.gex_libprov_guest_calls();
          e.gex_libprov_guest_calls_set(calls);
          if (err instanceof WebAssembly.Exception && err.is(tag)) return err.getArg(tag, 0);
          // Anything else that unwinds a provider's guest calls abandons
          // frames the guest will return into (their return addresses stay
          // on its stack): say which trap, where it resumes, and from where.
          if (open > calls && (this.#provLeaks = (this.#provLeaks ?? 0) + 1) <= 16) {
            console.error(`[gex] a trap left through ${open - calls} provider guest call(s): ${err?.message ?? err}; ` +
              `resume ${BigInt.asUintN(64, BigInt(e.gex_resume_pc())).toString(16)}\n${err?.stack ?? ''}`);
          }
          throw err;
        }
      },
      provider_throw: (code) => {
        throw new WebAssembly.Exception(tag, [code]);
      },
      // The shim's export itself: a wasm-to-wasm call on every guest call.
      provider_guest_call: e.gex_libprov_guest_call,
      provider_missing: (addr) => e.gex_libprov_missing(addr),
      provider_thread_entry: () => e.gex_libprov_thread_entry(),
      provider_code_page: () => e.gex_libprov_thread_entry(),
      provider_stack: () => this.#provStackLo,
      provider_guest_sym: (ptr, len) => e.gex_libprov_guest_sym(ptr, len),
      provider_guest_stack_end: (sp) => e.gex_libprov_guest_stack_end(sp),
      provider_abi: (which) => e.gex_libprov_abi(which),
      provider_table_size: (base) => e.gex_libprov_table_size(base),
      // Stopping the guest's threads around the runtime's collection (cgen/libprov.rs).
      provider_thread_register: (inGuest, stop) => e.gex_libprov_thread_register(inGuest, stop),
      provider_park: () => e.gex_libprov_park(),
      provider_stop_guest: () => e.gex_libprov_stop(),
      provider_start_guest: () => e.gex_libprov_start(),
      // The runtime's wasm JIT (the native `wasmjit`): a range of the process's function
      // table, and each method's module compiled once per process and instantiated in every
      // Worker that runs it.
      provider_jit_reserve: (n) => {
        const base = e.gex_libprov_jit_reserve(BigInt(n));
        this.#cgenCatchUp();
        return base;
      },
      provider_jit_instantiate: (...a) => this.#jitInstantiate(...a),
      provider_jit_table: (op, a, b) => {
        a = BigInt(a);
        if (op === 0) {
          const f = this.#jitInstalled.get(a);
          if (!f) return 0n;
          ftab.set(a, f);
          return 1n;
        }
        if (op === 1) {
          const f = ftab.get(BigInt(b));
          if (!f) throw new Error(`wasmjit: alias of an empty slot ${b}`);
          ftab.set(a, f);
          this.#jitInstalled.set(a, f);
          return 0n;
        }
        if (op === 2) {
          const f = ftab.get(a);
          return f ? BigInt(f.length) : -1n;
        }
        // SIG: from the provider module's own types (JS cannot ask a function for them).
        for (const { base, sigs } of this.#provSigs) {
          const k = Number(a) - base;
          if (k >= 0 && k < sigs.length) return BigInt(sigs[k]);
        }
        return -1n;
      },
    };
  }

  /** Wasm64 providers' table ranges and their slots' packed function types. */
  #provSigs = [];

  /** The functions this Worker's JIT instantiations installed, by table slot. */
  #jitInstalled = new Map();

  /** `gex.provider_jit_instantiate(bytes, len, cnames, consts, nconsts, slots, names, n,
   *  errbuf, errcap)`: compile (once per process, by content) and instantiate a JIT module in
   *  this Worker, installing its named exports at table slots. 1, or 0 with the reason in
   *  errbuf. */
  #jitInstantiate(bytes, len, cnames, consts, nconsts, slots, names, n, errbuf, errcap) {
    const mem = new Uint8Array(this.#memory.buffer);
    const N = Number;
    try {
      const wasm = mem.slice(N(bytes), N(bytes) + N(len));
      let h = 0x811c9dc5;
      for (let i = 0; i < wasm.length; i++) h = Math.imul(h ^ wasm[i], 16777619) >>> 0;
      const name = `wasmjit:${wasm.length}:${h.toString(16)}`;
      let mod = this.#sharedModules.get(name);
      if (!mod) {
        mod = new WebAssembly.Module(wasm);
        this.#sharedModules.set(name, mod);
        this.#env.shareModule?.(name, mod);
      }
      const dv = new DataView(this.#memory.buffer);
      const cv = new Map();
      if (N(nconsts) > 0) {
        const nb = mem.slice(N(cnames), N(cnames) + 256);
        let at = 0;
        for (let k = 0; k < N(nconsts); k++) {
          let end = at;
          while (end < nb.length && nb[end] !== 0) end++;
          cv.set(new TextDecoder().decode(nb.subarray(at, end)), dv.getBigUint64(N(consts) + 8 * k, true));
          at = end + 1;
        }
      }
      const globals = importedGlobals(wasm);
      const ftab = this.#cgen.ftab;
      const imports = { m: { h: this.#memory }, f: { f: ftab }, s: {}, x: { e: this.#providerTag() }, h: {} };
      for (const imp of WebAssembly.Module.imports(mod)) {
        if (imp.module === 's') {
          if (imp.name === 'p') {
            imports.s.p = this.#providerStack64();
            continue;
          }
          const v = cv.get(imp.name);
          if (v === undefined) throw new Error(`no value for import s.${imp.name}`);
          const g = globals.get(`s.${imp.name}`) ?? { value: 'i64', mutable: false };
          imports.s[imp.name] = new WebAssembly.Global(g, g.value === 'i64' ? BigInt.asIntN(64, v) : Number(BigInt.asIntN(32, v)));
        } else if (imp.module === 'h') {
          const f = ftab.get(BigInt(imp.name));
          if (!f) throw new Error(`import h.${imp.name}: table slot holds no function`);
          imports.h[imp.name] = f;
        }
      }
      const inst = new WebAssembly.Instance(mod, imports);
      const k = N(n);
      if (k > 0) {
        const nb = mem.slice(N(names), N(names) + 64 * k);
        const list = [];
        let at = 0;
        for (let i = 0; i < k; i++) {
          let end = at;
          while (end < nb.length && nb[end] !== 0) end++;
          list.push(new TextDecoder().decode(nb.subarray(at, end)));
          at = end + 1;
        }
        const todo = list.map((nm, i) => {
          const f = inst.exports[nm];
          if (typeof f !== 'function') throw new Error(`no export ${nm}`);
          return [dv.getBigUint64(N(slots) + 8 * i, true), f];
        });
        for (const [slot, f] of todo) {
          ftab.set(slot, f);
          this.#jitInstalled.set(slot, f);
        }
      }
      this.#jitInstances.push(inst);
      return 1;
    } catch (err) {
      const cap = N(errcap);
      if (N(errbuf) && cap > 0) {
        const b = new TextEncoder().encode(String(err?.message ?? err)).subarray(0, cap - 1);
        mem.set(b, N(errbuf));
        mem[N(errbuf) + b.length] = 0;
      }
      return 0;
    }
  }
  #jitInstances = [];

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
          ftab.get(m64() ? BigInt(f) : f >>> 0)(arg);
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
      ftab_twin: (addr) => e.gex_ftab_twin(addr),
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
  #cgenFileModule(file, bytes = null, key = null) {
    const name = key ?? `cgenfile:${file}`;
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
      // A replaced Linux library (cgen/libprov.rs): its wasm64 provider, installed as a module.
      if (e.gex_cgen_provider_libprov?.(u, i)) {
        const number = e.gex_libprov_publish(u, i);
        this.#cgenCatchUp();
        if (number >= 0) {
          e.gex_cgen_pmod_installed(number);
          // its GexA64Icall entry thunks, bound as guest addresses
          e.gex_libprov_installed?.(number);
          this.#wasmdllCatchUp();
        }
        continue;
      }
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
        console.error(`gex-nt: missing unit: no provider module ${file} in the unit cache, so ${this.#cgenStr(e.gex_cgen_path, u)} runs without its providers; build them with GEX_CGEN_CACHE=<the unit cache> gexc cgen-image --providers (or natively: a run of the program, or gexc cgen-prefix)`);
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
    const dt = performance.now() - t0;
    this.#compileMs += dt;
    this.#fjitCompileMs += dt;
    // Every 60 s while the JIT makes modules: what it has cost so far (the
    // census at exit comes too late for a game that never exits).
    if (t0 >= this.#fjitNextReport && e.gex_fjit_translate_ms) {
      if (this.#fjitNextReport) {
        console.error(`[gex] fjit so far: ${e.gex_fjit_modules()} modules, ${e.gex_fjit_wasm_kib()} KiB of wasm, `
          + `${e.gex_fjit_translate_ms()} ms translating (the process), ${this.#fjitCompileMs.toFixed(0)} ms compiling (this Worker)`);
      }
      this.#fjitNextReport = t0 + 60000;
    }
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
      // A provider of a program an execve replaced (`FuncMap::kill_module`) is skipped.
      if (kind === 1) {
        const u = e.gex_ftab_install_unit(v) >>> 0;
        if (u !== 0xffffffff) this.#cgenInstall(u, e.gex_ftab_install_base(v) >>> 0);
      }
      else if (kind === 2) st.ftab.set(tIdx(v), st.resolve);
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

  /** The process's wasm32 link map, for a fork's child (cgen/dylink.rs), or null. */
  #wasmprogLink() {
    const e = this.#shim.exports;
    const n = (e.gex_wasmprog_link?.(0, 0) ?? 0) >>> 0;
    if (!n) return null;
    const buf = e.gex_alloc(n) >>> 0;
    try {
      e.gex_wasmprog_link(buf, n);
      return new TextDecoder().decode(this.#bytes().slice(buf, buf + n));
    } finally {
      e.gex_free?.(buf, n);
    }
  }

  /** This Worker's `__stack_pointer` for wasm32 programs: one global the
   *  program and its shared libraries share (one C stack per thread). */
  #wasmprogSp = null;
  #wasmprogStack() {
    return (this.#wasmprogSp ??= new WebAssembly.Global(m64() ? { value: 'i64', mutable: true } : { value: 'i32', mutable: true }, m64() ? 0n : 0));
  }

  /** This Worker's asyncify state and data globals, which a program's
   *  modules built with relocatable asyncify share. */
  #wasmprogAsync = null;
  /** The mutable globals the program's modules were bound to (`G`), which
   *  a vfork child's run may change (`#vforkChild`). */
  #wasmprogMuts = [];

  /** Apply provider module `prov`'s binding list (cgen/dylink.rs's format)
   *  to `imports`; the table slots to fill with exports after
   *  instantiation, as [slot, name]. */
  #wasmprogBind(prov, imports, st) {
    const e = this.#shim.exports;
    if (!e.gex_wasmprog_binds) return [];
    const n = e.gex_wasmprog_binds(prov, 0, 0) >>> 0;
    this.#wasmprogBindText = '';
    if (!n) return [];
    const buf = e.gex_alloc(n) >>> 0;
    let text;
    try {
      e.gex_wasmprog_binds(prov, buf, n);
      text = new TextDecoder().decode(this.#bytes().slice(buf, buf + n));
    } finally {
      e.gex_free?.(buf, n);
    }
    this.#wasmprogBindText = text;
    const extra = [];
    const set = (m, name, v) => { (imports[m] ??= {})[name] = v; };
    for (const line of text.split('\n')) {
      if (!line) continue;
      const f = line.split('\t');
      switch (f[0]) {
        case 'S':
          if (f[1] === 'env' && (f[2] === '__asyncify_state' || f[2] === '__asyncify_data')) {
            this.#wasmprogAsync ??= [0, 1].map(() => new WebAssembly.Global({ value: 'i32', mutable: true }, 0));
            set('env', f[2], this.#wasmprogAsync[f[2] === '__asyncify_state' ? 0 : 1]);
          }
          break;
        case 'G': {
          const g = new WebAssembly.Global({ value: 'i32', mutable: f[4] === '1' }, Number(f[3]) | 0);
          if (f[4] === '1') this.#wasmprogMuts.push(g);
          set(f[1], f[2], g);
          break;
        }
        case 'E': {
          const from = st.dyl.get(Number(f[3]));
          const x = from?.exports[f[4]];
          if (x === undefined) throw new Error(`wasm32 module ${prov}: ${f[1]}.${f[2]}: ${f[4]} of module ${f[3]} is not instantiated in this Worker`);
          set(f[1], f[2], x);
          break;
        }
        case 'L': {
          const slot = tIdx(Number(f[3])), t = st.ftab;
          set(f[1], f[2], (...a) => t.get(slot)(...a));
          break;
        }
        case 'M': {
          const msg = f.slice(3).join('\t');
          set(f[1], f[2], () => { throw new Error(msg); });
          break;
        }
        case 'X':
          extra.push([Number(f[1]), f[2]]);
          break;
      }
    }
    return extra;
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
    const staged = () => {
      if (e.gex_wasmprog_staged()) {
        // posix_spawn's child first, here, when it can be (`#vforkChild`);
        // any other child is started below.
        if (!single.on) {
          const ok = e.gex_vfork_ok?.() ?? 0;
          if (ok > 0) this.#vforkChild();
          else if (ok !== -2) vforkTrace(`[vfork] not here: ${ok}`);
        }
        this.#drainFork().catch((err) => console.error(`[gex] starting a process: ${err.message ?? err}`));
        this.#drainThread().catch((err) => console.error(`[gex] starting a thread: ${err.message ?? err}`));
      }
    };
    const sync = (ctx, pc) => {
      e.gex_syscall(ctx, pc);
      staged();
    };
    return {
      syscall: single.on ? this.#suspendingSyscall(sync, staged) : sync,
      asyncify,
      fork_point: (rec) => {
        const st = asyncify(0, 0);
        if (st === 0) asyncify(1, rec);
        else if (st === 2) asyncify(4, 0);
        else throw new Error(`gex.fork_point in asyncify state ${st}`);
      },
      // libgex's dlopen/dlsym past its static registry
      // (cgen/dylink.rs): the shim loads and publishes, this Worker
      // instantiates the new modules and runs their constructors; other
      // Workers instantiate them as they next catch up
      dlopen: (path, err, errlen) => {
        const h = e.gex_wasmprog_dlopen(path, err, errlen) >>> 0;
        if (!h) return 0;
        this.#cgenCatchUp();
        const st = this.#cgenStore();
        for (let k = 0, n; (n = e.gex_wasmprog_dl_new(k)) >= 0; k++) st.dyl.get(n)?.exports.__wasm_call_ctors?.();
        return h | 0;
      },
      dlsym: (h, name) => e.gex_wasmprog_dlsym(h, name) | 0,
      // Door calls into the wineserver (docs/wineserver-kernel.md).
      door_publish: (area) => this.#doorPublish(area >>> 0),
      door_call: (req, pid, tid) => this.#doorCall(req >>> 0, pid, tid),
      // A Worker the door was broadcast to while it ran (it takes no
      // messages): Wine runs its program again as a child, which starts with
      // the door (loader.c `wineserver_relaunch`). None in the SP build.
      door_missing: () => (this.#door || single.on ? 0 : 1),
      // The session mapping, a block of the server's memory (area +128,
      // +132), copied from directly by win32u (docs/wineserver-kernel.md
      // §6.1): the bytes copied, or -1 with no door session; the session's
      // size for a size of 0.
      session_copy: (dst, off, size) => this.#sessionCopy(dst >>> 0, off >>> 0, size >>> 0),
      session_seq: (off) => this.#sessionSeq(off >>> 0),
      // The server's user shared data page (area +140), copied at a
      // process's start and written by the first boot's fill (§6.2).
      usd_copy: (dst, size) => this.#usdMove(dst >>> 0, size >>> 0, false),
      usd_publish: (src, size) => this.#usdMove(src >>> 0, size >>> 0, true),
      // The server's own (the pump): a thread's wait pipe, which door calls
      // write in place (docs/wineserver-kernel.md §4.5). Global pipe ids.
      wake_pipe: (fd) => this.#shim.exports.gex_fd_pipe_id?.(fd) ?? -1,
      wake_write: (id, buf, len) => this.#kernel?.k_pipe_write?.(id, buf >>> 0, len >>> 0) ?? -38,
      wake_close: () => {},
    };
  }

  // -- door calls (docs/wineserver-kernel.md) ------------------------------
  //
  // The wineserver is its own process. When it is up it publishes its door
  // area; the host hands the server's memory and module to every Worker.
  // A client thread then makes a Wine request by running it itself, on its
  // Worker's own instance of the server's module on the server's memory,
  // under the server's door lock: no pipe, no wake. A system call the server
  // code makes inside a door is answered here when it needs no process (the
  // clock, getpid), and otherwise proxied to the server's pump.

  /** This process's wasm32 program as installed (`#cgenInstall`). */
  #wasmprogInfo = null;
  /** The binding list `#wasmprogBind` read last (for `#wasmprogInfo`). */
  #wasmprogBindText = '';
  /** The session's door server, and this Worker's instance on it. */
  #door = null;
  #doorInst = null;
  #doorViews = null;
  /** Census: door calls, fallbacks, proxies. */
  #doorStats = { calls: 0, fallbacks: 0, proxies: 0, t0: 0, nodoor: 0, tPre: 0, tDisp: 0, tPost: 0, T2: 0, tSys: 0, sys: 0, tProxy: 0, byNr: {} };

  /** The server: `area` is its door area, in this process's memory. */
  #doorPublish(area) {
    const info = this.#wasmprogInfo;
    if (!info || single.on) return;
    const e = this.#shim.exports;
    const dv = new DataView(this.#memory.buffer);
    const kickFd = dv.getInt32(area + 68, true);
    const kickId = e.gex_fd_pipe_id?.(kickFd) ?? -1;
    if (kickId < 0) {
      console.error(`[gex-door] the server's kick fd ${kickFd} is not a global pipe: no doors`);
      return;
    }
    // The door context must hold gex's whole register context (gex_abi
    // ctx::SIZE, 1120 bytes for i386): the kernel writes past the
    // system-call slots, and a short one corrupted the server's heap.
    const ctxSize = dv.getUint32(area + 136, true);
    if (ctxSize < 1120) {
      console.error(`[gex-door] the server's door context is ${ctxSize} bytes, under gex's 1120: no doors`);
      return;
    }
    const door = {
      memory: this.#memory, module: info.module, base: info.base, memBase: info.memBase, binds: info.binds ?? '',
      area, kickId, pid: dv.getInt32(area + 112, true), id: `${Date.now()}:${area}`,
    };
    if (globalThis.GEX_DOOR_TRACE) console.error(`[gex-door] wineserver pid ${door.pid} published its door (area ${area})`);
    this.#env.publishDoor?.(door);
  }

  #usdMove(at, size, publish) {
    const d = this.#door;
    if (!d || single.on) return -1;
    const { u8, dv } = this.#doorView();
    const usd = dv.getUint32(d.area + 140, true);
    if (!usd) return -1;
    const n = Math.min(size, 4096);
    if (publish) u8.set(this.#bytes().subarray(at, at + n), usd);
    else this.#bytes().set(u8.subarray(usd, usd + n), at);
    return n;
  }

  #sessTr = 0;
  #sessionCopy(dst, off, size) {
    const d = this.#door;
    if (globalThis.GEX_SESSION_TRACE && !(++this.#sessTr % 20000)) console.error(`[session] copy ${this.#sessTr} off ${off} size ${size}`);
    if (!d) return -1;
    const { u8, dv } = this.#doorView();
    const base = dv.getUint32(d.area + 128, true), len = dv.getUint32(d.area + 132, true);
    if (!base || !len) return -1;
    if (!size) return len;
    if (off >= len) return -1;
    const n = Math.min(size, len - off);
    this.#bytes().set(u8.subarray(base + off, base + off + n), dst);
    return n;
  }

  #sessionSeq(off) {
    const d = this.#door;
    if (globalThis.GEX_SESSION_TRACE && !(++this.#sessTr % 20000)) console.error(`[session] seq ${this.#sessTr} off ${off} base ${d && this.#doorView().dv.getUint32(d.area + 128, true)}`);
    const { dv } = this.#doorView();
    const base = d ? dv.getUint32(d.area + 128, true) : 0;
    if (!base) return 0n;
    const at = base + off;
    // A seqlock's word: whole, as the server stores it (8-aligned).
    if (!(at & 7)) return Atomics.load(this.#doorView().i64, at / 8);
    return dv.getBigInt64(at, true);
  }

  /** The door server, from the host (all Workers of the session). */
  setDoor(door) {
    if (this.#door?.id === door?.id) return;
    this.#door = door;
    this.#doorInst = null;
    this.#doorViews = null;
  }

  #doorView() {
    const d = this.#door;
    const buf = d.memory.buffer;
    let v = this.#doorViews;
    if (!v || v.buf !== buf || v.len !== buf.byteLength) {
      v = this.#doorViews = { buf, len: buf.byteLength, u8: new Uint8Array(buf), i32: new Int32Array(buf), i64: new BigInt64Array(buf, 0, Math.floor(buf.byteLength / 8)), dv: new DataView(buf) };
    }
    return v;
  }

  #doorInstantiate(d) {
    const v = this.#doorView();
    const a = d.area;
    const stackTop = v.dv.getUint32(a + 52, true);
    const sp = new WebAssembly.Global({ value: 'i32', mutable: true }, stackTop);
    // `slots` is the program's table reservation (tens of millions, past
    // an engine's table limit): the door's table needs only the module's
    // own elements, its dylink table_size.
    // The binding list (dylink.rs): a static program's are its exports past
    // the element segment (`X`) and its GOT (`G`); a server with shared
    // libraries is not a door server.
    const extra = [];
    const got = [];
    let size = d.base + dylinkTableSize(d.module);
    for (const line of d.binds.split('\n')) {
      if (!line) continue;
      const f = line.split('\t');
      if (f[0] === 'X') { extra.push([Number(f[1]), f[2]]); size = Math.max(size, Number(f[1]) + 1); }
      else if (f[0] === 'G') got.push(f);
      else if (f[0] === 'S') { if (f[1] === 'env' && f[2].startsWith('__asyncify_')) got.push(['G', 'env', f[2], '0', '1']); }
      else throw new Error(`door: the server program's binding ${f[0]} ${f[1]}.${f[2]} is not supported`);
    }
    const table = new WebAssembly.Table({ element: 'anyfunc', initial: size });
    class DoorExit extends Error {}
    const st = this.#doorStats;
    const syscall = globalThis.GEX_DOOR_STATS
      ? (ctx) => { const t = performance.now(); try { this.#doorSyscall(ctx >>> 0, DoorExit); } finally { st.sys++; st.tSys += performance.now() - t; } }
      : (ctx) => this.#doorSyscall(ctx >>> 0, DoorExit);
    const imports = {
      env: {
        memory: d.memory, __indirect_function_table: table, __stack_pointer: sp,
        __memory_base: new WebAssembly.Global({ value: 'i32', mutable: false }, d.memBase | 0),
        __table_base: new WebAssembly.Global({ value: 'i32', mutable: false }, d.base | 0),
      },
      gex: {
        syscall,
        asyncify: (op) => { if (op === 0) return 0; throw new Error('gex.asyncify: no fork inside a door call'); },
        // a thread's wait pipe is resolved by the pump (init_thread); written in place
        wake_pipe: () => -1,
        wake_write: (id, buf, len) => this.#doorWake(id, buf >>> 0, len >>> 0),
        wake_close: () => {},
      },
    };
    for (const f of got) (imports[f[1]] ??= {})[f[2]] = new WebAssembly.Global({ value: 'i32', mutable: f[4] === '1' }, Number(f[3]) | 0);
    for (const imp of WebAssembly.Module.imports(d.module)) {
      if (imp.kind === 'function' && !imports[imp.module]?.[imp.name]) {
        (imports[imp.module] ??= {})[imp.name] = () => { throw new Error(`${imp.module}.${imp.name}: not available inside a door call`); };
      }
    }
    const inst = new WebAssembly.Instance(d.module, imports);
    for (const [slot, name] of extra) table.set(slot, inst.exports[name]);
    // Its first call sets its context, thread pointer and TLS
    // (`__gex_thread_start` on the area's frame) and ends at the exit call.
    const ctx = v.dv.getUint32(a + 72, true);
    const frame = v.dv.getUint32(a + 76, true);
    v.dv.setUint32(ctx + 32, frame, true);
    v.dv.setUint32(ctx + 36, 0, true);
    try {
      table.get(v.dv.getUint32(a + 108, true))(ctx, 0, 0, 0n);
      throw new Error('door: __gex_thread_start returned');
    } catch (err) {
      if (!(err instanceof DoorExit)) throw err;
    }
    const dispatch = table.get(v.dv.getUint32(a + 60, true));
    return { door: d, dispatch, sp, stackTop, bootNs: BigInt(this.#kernel?.k_boot_secs?.(0) ?? 0) * 1000000000n };
  }

  /** A system call from server code inside a door (i386 numbers, the
   *  context in the server memory). */
  #doorSyscall(ctx, DoorExit) {
    const d = this.#door;
    const { dv, i32 } = this.#doorView();
    const nr = dv.getUint32(ctx, true);
    const arg = (slot) => dv.getUint32(ctx + slot * 8, true);
    const ret = (r) => { dv.setUint32(ctx, r >>> 0, true); dv.setUint32(ctx + 4, 0, true); };
    switch (nr) {
      case 1: case 252: throw new DoorExit();
      case 20: case 224: ret(d.pid); return;
      case 265: case 403: case 78: {
        // clock_gettime, clock_gettime64, gettimeofday: as the shim's
        // browser_clock and gex-kernel's monotonic_from (one boot base).
        const clk = arg(3), buf = nr === 78 ? arg(3) : arg(1);
        if (nr === 78 || clk === 0 || clk === 5 || clk === 8 || [1, 4, 6, 7, 9].includes(clk)) {
          if (!buf) { ret(nr === 78 ? 0 : -14); return; }
          const ns = this.#env.now();
          let secs = Number(ns / 1000000000n), nanos = Number(ns % 1000000000n);
          if (nr !== 78 && [1, 4, 6, 7, 9].includes(clk)) secs -= this.#kernel?.k_boot_secs?.(0) ?? 0;
          if (nr === 403) { dv.setBigUint64(buf, BigInt(secs), true); dv.setBigUint64(buf + 8, BigInt(nanos), true); }
          else { dv.setUint32(buf, secs >>> 0, true); dv.setUint32(buf + 4, nr === 78 ? Math.floor(nanos / 1000) : nanos, true); }
          ret(0);
          return;
        }
        break;
      }
    }
    // Proxied: the pump makes it as the server process. ebx, ecx, edx,
    // esi, edi, ebp.
    const a = d.area;
    i32[(a + 8) >> 2] = nr;
    const regs = [3, 1, 2, 6, 7, 5];
    for (let k = 0; k < 6; k++) i32[(a + 12 + 4 * k) >> 2] = arg(regs[k]);
    const stats = dv.getUint32(a + 96, true);
    if (stats && nr < 512) Atomics.add(i32, (stats >> 2) + nr, 1);
    Atomics.add(i32, (a + 100) >> 2, 1);
    const st = this.#doorStats;
    st.proxies++;
    const tp = globalThis.GEX_DOOR_STATS ? performance.now() : 0;
    Atomics.store(i32, (a + 4) >> 2, 1);
    Atomics.notify(i32, a >> 2);
    this.#doorKick();
    while (Atomics.load(i32, (a + 4) >> 2) !== 2) Atomics.wait(i32, (a + 4) >> 2, 1, 5);
    const r = i32[(a + 36) >> 2];
    Atomics.store(i32, (a + 4) >> 2, 0);
    if (tp) { st.tProxy += performance.now() - tp; (st.byNr[nr] = (st.byNr[nr] ?? 0) + 1); }
    ret(r);
  }

  #doorScratch = 0;

  /** `gex.wake_write` inside a door: a client's wait pipe written from this
   *  Worker (the pipes are the global kernel's), not proxied to the pump. */
  #doorWake(id, buf, len) {
    if (len > 64 || !this.#doorScratch) return -22;
    const { u8 } = this.#doorView();
    this.#bytes().set(u8.subarray(buf, buf + len), this.#doorScratch);
    return this.#kernel?.k_pipe_write?.(id, this.#doorScratch, len) ?? -38;
  }

  #doorKick() {
    // one byte, from anywhere in this process's memory: the pump only drains
    this.#kernel?.k_pipe_write?.(this.#door.kickId, 0, 1);
  }

  /** `gex.door_call(req, unix_pid, unix_tid)`: 0 done (the reply in `req`),
   *  1 use the pipes, 2 the thread died in the request. The hot path is kept
   *  small and free of the statistics (`#doorCallStats`, GEX_DOOR_STATS), so
   *  it stays optimised: every server request of a W3 process comes here
   *  (docs/wine-overhead.md). */
  #doorCall(req, pid, tid) {
    if (this.#doorStatsOn) return this.#doorCallStats(req, pid, tid);
    const d = this.#door;
    if (!d || single.on) return 1;
    return this.#doorRun(d, req, pid, tid, null);
  }

  /** GEX_DOOR_STATS, read once per Worker (a page sets it before it starts). */
  #doorStatsOn = !!globalThis.GEX_DOOR_STATS;

  /** The guest memory's DataView, rebuilt as `#bytes` is. */
  #gdv = null;
  #guestView() {
    const cm = this.#bytes();
    let v = this.#gdv;
    if (!v || v.buffer !== cm.buffer || v.byteLength !== cm.byteLength) v = this.#gdv = new DataView(cm.buffer, 0, cm.byteLength);
    return v;
  }

  /** One door call under the door lock (`st`: the statistics, or null). */
  #doorRun(d, req, pid, tid, st) {
    const T = st ? performance.now() : 0;
    // The request's header is in the door buffer once copied in: its 64
    // bytes are the scratch an in-place wakeup is written from (#doorWake).
    this.#doorScratch = req;
    const cm = this.#bytes();
    const cdv = this.#guestView();
    const reqSize = cdv.getUint32(req + 4, true);
    const replyMax = cdv.getUint32(req + 8, true);
    const count = Math.min(cdv.getUint32(req + 64, true), 16);
    const replyData = cdv.getUint32(req + 68, true);
    const { u8, i32, dv } = this.#doorView();
    const a = d.area;
    const buf = dv.getUint32(a + 44, true), bufSize = dv.getUint32(a + 48, true);
    const replyOff = (64 + reqSize + 7) & ~7;
    if (replyOff + 64 + replyMax > bufSize) { if (st) st.fallbacks++; return 1; }
    // the lock: 0 free, 1 held, 2 held with waiters
    const L = a >> 2;
    // Poisoned (a door call died midway): no more doors; the pipes say the
    // rest when the server goes.
    if (Atomics.load(i32, (a + 124) >> 2) !== 0) return 1;
    if (Atomics.compareExchange(i32, L, 0, 1) !== 0) this.#doorLockSlow(i32, a, L, pid, tid);
    i32[(a + 116) >> 2] = tid;
    i32[(a + 120) >> 2] = pid;
    let r;
    try {
      // This Worker's instance, started under the lock: its start runs on
      // the one door stack and context.
      let di = this.#doorInst;
      if (!di || di.door !== d) di = this.#doorInst = this.#doorInstantiate(d);
      u8.set(cm.subarray(req, req + 64), buf);
      let at = buf + 64;
      for (let k = 0; k < count; k++) {
        const ptr = cdv.getUint32(req + 72 + 8 * k, true), size = cdv.getUint32(req + 76 + 8 * k, true);
        u8.set(cm.subarray(ptr, ptr + size), at);
        at += size;
      }
      di.sp.value = di.stackTop;
      const T1 = st ? performance.now() : 0;
      // The time, as set_current_time would read it (gettimeofday,
      // CLOCK_BOOTTIME, both `env.now()`), in 100 ns: two clock calls out
      // of the door saved.
      const ns = this.#env.now();
      r = di.dispatch(pid, tid, ns / 100n + 116444736000000000n, (ns - di.bootNs) / 100n) >>> 0;
      if (st) { const T2 = performance.now(); st.tPre += T1 - T; st.tDisp += T2 - T1; st.T2 = T2; }
      if (r === 0xffffffff) { if (st) st.fallbacks++; r = 1; }
      else if (r === 0xfffffffe) r = 2;
      else {
        const ro = buf + r;
        cm.set(u8.subarray(ro, ro + 64), req);
        const size = Math.min(dv.getUint32(ro + 4, true), replyMax);
        if (size && replyData) cm.set(u8.subarray(ro + 64, ro + 64 + size), replyData);
        if (st) st.calls++;
        r = 0;
      }
    } catch (err) {
      this.#doorInst = null;
      // A handler cut off midway: the server is poisoned (area +124, the
      // caller's tid), and its pump ends it at its next turn, as when
      // wineserver dies. The caller takes the pipes, which then close.
      Atomics.store(i32, (a + 124) >> 2, tid || -1);
      console.error(`[gex-door] the wineserver trapped in a door call (pid ${pid} tid ${tid}); the server is poisoned: ${err?.stack ?? err}`);
      r = 1;
    }
    const kick = Atomics.exchange(i32, (a + 80) >> 2, 0) !== 0;
    if (Atomics.exchange(i32, L, 0) === 2) Atomics.notify(i32, L);
    if (kick) this.#doorKick();
    return r;
  }

  /** The door lock, contended: spin, then wait (the lock word 2: waiters). */
  #doorLockSlow(i32, a, L, pid, tid) {
    let waitedMs = 0;
    for (let spins = 0; ; spins++) {
      const v = Atomics.compareExchange(i32, L, 0, 1);
      if (v === 0) return;
      if (spins < 200) continue;
      if (v === 1 && Atomics.compareExchange(i32, L, 1, 2) !== 1) continue;
      Atomics.wait(i32, L, 2, 1);
      // A lock held this long is a stuck door (a Worker ended inside one,
      // or a deadlock): say who holds it, every 5 s of waiting.
      if (++waitedMs % 5000 === 0) {
        console.error(`[gex-door] pid ${pid} tid ${tid}: ${waitedMs / 1000} s waiting for the door lock, held by tid ${i32[(a + 116) >> 2]} (proxy state ${i32[(a + 4) >> 2]})`);
      }
    }
  }

  /** `#doorCall` with GEX_DOOR_STATS: the same call, timed, and a report
   *  every GEX_DOOR_STATS ms (default 10 s) or 50,000 calls. */
  #doorCallStats(req, pid, tid) {
    const d = this.#door;
    const st = this.#doorStats;
    if (!d || single.on) {
      if (!(st.nodoor++ & 0x3fff)) console.error(`[gex-door] pid ${pid} tid ${tid}: no door here (${st.nodoor} calls)`);
      return 1;
    }
    const r = this.#doorRun(d, req, pid, tid, st);
    const { i32, dv } = this.#doorView();
    const a = d.area;
    const now = performance.now();
    if (st.T2) { st.tPost += now - st.T2; st.T2 = 0; }
    if (!st.t0) st.t0 = now;
    else if (now - st.t0 >= (globalThis.GEX_DOOR_STATS > 1 ? globalThis.GEX_DOOR_STATS : 10000) || st.calls + st.fallbacks >= 50000) {
      const sa = dv.getUint32(a + 96, true) >> 2;
      const top = [];
      if (sa) for (let n = 0; n < 512; n++) if (i32[sa + n]) top.push([i32[sa + n], n]);
      top.sort((x, y) => y[0] - x[0]);
      console.error(`[gex-door] pid ${pid} tid ${tid}: ${st.calls} door calls, ${st.fallbacks} fallbacks, ${st.proxies} proxies in ${Math.round(now - st.t0)} ms;`
        + ` server: ${i32[(a + 104) >> 2]} calls, ${i32[(a + 100) >> 2]} proxies (${top.slice(0, 16).map(([c, n]) => `${n}:${c}`).join(' ')});`
        + ` per call us: in ${(st.tPre / st.calls * 1000).toFixed(2)} dispatch ${(st.tDisp / st.calls * 1000).toFixed(2)} out ${(st.tPost / st.calls * 1000).toFixed(2)};`
        + ` in-door syscalls ${st.sys} (${(st.tSys / Math.max(1, st.sys) * 1000).toFixed(2)} us each);`
        + ` proxied ${st.proxies} waited ${st.tProxy.toFixed(1)} ms [${Object.entries(st.byNr).sort((x, y) => y[1] - x[1]).slice(0, 6).map(([n, c]) => `${n}:${c}`).join(' ')}];`
        + ` in doors ${(st.tPre + st.tDisp + st.tPost).toFixed(1)} ms`);
      st.tPre = st.tDisp = st.tPost = st.tSys = st.sys = st.tProxy = 0;
      st.byNr = {};
      st.t0 = now; st.calls = st.fallbacks = st.proxies = 0;
    }
    return r;
  }

  /** A native wasm32 program the shim staged (a load, an exec, a fork's
   *  child): published and installed here before the guest runs. */
  #wasmprogServe() {
    const e = this.#shim.exports;
    const n = e.gex_wasmprog_serve?.() ?? -1;
    if (n < 0) return;
    this.#cgenCatchUp();
    e.gex_cgen_pmod_installed(n);
    // libc's registry of the program's shared libraries (cgen/dylink.rs)
    e.gex_wasmprog_ready?.();
  }

  /** Wine's PE module's `gex.syscall` (cgen/wasmdll.rs): a system call from
   *  native code (gexrt marks its pc, bit 61) that traps to JS -- a unit to
   *  install, a thread to start -- is served here and returns, so the native
   *  frames below it survive, as natively. Anything else (the process or
   *  thread ending, a callback's return) goes on up. */
  /** Tier-1's `gex.syscall`: the shim's export itself, except in a process with a
   *  stateful provider, where guest code a provider called (its frames below) stages a
   *  thread or a child rather than trapping, and the host starts it here. */
  #guestSyscall(e) {
    if (!Gex.wasmDlls) return e.gex_syscall;
    const staged = () => {
      if (e.gex_wasmprog_staged()) {
        this.#drainFork().catch((err) => console.error(`[gex] starting a process: ${err.message ?? err}`));
        this.#drainThread().catch((err) => console.error(`[gex] starting a thread: ${err.message ?? err}`));
      }
    };
    const sync = (ctx, pc) => {
      e.gex_syscall(ctx, pc);
      staged();
    };
    return single.on ? this.#suspendingSyscall(sync, staged) : sync;
  }

  /** A wasm64 provider's `gex.syscall(ctx, pc)` (cgen/libprov.rs): marked, so the shim
   *  serves it without leaving the provider's frames (a blocking call waits in place, a
   *  thread is staged); the host then starts what it staged, and the call returns. */
  #providerSyscallSync = (ctx, pc) => {
    const e = this.#shim.exports;
    e.gex_syscall(ctx, BigInt.asIntN(64, BigInt(pc) | (1n << 61n)));
    this.#providerStaged();
  };
  #providerStaged = () => {
    const e = this.#shim.exports;
    if (e.gex_wasmprog_staged()) {
      this.#drainFork().catch((err) => console.error(`[gex] starting a process: ${err.message ?? err}`));
      this.#drainThread().catch((err) => console.error(`[gex] starting a thread: ${err.message ?? err}`));
    }
  };
  #providerSyscallS = null;
  /** The single-thread build's: see `#suspendingSyscall`. */
  get #providerSyscall() {
    if (!single.on) return this.#providerSyscallSync;
    return (this.#providerSyscallS ??= this.#suspendingSyscall(this.#providerSyscallSync, this.#providerStaged, 1n << 61n));
  }

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
    if (!(this.#cgenMode & 3) && e.gex_fjit_on?.()) {
      this.#cgenCatchUp();
      return;
    }
    if (!(this.#cgenMode & 3) || !e.gex_cgen_wanted) return;
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
    // For gex-eject's W3 recorder: which image each unit is, and which images
    // had none; and which program this process is, at its main image.
    console.warn(`[gex-record] unit ${key} ${got ? 'hit' : 'miss'} ${path}`);
    if (/\.exe$/i.test(path)) console.warn(`[gex-record] process ${path} :: ${this.cmdline}`);
    if (!got) {
      if (e.gex_cgen_missing(u)) throw new Error(`gex-cgen: --cgen: ${path} has no function-tier unit (${key})`);
      this.#cgenProviders(u);
      return;
    }
    if (this.#cgenPublish(u, key, path, got) === 1) {
      // Another thread of this process took the same wanted unit first and
      // publishes it (and its providers and region entries); installed here.
      console.error(`gex-nt: function tier for ${path}: placed by another thread`);
      return;
    }
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
    return r;
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
  wlOutput(w, h, scale = 1) {
    // Kept, and given again to every shim instance this process makes: a
    // fresh instance (a fork child's, an exec's) starts at the default
    // 640x480, and W3 starts every Windows program by fork and exec.
    this.#wlOut = [w | 0, h | 0];
    this.#wlScale = Math.max(1, scale | 0);
    this.#shim?.exports?.gex_set_wl_output?.(w | 0, h | 0);
    this.#shim?.exports?.gex_set_wl_scale?.(this.#wlScale);
  }

  #wlOut = null;
  #wlScale = 1;

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
    if (!globalThis.__gexKeepTrace && !globalThis.GEX_TRAP_STACK) Error.stackTraceLimit = 0;
    try {
      return await this.#runLoop(entry, startCtx);
    } finally {
      // An instrumented unit's hits, before a short-lived process is gone.
      if (this.#cov.size) this.covFlush();
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
      if (this.#cov.size && performance.now() - this.#covAt > 250) {
        this.#covAt = performance.now();
        this.covFlush();
      }
      if (trappedAt) {
        this.#shim.exports.gex_nt_note_js_ns?.(ctx, (performance.now() - trappedAt) * 1e6);
        trappedAt = 0;
      }
      try {
        // Address zero is never a guest instruction: entering there is how a
        // lost resume address shows up, far from whatever forgot to set it.
        // An exec handed over from posix_spawn's child whose program could
        // not be loaded: the process has already exited.
        if (!pc && this.#shim.exports.gex_exited?.()) return this.#shim.exports.gex_exit_code();
        if (!pc) {
          throw new Error(
            `gex: nothing said where to resume (ctx=0x${ctx.toString(16)}). ` +
              'A trap gave the stack back without setting a resume address.',
          );
        }
        this.#shim.exports.gex_callout_leave_ok?.(0);
        // The single-thread build enters on a stack of the guest's own
        // (JSPI), which a blocking call suspends; see single.mjs.
        if (single.on) {
          this.#where = 'enter';
          await this.#enterP(ctx, pc);
          this.#where = 'trap';
        }
        else this.#guest.exports.enter(ctx, pc);
        // `enter` returning means the guest gave the stack back for good,
        // normally because the last process exited.
        if (globalThis.GEX_DEBUG_EXIT) {
          console.error(`[gex] run ended: enter returned, code ${this.#shim.exports.gex_exit_code()}`);
        }
        return this.#shim.exports.gex_exit_code();
      } catch (err) {
        trappedAt = performance.now();
        this.#where = 'trap';
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
              'anything printed above is all the guest managed to do first.'
              + (globalThis.GEX_TRAP_STACK ? `\n${err.stack ?? ''}` : ''),
          );
        }
        // A trap abandons the engine's frames without running the epilogues
        // that restore the shim's shadow-stack global, leaking a few dozen
        // bytes each. Nothing is live below here, so reset it exactly -- left
        // alone it slid into the guest's stack and overwrote saved registers.
        this.#shim.exports.__stack_pointer.value = addrVal(this.#workerStack());
        const e = this.#shim.exports;
        // A trap that reached the run loop with a provider's guest call
        // still open abandoned the provider's frames (see provider_try).
        if ((e.gex_libprov_guest_calls?.() ?? 0) > 0) {
          if ((this.#provLeaks = (this.#provLeaks ?? 0) + 1) <= 16) {
            console.error(`[gex] a trap reached the run loop with ${e.gex_libprov_guest_calls()} provider guest call(s) open: ${err?.message ?? err}; ` +
              `resume ${BigInt.asUintN(64, BigInt(e.gex_resume_pc())).toString(16)}\n${err?.stack ?? ''}`);
          }
          e.gex_libprov_guest_calls_set(0);
        }
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
            this.#where = `fillSlot(${want >>> 0})`;
            answer = await this.fillSlot(want >>> 0);
            this.#where = 'trap';
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
        this.#where = 'install';
        while (e.gex_jit_pending_len() >>> 0) await this.#installTranslation();
        this.#where = 'adopt';
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
        this.#where = 'catchUp';
        await this.#catchUp();
        this.#cgenServe();
        this.#where = 'drainFork';
        // A `fork` leaves a child here for the host to start. Drained before
        // the parent runs on, so the two are only ever one process apart --
        // and before the memory is touched again, because what the child gets
        // is a copy of it as `fork` left it.
        await this.#drainFork();
        this.#where = 'drainThread';
        await this.#drainThread();
        this.#where = 'resume';
        // A guest fault the shim staged for the process's hook (W3): run
        // it, and go on where it left the context.
        const faulted = this.#deliverFault(ctx);
        // A process that has exited is not resumed, whatever the shim left in
        // the resume slot. The shim clears it in `exit_process` now, and this
        // is the same rule stated on the side that acts on it: re-entering a
        // guest whose descriptors have been released produces failures that
        // read as bugs anywhere but here.
        const next = e.gex_exited?.() ? 0n : e.gex_resume_pc();
        if (next !== 0n) {
          pc = faulted ?? next;
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
          let snap = this.#netSnap();
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
              // The single-thread build parks for the network as well: the
              // park is a promise, so the event loop delivers the bytes, and
              // the network's activity wakes it (`onActivity`). A turn per
              // waiting fiber instead spins the one thread every fiber shares.
              if (single.on ? (await this.#parkP(20)) < 0 : (e.gex_paused_for_host() || e.gex_park(20) < 0)) {
                // A network wait runs on this Worker's event loop, so it cannot
                // sleep -- but it needs a turn, not a timer: a MessageChannel
                // turn returns to the loop in microseconds where a nested
                // `setTimeout(_, 1)` is clamped to 4ms, which was the whole
                // speed of a download. `gex_paused_for_host` is the network
                // case, so it is the condition for turning.
                const onLoop = !!e.gex_paused_for_host();
                this.#netTurns = onLoop ? this.#netTurns + 1 : 0;
                if (onLoop && e.gex_wait_bound_ms && e.gex_wait_word?.()) {
                  // Asleep until the network, another Worker or the clock.
                  this.waits.net++;
                  await this.#netWait(e, snap);
                } else
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
            snap = this.#netSnap();
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
            // (A thread that ended in a process that goes on -- a joined
            // worker thread -- is this Worker's end too, not a stall.)
            const stalled = (e.gex_exited ? !e.gex_exited() : false) && !(e.gex_ctx_exited?.(ctx) > 0);
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
          this.#shim.exports.gex_callout_leave_ok?.(0);
          f(ctx);
        } else {
          this.#shim.exports.gex_callout_leave_ok?.(0);
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

  // --- The single-thread build (single.mjs, docs/single-thread.md) ---------
  //
  // Every guest thread is a fiber of one realm, its guest on a JSPI stack. A
  // blocking call suspends that stack (`k_ws_park`, below) where the default
  // build waits in `Atomics.wait`, so wherever JS calls into wasm that may
  // block it calls through `WebAssembly.promising`, and wherever wasm calls
  // JS that calls back into wasm that may block, that JS is a
  // `WebAssembly.Suspending` returning the inner call's promise.
  #enterPf = null;
  #enterPOf = null;
  /** `enter`, on a stack of its own. */
  #enterP(ctx, pc) {
    if (this.#enterPOf !== this.#guest) {
      this.#enterPf = WebAssembly.promising(this.#guest.exports.enter);
      this.#enterPOf = this.#guest;
    }
    return this.#enterPf(ctx, pc);
  }
  #parkPf = null;
  /** `gex_park`, on a stack of its own (it suspends in `k_ws_park`). */
  #parkP(ms) {
    this.#parkPf ??= WebAssembly.promising(this.#shim.exports.gex_park);
    return this.#parkPf(ms);
  }
  #sysPf = null;
  /**
   * A system call made from JS that wasm called (a wasm32 program's, a
   * provider's): the shim runs on a stack of its own, so a wait inside it
   * suspends that stack, and the returned promise suspends the caller's.
   * `after` runs when it is done, before the caller goes on (a fork's image
   * is taken there). Nested inside a call-out (JS frames below), nothing can
   * suspend, so it runs as the default build's does.
   */
  #suspendingSyscall(sync, after, mark = 0n) {
    const e = this.#shim.exports;
    return new WebAssembly.Suspending((ctx, pc) => {
      if (this.#hardNested()) return sync(ctx, pc);
      this.#sysPf ??= WebAssembly.promising(e.gex_syscall);
      this.#where = 'syscall';
      const p = this.#sysPf(ctx, mark ? BigInt.asIntN(64, BigInt(pc) | mark) : pc);
      return p.then(() => {
        this.#where = 'syscall-done';
        after?.();
        // A preemption point as well: a program that only makes system
        // calls gives the other fibers a turn here, and `sched_yield` gives
        // the woken one at once (single.mjs `Sched#settle`).
        if (e.gex_last_syscall() === (globalThis.GEX_ARCH === 'x86' ? 24 : 124)) return sched.yieldNow();
        return sched.maybeYield();
      });
    });
  }
  /** A safepoint (every 1024 back edges of translated code): the shim's,
   *  then a turn for the other fibers if the slice is over. */
  #safepointS = null;
  #singleSafepoint(e) {
    return (this.#safepointS ??= new WebAssembly.Suspending((ctx, pc) => {
      e.gex_safepoint(ctx, pc);
      this.#where = 'safepoint';
      return this.#hardNested() ? 0 : sched.maybeYield();
    }));
  }
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
    return this.#drive(this.#callOutSteps(ctx, target, ret, spWant, false));
  }

  /**
   * The single-thread build's call-out: the same loop, with the guest on a
   * stack of its own (`WebAssembly.promising`), so guest code the call-out
   * runs can block -- the import that asked is a `Suspending`, and the
   * whole fiber waits (docs/single-thread.md §7).
   */
  #callOutAsync(ctx, target, ret, spWant) {
    return this.#driveAsync(this.#callOutSteps(ctx, target, ret, spWant, true));
  }

  /** Run a nested loop's steps (`['enter', ctx, pc]` / `['call', f, ...args]`)
   *  synchronously: an exception goes back into the loop at its step. */
  #drive(g) {
    let r = g.next();
    while (!r.done) {
      let v;
      try {
        const [op, a, b, c, d, x] = r.value;
        v = op === 'enter' ? this.#guest.exports.enter(a, b) : a(b, c, d, x);
      } catch (err) {
        // A nested loop never yields to the event loop (a Wine thread's
        // whole life can be one call-out): an instrumented unit's hits go
        // from here too.
        if (this.#cov.size && performance.now() - this.#covAt > 250) {
          this.#covAt = performance.now();
          this.covFlush();
        }
        r = g.throw(err);
        continue;
      }
      r = g.next(v);
    }
    return r.value;
  }

  /** The same, each step on a JSPI stack of its own that may suspend. */
  async #driveAsync(g) {
    let r = g.next();
    while (!r.done) {
      let v;
      try {
        const [op, a, b, c, d, x] = r.value;
        v = op === 'enter' ? await this.#enterP(a, b) : await this.#promisingOf(a)(b, c, d, x);
      } catch (err) {
        r = g.throw(err);
        continue;
      }
      r = g.next(v);
    }
    return r.value;
  }

  /** `WebAssembly.promising` of a wasm function, made once per function. */
  #promising = new WeakMap();
  #promisingOf(f) {
    let p = this.#promising.get(f);
    if (!p) this.#promising.set(f, (p = WebAssembly.promising(f)));
    return p;
  }

  /** Whether a JS frame that cannot suspend is open (a nested loop run
   *  synchronously); `~` marks one that can (single.mjs). */
  #hardNested() {
    for (const n of this.#nest) if (!n.endsWith('~')) return true;
    return false;
  }

  *#callOutSteps(ctx, target, ret, spWant, suspendable) {
    const e = this.#shim.exports;
    const sp0 = e.__stack_pointer.value;
    const me = this.#callouts.length;
    const c = { ret: BigInt.asUintN(64, ret), spWant: BigInt.asUintN(64, spWant), w: null, nestAt: this.#nest.length };
    // A guest call from native Wine (its return is gex-kernel's
    // NATIVE_RETURN_PAD): its native caller's frames are just above the
    // callee's arguments (cgen.rs NATIVE_UNWOUND, as natively).
    c.unwound = c.ret === NATIVE_RETURN_PAD ? NATIVE_UNWOUND : CALLOUT_UNWOUND;
    this.#callouts.push(c);
    this.#nest.push(suspendable ? 'callout~' : 'callout');
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
            e.gex_callout_leave_ok?.(me + 1);
            yield ['call', f, ctx];
          } else {
            e.gex_callout_leave_ok?.(me + 1);
            yield ['enter', ctx, pc];
          }
          // A miss at a call-out's return with only tail calls since this
          // `enter` returns instead of trapping (DISPATCH_MISS_LEAVE).
          const hit = e.gex_callout_hit?.() ?? -1;
          if (hit >= 0) { if (globalThis.GEX_CALLOUT_TRACE) console.error(`[callout] depth ${me} leaves: hit ${hit} (no trap)`); return hit === me ? 1 : 0; }
          console.error('[gex] guest code returned out of a call-out');
          return 0;
        } catch (err) {
          e.__stack_pointer.value = sp0;
          const sig = this.#calloutSignal;
          this.#calloutSignal = null;
          // A call-out ended here was the trap's whole business: the resume
          // the miss path set at its return (`cgen::returned`) is spent, and
          // left in place it is where the run loop next goes -- the next
          // time this thread blocks with nothing else to run, it "resumed"
          // at the provider's return address (HK under ?fjit, docs §12 34).
          // (Only this one's: an outer call-out's hit returns through the
          // frames between, which leave as before.)
          const ended = (v) => {
            if (v === 1) {
              e.gex_resume_pc();
              e.gex_resume_ctx();
            }
            return v;
          };
          if (sig?.kind === 'return') return ended(sig.i === me ? 1 : 0);
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
          if (hit >= 0) { if (globalThis.GEX_CALLOUT_TRACE) console.error(`[callout] depth ${me} leaves: hit ${hit}`); return ended(hit === me ? 1 : 0); }
          // gex_nr::NATIVE_LEFT (gexrt saw the guest leave a native caller's
          // frames): a native caller's call-out leaves, at the context's PC
          // (the entry the guest makes again); anything else carries on.
          if (err?.gexNativeLeft && c.ret === NATIVE_RETURN_PAD
              && new DataView(this.#memory.buffer).getBigUint64(ctx + CTX_SP, true) > c.spWant + c.unwound) { if (globalThis.GEX_CALLOUT_TRACE) console.error(`[callout] depth ${me} leaves: native left`); return 0; }
          // The process ended: down through every frame to the run loop
          // (returning would run the guest on).
          if (e.gex_exited?.()) {
            err.gexHandoff = true;
            throw err;
          }
          // Control left an exception dispatched nested further out: this
          // call-out's caller leaves, and the dispatch's loop sees the escape
          // at the next check (nt.rs `exc_escape`).
          if ((e.gex_exc_escaped?.(0) ?? -1) >= 0) { if (globalThis.GEX_CALLOUT_TRACE) console.error(`[callout] depth ${me} leaves: exception escaped`); return 0; }
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
          const faulted = this.#deliverFault(ctx);
          const resumed = e.gex_resume_pc();
          const nextPc = faulted ?? resumed;
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
            if (globalThis.GEX_CALLOUT_TRACE) console.error(`[callout] depth ${me} leaves: resume on ctx ${rc.toString(16)} not ${ctx.toString(16)}`);
            return 0;
          }
          // The stack unwound past the caller (longjmp, an exception
          // handled further out): its frame is gone, so it leaves.
          const sp = new DataView(this.#memory.buffer).getBigUint64(ctx + CTX_SP, true);
          if (sp > c.spWant + c.unwound) {
            if (globalThis.GEX_CALLOUT_TRACE) console.error(`[callout] depth ${me} leaves: unwound (sp ${sp.toString(16)}, resume ${pc.toString(16)})`);
            new DataView(this.#memory.buffer).setBigUint64(ctx + CTX_PC, pc, true);
            return 0;
          }
          this.#armCallouts();
        }
      }
    } finally {
      e.gex_callout_leave_ok?.(0);
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
   * A guest fault the shim staged (`gex_fault_take`: a W3 process's branch
   * to non-executable memory) given to the process's fault hook, Wine's
   * ntdll.so, as shims/native's `deliver_guest_fault_kind` does: the hook
   * builds the guest's exception frame and points the context at the
   * dispatcher. Returns where the guest goes on, or null with none staged.
   */
  #deliverFault(ctx) {
    const e = this.#shim.exports;
    if (!e.gex_fault_take) return null;
    const buf = e.gex_alloc(16) >>> 0;
    let hook, code, addr, kind;
    try {
      hook = e.gex_fault_take(buf) >>> 0;
      if (!hook) return null;
      const dv = new DataView(this.#memory.buffer);
      code = dv.getUint32(buf, true);
      addr = dv.getBigUint64(buf + 4, true);
      kind = dv.getUint32(buf + 12, true);
    } finally {
      e.gex_free?.(buf, 16);
    }
    const f = this.#cgenStore().ftab.get(tIdx(hook));
    if (!f) throw new Error(`gex: the guest fault hook (function ${hook}) is not in this Worker's table`);
    // The hook's C frames go below the guest's stack pointer, as a signal
    // frame does, with room for the exception frame it builds there.
    const spG = this.#wasmprogStack();
    const cur = spG.value;
    const esp = new DataView(this.#memory.buffer).getUint32(ctx + CTX_SP, true);
    const low = Math.min(Number(cur) >>> 0, esp);
    spG.value = m64() ? BigInt(((low - 4096) & ~15) >>> 0) : ((low - 4096) & ~15) | 0;
    try {
      f(ctx, code | 0, Number(BigInt.asUintN(32, addr)) | 0, kind | 0);
    } finally {
      spG.value = cur;
    }
    return BigInt.asUintN(64, new DataView(this.#memory.buffer).getBigUint64(ctx + CTX_PC, true));
  }

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
            this.#shim.exports.gex_callout_leave_ok?.(0);
            f(ctx);
          } else {
            this.#shim.exports.gex_callout_leave_ok?.(0);
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
          const faulted = this.#deliverFault(ctx);
          const resumed = e.gex_resume_pc();
          const nextPc = faulted ?? resumed;
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

  /**
   * The process's JS-side module store: a growable SharedArrayBuffer every
   * Worker of the process holds, where the registry keeps a joining Worker's
   * catch-up bytes instead of in the shim's heap (which shares a fixed 2 GiB
   * arena; the registry alone reached 980 MiB in a Muck fjit run). Null where
   * there is no growable SharedArrayBuffer; the registry then keeps the bytes
   * itself, as before.
   */
  #moduleStore = null;

  /** Registry module `i`'s bytes (a copy), wherever they are kept; null if none. */
  /** Registry module `i`'s content key: as the store's writer recorded it
   *  when the module is in the store, so a catching-up Worker does not hash
   *  bytes a sibling already hashed; computed from `bytes` otherwise. */
  #registryKey(i, bytes) {
    const r = this.#registryRow(i);
    if (r.ext) return `${bytes.length}:${r.keyHi}:${r.keyLo}`;
    return moduleContentKey(bytes);
  }

  /** Registry module `i`'s row: where its bytes are (`ptr`, or `ext`, the
   *  module store's offset plus one), how many, and the store writer's key.
   *  One call and a read of the row (`gex_module_row`), which the shim
   *  publishes for readers holding no lock; the getters one field at a time
   *  for a shim without it. Remembered for the last `i` asked, since a
   *  catch-up asks about each module several times running. */
  #registryRow(i) {
    if (this.#rowMemo?.i === i) return this.#rowMemo;
    const e = this.#shim.exports;
    let r;
    const at = e.gex_module_row ? e.gex_module_row(i) >>> 0 : -1;
    if (at > 0) {
      const v = new DataView(this.#bytes().buffer, at, 24);
      r = { i, ptr: v.getUint32(0, true), len: v.getUint32(4, true), ext: Number(v.getBigUint64(8, true)),
        keyHi: v.getUint32(20, true), keyLo: v.getUint32(16, true) };
    } else if (at === 0) {
      // Not published yet: not remembered either.
      return { i, ptr: 0, len: 0, ext: 0, keyHi: 0, keyLo: 0 };
    } else {
      const ext = e.gex_module_ext ? Number(e.gex_module_ext(i)) : 0;
      r = { i, ptr: e.gex_module_ptr(i) >>> 0, len: e.gex_module_len(i) >>> 0, ext,
        keyHi: ext && e.gex_module_key ? e.gex_module_key(i, 0) >>> 0 : 0,
        keyLo: ext && e.gex_module_key ? e.gex_module_key(i, 1) >>> 0 : 0 };
    }
    this.#rowMemo = r;
    return r;
  }
  #rowMemo = null;

  #registryBytes(i) {
    const { ptr, len, ext } = this.#registryRow(i);
    if (!len) return null;
    if (ext) {
      if (!this.#moduleStore) throw new Error(`gex: registry module ${i} is in the module store, and this Worker has none`);
      return new Uint8Array(this.#moduleStore, ext - 1, len).slice();
    }
    return this.#bytes().slice(ptr, ptr + len);
  }

  /** Before `gex_jit_installed`: when the registry will keep this module, put
   *  its bytes in the module store and tell the shim where, so it keeps the
   *  place instead of the bytes. */
  #noteExternal(bytes, key) {
    const e = this.#shim.exports;
    if (!this.#moduleStore || !key || !e.gex_jit_note_external || !e.gex_jit_keeps?.()) return;
    const off = storeAppend(this.#moduleStore, bytes);
    if (off < 0) return;
    // Its size now and then (every 64 MiB; every MiB under
    // GEX_REGISTRY_TRACE), beside what the registry still holds in the heap.
    const trace = globalThis.GEX_REGISTRY_TRACE || globalThis.process?.env?.GEX_REGISTRY_TRACE;
    const step = trace ? 20 : 26;
    if (Math.floor((off + bytes.length) / 2 ** step) !== Math.floor(off / 2 ** step)) {
      const line = `[gex] module store at ${((off + bytes.length) / 1048576).toFixed(0)} MiB; registry bytes in the shim heap: ${e.gex_registry_mib?.(0) ?? '?'} MiB`;
      // Traced under Node, straight to fd 2: a Worker's console output can be
      // lost when the process exits soon after (tests/browser-registry.sh).
      const fs = trace ? globalThis.process?.getBuiltinModule?.('fs') : null;
      if (fs) fs.writeSync(2, `${line}\n`);
      // A size report, not an error (a console error fails a page test).
      else narrate(line);
    }
    const [, a, b] = key.split(':');
    e.gex_jit_note_external(BigInt(off), bytes.length, Number(a) | 0, Number(b) | 0);
  }

  /** `#installTranslation` without yielding, for `#ntCallback`. */
  #installTranslationSync() {
    const e = this.#shim.exports;
    // The queue's front now, not whenever this Worker last pushed or popped:
    // a module a sibling installed since may have been freed.
    e.gex_jit_pending_refresh?.();
    const ptr = e.gex_jit_pending_ptr() >>> 0;
    const len = e.gex_jit_pending_len() >>> 0;
    if (len === 0) return;
    const bytes = this.#bytes().slice(ptr, ptr + len);
    const key = moduleContentKey(bytes);
    const ukey = this.#unitKey(bytes);
    this.#installModule(bytes, e.gex_jit_slots() >>> 0, 64, null, 'nt-callback', this.#unitGet(ukey), true);
    this.#unitShare(ukey);
    this.#noteExternal(bytes, key);
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

  /**
   * With the page's unit cache (`?unitcache=1`, GEX_UNIT_CACHE): a
   * translation's key in it, `u:` and its content key with the dense base
   * word zeroed -- the one part of a unit's bytes that differs between the
   * processes it is installed in (gex_emit::dense::GLOBAL) -- else null.
   * `bytes` is a private copy; the word is put back.
   */
  #unitKey(bytes) {
    if (!globalThis.GEX_UNIT_CACHE || !(bytes instanceof Uint8Array)) return null;
    const at = denseBaseAt(bytes);
    if (at < 0) return `u:${moduleContentKey(bytes)}`;
    const w = bytes.slice(at, at + 4);
    bytes.fill(0, at, at + 4);
    const k = moduleContentKey(bytes);
    bytes.set(w, at);
    return `u:${k}`;
  }

  /** The compiled module for `ukey`, counted (`unitHits`), or null. */
  #unitGet(ukey) {
    const m = (ukey && this.#sharedModules.get(ukey)) || null;
    if (m) this.#unitHits++;
    return m;
  }
  #unitHits = 0;
  get unitHits() { return this.#unitHits; }

  /** The module just installed, to the page's unit cache under `ukey`. */
  #unitShare(ukey) {
    if (!ukey || this.#sharedModules.has(ukey)) return;
    const mod = this.#compiled[this.#compiled.length - 1]?.mod;
    if (!mod) return;
    this.#sharedModules.set(ukey, mod);
    this.#env.shareModule?.(ukey, mod);
  }

  /** `#catchUp` (eager) without yielding, for `#ntCallback`. */
  #catchUpSync() {
    const e = this.#shim.exports;
    if (!e.gex_module_count) return;
    const have = e.gex_module_count() >>> 0;
    const need = (e.gex_jit_slots_total?.() ?? 0) >>> 0;
    for (; this.#installed < have; this.#installed++) {
      const i = this.#installed;
      const bytes = this.#registryBytes(i);
      if (!bytes) continue;
      const key = this.#registryKey(i, bytes);
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
    // The queue's front now, not whenever this Worker last pushed or popped:
    // a module a sibling installed since may have been freed.
    e.gex_jit_pending_refresh?.();
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
    // A module another process compiled (a served unit) comes from the
    // page's unit cache: instantiated, not compiled again.
    const ukey = this.#unitKey(bytes);
    await this.#installModule(bytes, e.gex_jit_slots() >>> 0, 64, null, 'jit2878', this.#unitGet(ukey));
    this.#unitShare(ukey);
    this.#noteExternal(bytes, key);
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
  async #installModule(bytes, need, segments = 64, meta = null, who = '?', precompiled = null, sync = false, dense = null) {
    const t0 = performance.now();
    this.#installs++;
    if (sync) {
      // Timed here: the inner call has done all its work when it returns,
      // and a `finally` after an `await` would run only once the guest
      // next yields, counting everything it ran meanwhile (a node start-up
      // reported hours of "installing").
      const done = this.#installModuleInner(bytes, need, segments, meta, who, precompiled, sync, dense);
      this.#installMs += performance.now() - t0;
      return done;
    }
    try {
      // With `sync` the inner call never awaits, so the install is complete
      // when this returns its promise -- what a nested loop inside an import
      // needs, since it cannot yield.
      return await this.#installModuleInner(bytes, need, segments, meta, who, precompiled, sync, dense);
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
  async #installModuleInner(bytes, need, segments = 64, meta = null, who = '?', precompiled = null, sync = false, dense = null) {
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
    // Under Node, `GEX_DUMP_WASM=DIR` keeps every unit compiled, for
    // looking at what a translation is made of (docs/jit-guest.md).
    if (bytes instanceof Uint8Array && globalThis.process?.env?.GEX_DUMP_WASM) {
      try { globalThis.process.getBuiltinModule('fs').writeFileSync(`${globalThis.process.env.GEX_DUMP_WASM}/u-${this.#shim.exports.gex_my_pid?.() ?? 0}-${moduleContentKey(bytes).replaceAll(':', '-')}.wasm`, bytes); } catch {}
    }
    // With its metadata: a fork hands the child `#compiled`, and without the
    // metadata the child cannot tell which module covers which page and must
    // instantiate all of them. The section is in the bytes; keeping it is a
    // parse.
    // And its dense base as THIS process numbered it (`gex.dense` from its
    // bytes, or what it was handed): with the page's unit cache, `mod` may be
    // another process's compile, whose own section names that process's base,
    // and a fork child instantiating from `mod` would put the blocks there.
    const ds = dense ?? denseSection(bytes instanceof Uint8Array ? bytes : mod);
    this.#compiled.push({
      mod,
      need,
      meta: bytes instanceof Uint8Array ? metadataSection(bytes) : undefined,
      dense: ds ?? undefined,
    });
    const imports = { ...this.#imports };
    for (let i = 0; i < tables.length; i++) imports[TABLE_NAMES[i]] = tables[i];
    // A runtime module imports its dense base (gex_emit::dense::GLOBAL)
    // whether or not it has dense blocks; set below when it has.
    imports.dense_base = 0;
    // Dense: the module's blocks go to `blocks` at the base its bytes name,
    // within what the process has numbered.
    if (this.#guest?.exports?.blocks) {
      imports.blocks = this.#guest.exports.blocks;
      this.#growBlocks();
      if (ds) {
        const dv = new DataView(ds.buffer, ds.byteOffset, ds.byteLength);
        const base = dv.getUint32(0, true);
        if (globalThis.GEX_DENSE_DEBUG) {
          const theirs = bytes instanceof Uint8Array ? denseSection(mod) : null;
          const tb = theirs ? new DataView(theirs.buffer, theirs.byteOffset).getUint32(0, true) : base;
          console.error(`[dense] pid ${e.gex_my_pid?.() ?? '?'} ${who} base=${base} n=${dv.getUint32(4, true)} blocks=${imports.blocks.length} size=${e.gex_dense_size()}`
            + (tb !== base ? ` (a module compiled elsewhere, at base ${tb})` : ''));
        }
        // Base 0 is a module nobody numbered: its blocks would land on the
        // miss stub and its neighbours.
        if (base === 0) throw new Error(`gex: a translation with unnumbered dense blocks reached the install (${who})`);
        // Where a module places its dense segment by import rather than by a
        // constant in its bytes (gex_emit::dense::GLOBAL).
        imports.dense_base = base;
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
        await this.#installModule(lazy.mod, lazy.need, 64, lazy.meta, 'inherit', null, false, lazy.dense);
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
    const bytes = this.#registryBytes(i);
    if (!bytes) return;
    const key = this.#registryKey(i, bytes);
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

/** A PIC module's `dylink.0` table_size (WASM_DYLINK_MEM_INFO: mem_size,
 *  mem_align, table_size, table_align, LEB128). */
function dylinkTableSize(mod) {
  const sec = WebAssembly.Module.customSections(mod, 'dylink.0')[0];
  if (!sec) throw new Error('door: the server program has no dylink.0 section');
  const b = new Uint8Array(sec);
  let i = 0;
  const leb = () => { let r = 0, sh = 0, c; do { c = b[i++]; r += (c & 0x7f) * 2 ** sh; sh += 7; } while (c & 0x80); return r; };
  while (i < b.length) {
    const kind = b[i++], len = leb(), end = i + len;
    if (kind === 1) { leb(); leb(); return leb(); }
    i = end;
  }
  throw new Error('door: no WASM_DYLINK_MEM_INFO in the server program');
}
