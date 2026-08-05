// Cross-engine check: run a gex-translated module on V8.
//
// The native host is wasmtime. The browser target is V8, and the whole design
// rests on features — tail calls above all — whose behaviour must not differ
// between them. This script takes the *same* .wasm `gexc build` produced and
// executes it under Node, so a divergence in engine support shows up here
// rather than at the end of the browser milestone.
//
// It is not the browser shim. The real runtime compiles gex-kernel to wasm and
// shares one implementation with the native host (PLAN §9); the handful of
// syscalls below exist only to get a program to its exit, and anything beyond
// them fails loudly rather than quietly returning a plausible answer.
//
//   node tests/cross-engine/run-node.mjs <module.wasm> <program.elf>

import { readFileSync } from 'node:fs';
import { writeSync } from 'node:fs';

// Must match crates/gex-abi.
const CTX_BASE = 0x1000;
const CTX_SP = 31 * 8;
const CTX_PC = 32 * 8;
const GUEST_MEM_PAGES = 0xffff0000 / 65536;
const STACK_TOP = 0x8000_0000;

const NR = {
  IOCTL: 29, WRITE: 64, WRITEV: 66, EXIT: 93, EXIT_GROUP: 94,
  SET_TID_ADDRESS: 96, BRK: 214, MUNMAP: 215, MMAP: 222, MPROTECT: 226,
  GETRANDOM: 278, CLOCK_GETTIME: 113, RT_SIGACTION: 134, RT_SIGPROCMASK: 135,
  SET_ROBUST_LIST: 99, PRLIMIT64: 261, GETUID: 174, GETEUID: 175,
  GETGID: 176, GETEGID: 177, GETPID: 172, UNAME: 160, FSTAT: 80, READLINKAT: 78,
};

class Exit extends Error {
  constructor(code) { super(`guest exited with ${code}`); this.code = code; }
}

// -- fused multiply-add ----------------------------------------------------
//
// `gex.fma` must round exactly once. JS has no scalar FMA, and rounding the
// product first is what makes musl's `pow` return 1024.0000000000002 for
// `2^10`, so an approximation would show up as a cross-engine divergence that
// is not gex's fault. This is therefore exact rather than fast: the f64 case
// does the arithmetic in BigInt and rounds by hand, and the f32 case falls out
// of doing it in f64, where a 24x24-bit product and the addend still fit.

const scratch = new DataView(new ArrayBuffer(8));

function f64FromBits(bits) {
  scratch.setBigUint64(0, BigInt.asUintN(64, bits), true);
  return scratch.getFloat64(0, true);
}
function bitsFromF64(v) {
  scratch.setFloat64(0, v, true);
  return scratch.getBigUint64(0, true);
}
function f32FromBits(bits) {
  scratch.setUint32(0, Number(BigInt.asUintN(32, bits)), true);
  return scratch.getFloat32(0, true);
}
function bitsFromF32(v) {
  scratch.setFloat32(0, v, true);
  return BigInt(scratch.getUint32(0, true));
}

/** Split a double into `{ sign, m, e }` with value = (-1)^sign * m * 2^e. */
function decompose(x) {
  const bits = bitsFromF64(x);
  const sign = Number((bits >> 63n) & 1n);
  const rawExp = Number((bits >> 52n) & 0x7ffn);
  const frac = bits & 0xf_ffff_ffff_ffffn;
  if (rawExp === 0) return { sign, m: frac, e: -1074 };        // subnormal or zero
  return { sign, m: frac | 0x10_0000_0000_0000n, e: rawExp - 1075 };
}

/** Round `(sign, m, e)` -- an exact value -- to the nearest double. */
function roundToDouble(sign, m, e) {
  if (m === 0n) return sign ? -0 : 0;
  // Normalise to exactly 53 significant bits, keeping guard and sticky.
  let bits = m.toString(2).length;
  if (bits > 53) {
    const drop = BigInt(bits - 53);
    const rest = m & ((1n << drop) - 1n);
    const half = 1n << (drop - 1n);
    m >>= drop;
    e += Number(drop);
    // Nearest, ties to even.
    if (rest > half || (rest === half && (m & 1n) === 1n)) {
      m += 1n;
      if (m.toString(2).length > 53) { m >>= 1n; e += 1; }
    }
  }
  let v = Number(m) * Math.pow(2, e);
  // Math.pow can underflow to zero in two steps; scale in halves if so.
  if (!isFinite(v) || (v === 0 && m !== 0n)) {
    v = Number(m);
    let k = e;
    while (k > 0) { const step = Math.min(k, 512); v *= Math.pow(2, step); k -= step; }
    while (k < 0) { const step = Math.max(k, -512); v *= Math.pow(2, step); k -= step; }
  }
  return sign ? -v : v;
}

function fmaF64(a, b, c) {
  const p = a * b;
  // Specials and zeros: the ordinary expression already gives the right
  // answer, and the exact path below has no way to represent them.
  if (!isFinite(a) || !isFinite(b) || !isFinite(c) || a === 0 || b === 0) {
    return p + c;
  }
  const A = decompose(a), B = decompose(b), C = decompose(c);
  const sp = A.sign ^ B.sign;
  let pm = A.m * B.m;
  let pe = A.e + B.e;
  let cm = C.m, ce = C.e, cs = C.sign;
  if (cm === 0n) return roundToDouble(sp, pm, pe);
  // Align to the lower exponent so both are integers of the same scale.
  if (pe > ce) { pm <<= BigInt(pe - ce); pe = ce; } else { cm <<= BigInt(ce - pe); ce = pe; }
  let sign, m;
  if (sp === cs) {
    sign = sp;
    m = pm + cm;
  } else if (pm >= cm) {
    sign = sp;
    m = pm - cm;
  } else {
    sign = cs;
    m = cm - pm;
  }
  if (m === 0n) return 0;  // exact cancellation is +0 under round-to-nearest
  return roundToDouble(sign, m, pe);
}

function fma(aBits, bBits, cBits, double) {
  if (double) {
    return BigInt.asUintN(
      64,
      bitsFromF64(fmaF64(f64FromBits(aBits), f64FromBits(bBits), f64FromBits(cBits))),
    );
  }
  // A 24x24-bit product and a 24-bit addend are exact in f64, so one rounding
  // back to f32 is the whole operation.
  const r = f32FromBits(aBits) * f32FromBits(bBits) + f32FromBits(cBits);
  return BigInt.asUintN(64, bitsFromF32(Math.fround(r)));
}

/** Minimal ELF64 reader: just enough to place the segments. */
function loadElf(bytes) {
  const dv = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  const u16 = (o) => dv.getUint16(o, true);
  const u64 = (o) => Number(dv.getBigUint64(o, true));
  const isPie = u16(16) === 3;
  const bias = isPie ? 0x2000_0000 : 0;
  const entry = u64(24) + bias;
  const phoff = u64(32), phentsize = u16(54), phnum = u16(56);

  const segments = [];
  let brk = 0, phdrVaddr = 0;
  for (let i = 0; i < phnum; i++) {
    const o = phoff + i * phentsize;
    if (dv.getUint32(o, true) !== 1) continue; // PT_LOAD
    const offset = u64(o + 8), vaddr = u64(o + 16) + bias;
    const filesz = u64(o + 32), memsz = u64(o + 40);
    segments.push({ vaddr, memsz, bytes: bytes.subarray(offset, offset + filesz) });
    brk = Math.max(brk, vaddr + memsz);
    if (offset <= phoff && phoff < offset + filesz) phdrVaddr = vaddr + (phoff - offset);
  }
  return { entry, segments, brk: (brk + 0xfff) & ~0xfff, phdrVaddr, phentsize, phnum, bias };
}

function buildStack(mem, image, argv, envp) {
  const u8 = new Uint8Array(mem.buffer);
  const dv = new DataView(mem.buffer);
  const enc = new TextEncoder();

  const blob = [];
  const argvOff = [], envOff = [];
  const push = (s) => { const o = blob.length; for (const b of enc.encode(s)) blob.push(b); blob.push(0); return o; };
  for (const a of argv) argvOff.push(push(a));
  for (const e of envp) envOff.push(push(e));
  while (blob.length % 8) blob.push(0);
  const randomOff = blob.length;
  for (let i = 0; i < 16; i++) blob.push(0x5a);

  const blobAddr = (STACK_TOP - blob.length) & ~0xf;
  const aux = [
    [3, image.phdrVaddr], [4, image.phentsize], [5, image.phnum], [6, 4096],
    [7, 0], [8, 0], [9, image.entry], [11, 1000], [12, 1000], [13, 1000],
    [14, 1000], [16, 0x1 | 0x2 | 0x100 | 0x800], [17, 100], [23, 0],
    [25, blobAddr + randomOff], [0, 0],
  ];
  const words = [argv.length, ...argvOff.map((o) => blobAddr + o), 0,
                 ...envOff.map((o) => blobAddr + o), 0, ...aux.flat()];
  const sp = (blobAddr - words.length * 8) & ~0xf;

  for (let i = 0; i < words.length; i++) dv.setBigUint64(sp + i * 8, BigInt(words[i]), true);
  u8.set(Uint8Array.from(blob), blobAddr);
  return sp;
}

const [, , wasmPath, elfPath, ...guestArgs] = process.argv;
if (!wasmPath || !elfPath) {
  console.error('usage: run-node.mjs <module.wasm> <program.elf> [args...]');
  process.exit(2);
}

const module_ = new WebAssembly.Module(readFileSync(wasmPath));
const image = loadElf(new Uint8Array(readFileSync(elfPath)));

let memory;
// A top-down allocator matching gex-kernel's, so the two hosts hand out the
// same addresses and a divergence cannot be blamed on layout.
let mmapNext = 0x7000_0000;
let brk = image.brk;

const rd64 = (a) => new DataView(memory.buffer).getBigUint64(a, true);
const wr64 = (a, v) => new DataView(memory.buffer).setBigUint64(a, BigInt.asUintN(64, v), true);
const reg = (n) => Number(BigInt.asUintN(64, rd64(CTX_BASE + n * 8)));

function syscall(ctx, pc) {
  const n = reg(8);
  const a = [0, 1, 2, 3, 4, 5].map(reg);
  let r = 0n;
  switch (n) {
    case NR.EXIT: case NR.EXIT_GROUP: throw new Exit(a[0] & 0xff);
    case NR.WRITE: {
      const buf = new Uint8Array(memory.buffer, a[1], a[2]);
      r = BigInt(writeSync(a[0], buf));
      break;
    }
    case NR.WRITEV: {
      let total = 0;
      for (let i = 0; i < a[2]; i++) {
        const base = Number(rd64(a[1] + i * 16));
        const len = Number(rd64(a[1] + i * 16 + 8));
        if (len) total += writeSync(a[0], new Uint8Array(memory.buffer, base, len));
      }
      r = BigInt(total);
      break;
    }
    case NR.BRK: {
      if (a[0] !== 0 && a[0] >= image.brk && a[0] < 0x3000_0000) brk = (a[0] + 0xfff) & ~0xfff;
      r = BigInt(a[0] === 0 ? brk : a[0]);
      break;
    }
    case NR.MMAP: {
      const len = (a[1] + 0xfff) & ~0xfff;
      mmapNext -= len;
      new Uint8Array(memory.buffer, mmapNext, len).fill(0);
      r = BigInt(mmapNext);
      break;
    }
    case NR.SET_TID_ADDRESS: r = 1n; break;
    case NR.GETPID: r = 1n; break;
    case NR.GETUID: case NR.GETEUID: case NR.GETGID: case NR.GETEGID: r = 1000n; break;
    case NR.GETRANDOM: {
      new Uint8Array(memory.buffer, a[0], a[1]).fill(0x5a);
      r = BigInt(a[1]);
      break;
    }
    case NR.IOCTL: r = BigInt.asUintN(64, -25n); break; // ENOTTY
    case NR.MUNMAP: case NR.MPROTECT: case NR.RT_SIGACTION:
    case NR.RT_SIGPROCMASK: case NR.SET_ROBUST_LIST: case NR.PRLIMIT64:
      r = 0n; break;
    case NR.CLOCK_GETTIME: {
      const now = Date.now();
      wr64(a[1], BigInt(Math.floor(now / 1000)));
      wr64(a[1] + 8, BigInt((now % 1000) * 1e6));
      break;
    }
    default:
      // Anything unlisted would need a real implementation; guessing here
      // would turn a missing feature into a wrong answer.
      throw new Error(`cross-engine host: syscall ${n} at pc=0x${pc.toString(16)} not implemented`);
  }
  wr64(CTX_BASE, r);
}

// The guest's memory belongs to the host, not the module: every guest thread
// runs in its own store (its own Worker, in a browser) and they all act on the
// same bytes. Shared, so a SharedArrayBuffer is what backs it.
const guestMemory = new WebAssembly.Memory({
  initial: GUEST_MEM_PAGES,
  maximum: GUEST_MEM_PAGES,
  shared: true,
});

const imports = {
  gex: {
    memory: guestMemory,
    // Wasm has no fused multiply-add and rounding twice is not close enough:
    // musl's pow is built out of this instruction. JS has no scalar FMA
    // either, so this is the software one -- exact for the case that matters,
    // which is recovering the rounding error of a product.
    fma: (ctx, a, b, c, double) => fma(a, b, c, double !== 0),
    syscall: (ctx, pc) => syscall(ctx, Number(pc)),
    dispatch_miss: (ctx, pc) => {
      throw new Error(`branch to 0x${pc.toString(16)} with no translation`);
    },
    illegal: (ctx, pc, raw) => {
      throw new Error(`instruction 0x${(raw >>> 0).toString(16)} at 0x${pc.toString(16)} not implemented`);
    },
    breakpoint: (ctx, pc) => { throw new Error(`brk at 0x${pc.toString(16)}`); },
    sysreg_read: () => 0n,
    sysreg_write: () => {},
    cache_op: () => {},
    safepoint: () => {},
  },
};

const instance = new WebAssembly.Instance(module_, imports);
memory = guestMemory;
if (memory.buffer.byteLength < GUEST_MEM_PAGES * 65536) {
  console.error('cross-engine host: memory is smaller than expected');
  process.exit(1);
}

const u8 = new Uint8Array(memory.buffer);
for (const seg of image.segments) {
  u8.fill(0, seg.vaddr, seg.vaddr + seg.memsz);
  u8.set(seg.bytes, seg.vaddr);
}
const sp = buildStack(memory, image, [elfPath, ...guestArgs], ['PATH=/usr/bin:/bin', 'HOME=/root']);
wr64(CTX_BASE + CTX_SP, BigInt(sp));
wr64(CTX_BASE + CTX_PC, BigInt(image.entry));

try {
  instance.exports.enter(CTX_BASE, BigInt(image.entry));
  process.exit(0);
} catch (e) {
  if (e instanceof Exit) process.exit(e.code);
  console.error(`cross-engine host: ${e.message}`);
  process.exit(1);
}
