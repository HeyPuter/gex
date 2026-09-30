# PowerPC (32-bit, big-endian) as a gex guest

Stream P1. Branch `feature/ppc`, not merged anywhere until the user approves.
Written 2026-09-30. This is the design. What is built, and what is measured,
is at the end (§12).

**Scope.** 32-bit PowerPC, big-endian, Linux SysV ABI, musl: ELF32 MSB,
`EM_PPC` (20). ppc64 is out of scope, and nothing here keeps a path open for
it unless doing so costs nothing. The baseline CPU is the classic 32-bit
integer + FPU architecture (`-mcpu=ppc`, a 603/750-class machine):

- no AltiVec/VMX, no SPE, no VSX;
- no 64-bit instructions;
- vector, SPE and 64-bit encodings decode to "unknown" and fail loudly (SIGILL
  at the instruction, with its address and raw word).

**Toolchain.** zig can't provide libc for `powerpc-linux-musl`. The guest
toolchain is clang + lld against a musl sysroot that the repository builds:

- `tools/ppc/build-sysroot.sh` builds musl 1.2.5 and compiler-rt's generic
  builtins with `clang --target=powerpc-linux-musl -mcpu=ppc -mno-altivec` into
  `../ppc-sysroot`, outside git. It then checks that the result has no vector
  instructions.
- `tools/ppc/cc.sh` compiles and links a static executable against it at
  0x1000000.
- `qemu-ppc` is the oracle.

musl on ppc32 has `long double` = `double`, so there is no IBM double-double
to worry about.

---

## 1. Shape of the change

One guest ISA per build, as with `x86`. A new cargo feature, `ppc`:

| crate | what `ppc` changes |
| --- | --- |
| `gex-abi` | `PPC` constant; `ARCH_NAME = "ppc"`, `ELF_MACHINE = 20`, `INST_ALIGN_SHIFT = 2`, `DISPATCH_SHIFT = 2` (unchanged), `SYSCALL_INST_BYTES = 4`; the context slots in §4; `BIG_ENDIAN = true` and the guest-endian helpers (§3). A `compile_error!` if `x86` and `ppc` are both on. |
| `gex-isa-ppc` (new) | the decoder and disassembler |
| `gex-lift-ppc` (new) | the semantics, in GIR |
| `gex-ir` | additive only: the PowerPC registers (§4), `UnOp::Bswap` (§2), and a rule that a 32-bit guest may address memory with an `I32` (§2.3) |
| `gex-emit` | the new registers' slots; the `Bswap` lowering; the `I32` address path. Everything is under `gex_abi::PPC` or reachable only from PowerPC GIR, so AArch64 and x86-64 modules stay byte-identical. |
| `gex-xlate` | `arch.rs` grows a third `imp` (big-endian fetch, the ppc decoder and lifter) |
| `gex-ld` | ELF32 MSB, `EM_PPC`, the 32-bit big-endian initial stack and auxv |
| `gex-kernel` | the endian layer in `GuestMem` (§3); `ppc.rs`: syscall numbering, argument fixups, the error convention (§6), signal frames (§7) |
| shims, `gexc` | feature pass-through; the SO-bit result convention wherever a shim writes a syscall result |

The x86 guest's i386 compatibility mode already taught the kernel 32-bit
layouts (iovec, stat64, sigaction, execve's vectors, the `time32` calls)
under `Kernel::compat`. A ppc process sets `compat` too, and the few
places where "32-bit" meant "i386" (argument registers, the TLS slot,
stat64's layout, the signal frame, `uname`'s machine) get a `PPC` arm.

---

## 2. Big-endian guest, little-endian Wasm memory

Guest address = linear-memory offset, as for every gex guest, and the bytes
are stored in **guest order**. A byte array means the same thing to the
guest, the kernel and the host (a `write(2)` buffer, a file mapping, a string
in the ELF image). So I/O, `mmap` of files and the loader's segment copies
need no conversion, and only multi-byte *values* are swapped.

### 2.1 Rejected: reversed memory

Store the address space reversed, so that guest address `a` lives at linear
offset `C - a`. A big-endian access of `n` bytes at `a` is then a native
little-endian access at `C - a - n + 1`, which is one `i32.sub` instead of a
swap, for any width and any alignment. The price: every byte array is
backwards.

- Every kernel copy to or from the guest reverses.
- The loader writes segments reversed.
- Code fetch reverses.
- Every shim that reads guest memory directly (GL marshalling, the compositor's
  zero-copy `wl_shm` upload, the browser futex) needs rewriting.

That touches every stream's files, so it is rejected.

A word-swizzled layout (addresses XOR 3 for bytes) has the same problem, plus
unaligned accesses, and is rejected for the same reason.

### 2.2 The lowering

The lifter emits a plain little-endian `Load`/`Store` and a new
`UnOp::Bswap { ty }` on the value. `lwbrx`/`stwbrx`/`lhbrx`/`sthbrx`, the
byte-reversed forms, become the plain access with no swap.

Wasm has no byte-swap instruction, so the emitter lowers `Bswap` to one of
these:

| width | lowering | wasm ops |
| --- | --- | --- |
| 16 (`lhz`/`lha`/`sth`) | `((x & 0xff) << 8) \| ((x >> 8) & 0xff)`; `lha` adds `extend16_s` | 5 (+1) |
| 32 (`lwz`/`stw`) | `(rotl(x,8) & 0x00ff00ff) \| (rotr(x,8) & 0xff00ff00)` | 5 |
| 64 (`lfd`/`stfd`) | `i64x2.splat` → `i8x16.shuffle` (reverse lanes 0–7) → `i64x2.extract_lane 0` | 3 |

Why these:

- The 32-bit rotate form is the shortest scalar sequence: two rotates, two
  masks and an or, all single-cycle on x86-64 and AArch64. The and-immediates
  fit in an instruction.
- The four-shift form is 7 ops, and a byte-at-a-time load is 4 memory ops.
- A `v128.load32_zero` + shuffle + extract costs about the same as the rotate
  form for a 32-bit value, but its latency crosses the integer and vector
  domains twice, and the rotate form stays in integer registers.
- For 64 bits the scalar form is 11 ops plus two 64-bit masks. Those masks
  don't fit in an x86 immediate, so each costs a `movabs`. The shuffle is one
  `pshufb` (or `tbl` on AArch64) with a constant mask, so the vector form wins.
- ppc32's 64-bit accesses are only `lfd`/`stfd`, whose values are doubles and
  pass through an FP reinterpret anyway.

### 2.3 Pricing

Checked once the corpus runs (§12):

- Count the swaps per kernel from GIR statistics. `Bswap` defs are counted
  like loads in `pass::stats`.
- Time the ppc32 build against the AArch64 build of the same corpus kernel,
  and against a ppc32 build with `GEX_UNSAFE_NO_BSWAP=1`. That diagnostic
  drops the swaps: the result is wrong, but the ratio is the price.
- Fold `Bswap(Bswap(x)) = x` in the block-local passes. A reload of a stored
  register inside one block then costs nothing.

### 2.4 32-bit effective addresses

ppc32 computes effective addresses modulo 2^32. The lifter keeps GPRs as
`I32` (§4) and computes `rA + d` with an `i32.add`, which wraps exactly as the
hardware does. GIR's `Load`/`Store`/`CmpXchg`/`Rmw` then take that `I32`
address directly.

For an `I32` address the emitter skips the 33-bit guard. The guard exists to
steer a 64-bit address that doesn't fit into the poison page. A 32-bit address
can't miss that way: anything at or above `GUEST_LIMIT` is past the end of
linear memory and traps there anyway.

A non-negative displacement rides as the wasm static offset. That is exact:
`rA + d` wrapping past 2^32 would land in page 0 on the hardware, which Linux
never maps (`mmap_min_addr`), and in gex it traps past the end of memory. Both
end as SIGSEGV. A negative displacement costs an `i32.add`.

AArch64 and x86-64 never produce an `I32` address, so their modules don't
change.

---

## 3. The kernel's view: a guest-endian accessor layer

`gex-kernel` reads and writes guest structures through `GuestMem`'s typed
accessors (`read_u32`, `write_u64`, …): stat, iovec, sigaction, timespec,
epoll events, poll fds, futex words, the signal frame. The same accessors
also read and write the thread context and the dispatch directory. Those are
gex's own little-endian structures, which translated code reads with native
wasm loads.

**The rule:** everything below `gex_abi::DISPATCH_DIR_END` is the runtime's
(contexts, the dispatch directory, the kernel's per-thread scratch area) and
is host-endian. Everything at or above it is guest memory and is
guest-endian. `mem::hits_runtime` already refuses every guest mapping below
that line, so the rule is an invariant, not a heuristic.

Under `ppc`, `GuestMem`'s typed accessors swap for addresses at or above the
line. The whole diff is the four accessor bodies:

```rust
pub fn read_u32(&self, addr: u64) -> Result<u32, Errno> {
    let b = self.read(addr, 4)?;
    Ok(gex_abi::load_u32(addr, b.try_into().unwrap()))   // LE below the line, guest order above
}
```

With `ppc` off, `gex_abi::load_*`/`store_*` are `from_le_bytes`/`to_le_bytes`
by construction, and the AArch64 and x86 builds compile to the same code.

The kernel's per-thread scratch area (`ctx::SCRATCH`, where i386's
translations materialise a widened timespec before calling the canonical
handler) sits below the line. Its writer and its reader are both the kernel,
through the same accessors, so it stays consistent.

The sites that build a guest structure as a byte buffer (`Stat::encode`,
`statx`, dirent records, termios, `uname`, eventfd's counter) use
`to_ge_bytes`/`from_ge_bytes`, from a `GuestEndian` trait in `gex-abi`. That is
a mechanical `s/to_le_bytes/to_ge_bytes/` at the sites that write *guest*
memory. With `ppc` off it is the identical `to_le_bytes`. The NT layer
(`gex-kernel/src/nt/`) and the i386 code are not touched: they only run on the
x86 build.

The loader uses the same helpers for the initial stack (argv/envp pointers,
auxv).

---

## 4. Guest state

The context keeps AArch64's slot numbering, so the kernel and shims find
`PC`, `SP` and the syscall registers through the existing `ctx::` names:

| state | GIR | context slot |
| --- | --- | --- |
| r0–r31 | `GuestReg::R(n)`, `I32` | low word of `ctx::x(n)` (the upper word stays 0, so the kernel's `read_u64` of an argument is the zero-extended value) |
| SP (r1) | `R(1)` | `ctx::SP = x(1)` |
| thread pointer (r2) | `R(2)` | `ctx::TPIDR = x(2)`, so `clone(CLONE_SETTLS)` writes r2 |
| PC | `Pc` | `ctx::PC`, unchanged |
| LR, CTR | `Lr`, `Ctr`, `I32` | `ctx::PPC + 32`, `+36` |
| XER.SO, OV, CA | `XerSo`, `XerOv`, `XerCa` — flags, one byte each | `ctx::PPC + 40..43` |
| XER byte count (bits 25–31) | `XerBc`, `I32` byte | `ctx::PPC + 43` |
| CR bits 0–31 | `Cr(i)` — flags, one byte each, 0 or 1 | `ctx::PPC + 0..32`; bit `4f+k` is CR*f*'s LT/GT/EQ/SO for k = 0..3 |
| FPSCR | `Fpscr`, `I32` | `ctx::PPC + 44` |
| f0–f31 | `GuestReg::F(n)`, `I64` bit patterns | low 8 bytes of `ctx::v(n)` |
| reservation | `MonAddr`/`MonValue`/`MonSize`, as AArch64 | unchanged |

`ctx::PPC` is the area after `v31`, where x86-64 keeps its x87 stack. The
scratch area stays at `ctx::SCRATCH`, and `ctx::SIZE = SCRATCH + 64`. A
compile-time assertion checks that the ppc area ends below `SCRATCH`.

**Why a byte per CR bit.** A conditional branch reads one bit (`bc 12,2`
tests CR0.EQ), so a byte per bit makes the test one `i32.load8_u` and nothing
else. When the compare is in the same block, it costs not even that: the
register cache holds the compare's value in a local. This mirrors how gex
already keeps NZCV, one byte per flag.

The alternatives are worse:

- a nibble per field makes every branch a load, shift and mask;
- a packed 32-bit CR makes every compare a read-modify-write.

A compare writes its field's four bytes. `mfcr`, `mtcrf`, `mcrf` and the CR
logical ops (`crxor`, `cror`, `creqv`, …) work bit by bit. `mfcr` is rare
outside prologues.

**Keeping CR cheap:**

1. The existing block-local `dead_store_elim` already drops a CR bit that is
   overwritten before it is read within the block. `is_flag` covers `Cr(_)`,
   so the bytes are stored as `i32.store8`.
2. The lifter tracks, per CR bit, the GIR value last written in the block,
   the way the A64 lifter's `FlagSource` tracks NZCV. A `cmpw; bne` pair
   branches on the `Cmp` value directly and reads nothing back.
3. Cross-block dead-flag elimination (`gex-xlate` running `pass::flag_effects`
   over the CFG) is keyed by a `u8` mask of AArch64/x86 flags. Extending it to
   the 35 ppc flag bits means a `u64` mask: a type change in S2's files, so it
   is proposed to the coordinator rather than made here, after measuring what
   it would save. The first measurement is CR stores per block executed.
   Packing a field's four bytes into one `i32.store` in `flush` (as NZCV
   already is) is the other lever, and is priced the same way.

**XER.SO in compares.** Every compare and every record form (`add.`) copies
XER.SO into the field's SO bit. That is a byte load per compare unless SO is
known in the block. The load stays, because it is exact. Only the `o` forms
(`addo`, `subfo`, …) and `mtxer` set SO.

---

## 5. ELF and the initial process

gex-ld:

- `Reader` gains a byte order: an `ELFDATA2MSB` image is accepted on the ppc
  build only, as `ELFCLASS32` + `EM_PPC`, and every header field is read
  big-endian.
- `Image::class32` is set, as for i386. The section tables are read in their
  32-bit layouts too, so the symbols seed discovery. The i386 path skipped
  them.

Static executables come first: non-PIE, linked at 0x1000000 by `cc.sh`, or at
lld's default 0x10000000. Both are above `EXEC_BASE` and inside the dispatch
directory's reach.

- Static-PIE needs `R_PPC_RELATIVE` (22).
- Dynamic linking needs musl's `ld.so` for ppc: `R_PPC_ADDR32` (1),
  `R_PPC_GLOB_DAT` (20), `R_PPC_JMP_SLOT` (21), `R_PPC_DTPMOD32` (68),
  `R_PPC_DTPREL32` (78), `R_PPC_TPREL32` (73), and the secure-PLT layout.

Both come after the static milestones.

**The initial stack** is the SysV one, in 4-byte big-endian words:
argc, argv, NULL, envp, NULL, auxv, then 16 bytes of `AT_RANDOM`. `r1` points
at argc, 16-byte aligned. musl's `_start` needs nothing else. The kernel's
ELF ABI also puts argc/argv/envp/auxv in r3–r6, and gex does the same.

**auxv:**

- `AT_HWCAP`: `PPC_FEATURE_32 | PPC_FEATURE_HAS_FPU | PPC_FEATURE_HAS_MMU`,
  and no `HAS_ALTIVEC`, so a libc that dispatches on it never picks a vector
  routine.
- `AT_PLATFORM`: "ppc750".
- `AT_DCACHEBSIZE`, `AT_ICACHEBSIZE`, `AT_UCACHEBSIZE`: 32. glibc's `memset`
  reads these.
- The rest as for the other guests.

**TLS:** variant I, TCB at the thread pointer. ppc32 puts the thread
pointer 0x7000 past the TCB end: `r2 = tcb + 0x7000`, and TLS offsets are
biased by −0x7000. musl sets r2 itself (`__set_thread_area` is `mr 2,3`, no
syscall). The loader only needs the bias for the relocations above.

---

## 6. Linux ppc32 system-call ABI

- `sc`: the number in r0, arguments in r3–r8, the result in r3.
- **Errors are signalled by CR0.SO** with a *positive* errno in r3. On
  success SO is cleared.
- The kernel clobbers r0, r9–r12, CTR, XER and CR0 across the call. gex
  preserves them, which is allowed.

**The number table** is arch/powerpc's `syscall.tbl` (`powerpc/32`/`common`
entries), in `gex_kernel::ppc::raw`. ppc32 is an old-numbering 32-bit
ABI like i386, so `ppc::translate` does three things:

1. Maps a ppc number to the i386 raw number of the same call where the two
   ABIs agree on arguments and structures, and reuses
   `i386::translate`. That covers `mmap2`, `_llseek`, the time32 calls, the
   legacy path forms, `getrlimit`, and the rest.
2. Fixes the calls where ppc32 differs before handing over:
   - 64-bit arguments go in an **aligned register pair, high word first**:
     `pread64(fd, buf, n, pad, hi, lo)`, `ftruncate64(fd, pad, hi, lo)`,
     `fallocate(fd, mode, hi, lo, hi, lo)`, and the `fadvise64_64` family
     (`fd, advice, hi, lo, hi, lo`);
   - `clone`'s TLS argument is a plain value, not a `user_desc`;
   - `sigreturn` and `rt_sigreturn` use ppc frames (§7);
   - the flag spaces that differ:
     - `O_DIRECT`/`O_LARGEFILE` (0400000/0200000 on ppc);
     - `MAP_NORESERVE` 0x40 and `MAP_LOCKED` 0x80;
     - `SA_*` are generic;
     - the ioctl encoding (`_IOC_READ = 2`, `_IOC_WRITE = 4`, direction bits
       29–31): `TCGETS` is 0x402c7413 and `TIOCGWINSZ` is 0x40087468. ppc's
       termios has its own field order, `NCCS = 19` and its own flag values,
       so the tty ioctls convert explicitly, and the rest fail loudly.
3. Leaves everything else unmapped, which is `ENOSYS` and recorded (the
   existing unknown-syscall report).

musl 1.2.5 on ppc32 uses the `*_time64` calls (403–422) and `statx`, whose
layout is architecture-neutral. So stat64's ppc layout (104 bytes, no
padding after `st_dev`) matters only for the legacy `stat64` numbers, which
get their own encoder. `epoll_event` is 16 bytes on ppc32: `u64` data is
8-aligned, unlike i386's packed 12. That is the kernel's existing non-x86
path.

**The result convention** is written in one place: a helper
`syscalls::set_result(m, ctx_addr, ret)`. On ppc it stores `-ret` in r3 and
sets CR0.SO for `-4095..=-1`, and otherwise stores `ret` and clears SO. It
stores to `x(0)` on the other guests. The existing sites that write `x(0)`
after a call (the main path, fork/clone's child, the blocking completions in
both shims) call it. That is a one-line change each, and identical code off
ppc.

**Syscall restart:** gex's kernel doesn't restart system calls on any guest,
since an interrupted call returns `EINTR`. The context reserves
`ctx::PPC_ORIG_R3` (Linux's `orig_gpr3`) for the day it does.

---

## 7. Signals

`rt_sigframe` for ppc32 (`arch/powerpc/kernel/signal_32.c`) is laid out from
the new stack pointer upward:

```
sp →  __SIGNAL_FRAMESIZE (64) + 16 bytes of caller frame (back chain at sp)
      struct siginfo            128
      struct ucontext
          uc_flags, uc_link             8
          uc_stack (sp, flags, size)   12
          uc_pad[7]                    28
          uc_regs → &uc_mcontext        4
          uc_sigmask                    8  (+ uc_maskext[30] + uc_pad2[3] = 132 bytes)
          uc_mcontext (16-aligned)
              mc_gregs[48]            192  r0–r31, nip, msr, orig_r3, ctr, lnk,
                                            xer, ccr, mq, trap, dar, dsisr, result
              mc_fregs[33]            264  f0–f31, fpscr (as a double)
              mc_pad[2]                 8  (the kernel's sigreturn trampoline)
              mc_vregs                     (no AltiVec: not written; the
                                            kernel omits it when MSR_VEC is 0)
      abigap[56]                   224
```

The handler is entered with:

- r1 = the frame, whose back chain is the old r1;
- r3 = signo, r4 = &info, r5 = &uc, r6 = the frame;
- nip = the handler;
- LR = `SIGRETURN_TRAMPOLINE`, gex's existing no-translation address. The
  shims already turn a branch there into `rt_sigreturn`, so no code is
  written into the frame.

The CR, XER, LR, CTR and FPSCR are packed into `ccr`, `xer` and friends
from the context's bytes, and unpacked on return. musl reads
`uc_mcontext.gregs[32]` for the faulting PC (`pthread_cancel`), which is
nip at index 32. Non-rt `sigreturn` (the old `struct sigframe`) is used by no
musl or glibc build for ppc32, so it fails loudly. The code is
`gex_kernel::ppc::{setup_frame, sigreturn}`, with two dispatch lines in
`signal.rs`.

Synchronous faults: a wasm trap on a guest access is SIGSEGV as for every
guest. `trap`/`tw`/`twi` are SIGTRAP with `TRAP_BRKPT` when unconditional;
conditional traps evaluate their condition inline. An illegal instruction is
SIGILL at its own PC.

---

## 8. Floating point

- **FPRs** are 64-bit double bit patterns (`GuestReg::F`). Single-precision
  values live in them in double format, as the architecture defines.
- **Loads and stores:** `lfs` converts single to double (exact), and `stfs`
  converts double to single. `stfs` of a double that isn't representable is
  architecturally a round-to-single, which `f32.demote` does, and a
  single-precision denormal promoted by `lfs` stays exact. `stfiwx` stores
  the FPR's low word. `lfd`/`stfd` are a 64-bit swap and a reinterpret.
- **Single-precision arithmetic** (`fadds`, `fsubs`, `fmuls`, `fdivs`,
  `fsqrts`): the operation in double, then `f32.demote` and `f64.promote`.
  - Power ISA Book I §4.3.5.1: operands of the single-precision instructions
    must be representable in single format, and results are undefined
    otherwise.
  - For single operands, double rounding through binary64 is innocuous for
    +, −, ×, ÷ and √, because 53 ≥ 2·24 + 2 (Figueroa). So this is exact on
    every defined input, and the fuzzer only feeds these instructions
    single-representable operands.
- **Fused multiply-add:** `fmadd`, `fmsub`, `fnmadd`, `fnmsub` and their
  single forms must be fused.
  - The double forms use gex's existing FMA (`HostFunc::Fma` natively,
    `gex-emit/src/fma.rs` inline in the browser).
  - The negated forms negate the *result* (`-(a·c + b)`), which differs from
    `(-a)·c - b` only in the sign of a zero or NaN, and gex computes the
    former.
  - The single forms can't take the double FMA and demote, because that
    rounds twice. They are computed with **round-to-odd**:
    - p = a·c exactly in double (24 + 24 bits);
    - s = p + b with its TwoSum error e;
    - s is rounded to odd in double: if e ≠ 0, step s toward zero by one ulp
      when e's sign opposes s's, then set its low bit;
    - then demote. A round-to-odd result with 53 ≥ 24 + 2 bits rounds
      correctly to single, so there is one rounding in total.
- **Conversions:**
  - `fctiw`/`fctiwz` convert to int32 in the current rounding mode or toward
    zero, and saturate: NaN gives 0x80000000, and out-of-range values give
    the nearest bound. That is wasm's `trunc_sat` plus a NaN select.
  - The result is written to the FPR's low word. The high word is undefined
    by the architecture. gex writes what QEMU (the oracle) writes: the
    result's sign extension, and zero for a NaN (checked with a probe
    program under `qemu-ppc`).
  - `fcfid` and `fctid*` are 64-bit-implementation instructions and fail
    loudly.
  - `frsp` rounds to single in the current mode.
- **Compares:** `fcmpu`/`fcmpo` set the CR field to LT/GT/EQ/UN, with UN in
  the SO slot, and set FPSCR's FPCC.
- **FPSCR:**
  - The rounding mode (RN, bits 30–31) is honoured where gex computes the
    rounding itself: `fctiw`, `frsp`, `frin`/`friz`/`frip`/`frim`.
  - Arithmetic in a non-default mode can't be expressed in wasm. So a write
    that sets RN ≠ 0 (`mtfsf`, `mtfsfi`, `mtfsb0`/`mtfsb1`) records the mode,
    and the next arithmetic instruction lifted in a block that reads a
    non-default mode fails loudly. That check is one byte test at block
    entry, emitted only in blocks that contain FP arithmetic.
  - The sticky exception bits, FR/FI and FPRF are **not computed**. `mffs`
    returns the bits gex maintains: RN, NI, the enables, and any bit the
    program wrote itself with `mtfsf`/`mtfsfi`/`mtfsb1`, so `feraiseexcept`
    followed by `fetestexcept` round-trips. The hardware-computed status
    bits read as 0. `GEX_FP_STRICT=1` makes `mffs` fail loudly instead. See
    §8.1.
  - Enabling FP exceptions (`prctl(PR_SET_FPEXC)`, `MSR[FE0,FE1]`) fails
    loudly.
- **Estimates:** `fres` and `frsqrte` have implementation-specific results
  (the ISA only bounds their error), so no single answer is "the hardware's".
  They fail loudly with the instruction named. Only `-ffast-math` code
  emits them.
- **`fsel`** is a compare and select. **`fmr`, `fneg`, `fabs`, `fnabs`** are
  bit operations, exact on NaNs.
- **NaNs:** propagation and the default NaN follow wasm, as they do for the
  AArch64 guest. ppc's default NaN is positive (0x7FF8…), and wasm on x86-64
  produces the negative one. So an invalid operation's NaN may differ in sign
  and payload (PLAN §7 "NaN payloads"). The fuzzer compares NaN-ness, not
  payload.

### 8.1 The FP status gap, on every gex guest

No gex guest computes the IEEE exception flags. Doing so would cost a check
after every FP operation, since wasm reports none of them. One gap, three
spellings:

| guest | register | what reads as zero | readers |
| --- | --- | --- | --- |
| AArch64 | FPSR | IOC, DZC, OFC, UFC, IXC, IDC, QC | `mrs x, fpsr` (`fetestexcept`) |
| x86-64, i386 | MXCSR, x87 SW | IE, DE, ZE, OE, UE, PE; the x87 exception bits | `stmxcsr`, `fnstsw` |
| PowerPC | FPSCR | FX, FEX, VX*, OX, UX, ZX, XX, FR, FI, FPRF | `mffs` |

The control half is kept exactly on all three: rounding mode, enables, and
what the program wrote.

A program asking "was that inexact?" is told no. The soundness-style test
`tests/ppc/fp-status.sh` keeps the gap visible: `fetestexcept(FE_INEXACT)`
after `1.0/3.0` reads 1 under `qemu-ppc` and 0 under gex, and the test is
marked xfail with this reason. On PowerPC, `GEX_FP_STRICT=1` turns the read
itself into a loud failure.

---

## 9. Atomics and barriers

- `lwarx rT, rA, rB`: a 4-byte load. The raw (unswapped) word goes to
  `MonValue`, the address to `MonAddr` and 4 to `MonSize`, and the swapped
  value to rT.
- `stwcx. rS, rA, rB`: exactly the AArch64 `STXR` lowering in `gex-lift`.
  - A `CmpXchg` at the address if the monitor is armed for it, otherwise at
    `EXCLUSIVE_SCRATCH`, expecting the raw `MonValue` and storing
    `Bswap(rS)`.
  - CR0 = `0b00 || success || XER.SO`, and the monitor is cleared.
  - An ABA between the pair is invisible, the known LL/SC → CAS
    approximation gex already makes on AArch64.
- `lbarx`/`lharx` are not in the baseline (Power ISA 2.06), so they fail
  loudly.
- `sync`, `lwsync` (a `sync 1`), `eieio`: `Stmt::Fence`. `isync`: nothing,
  since a translated block is already the synchronisation point.
- `icbi`: `cache_op(IC_INVALIDATE)`, the same hook AArch64's `ic ivau` uses,
  so self-modifying code is handled as on AArch64.
- `dcbz`: zero the 32-byte cache block (`AT_DCACHEBSIZE` = 32, since musl's
  and glibc's `memset` use `dcbz` only when told the size). `dcbf`, `dcbst`,
  `dcbt`, `dcbtst`, `dcbi`: no-ops.

---

## 10. SPRs

- `mfspr`/`mtspr` on LR (8), CTR (9) and XER (1) are the registers above.
- `mftb`/`mftbu` (TBL 268, TBU 269): a `SysRegRead` to the host's monotonic
  clock, at the frequency reported in `/proc/cpuinfo` ("timebase: 512000000",
  gex's choice).
- `mfspr` of PVR (287) is privileged on Linux and traps, so gex gives SIGILL.
- VRSAVE (256) reads 0 and writes are dropped, because the kernel allows it
  without AltiVec.
- Everything else raises SIGILL, as privileged SPRs do on Linux.
- `mfmsr` and the other supervisor instructions raise SIGILL.

---

## 11. Testing

- **P1.1:** `gex-isa-ppc` has a test that disassembles real binaries (the
  sysroot's `libc.a` objects and the corpus) and diffs every instruction
  against `llvm-objdump --mcpu=ppc` text. That includes the extended
  mnemonics, which llvm prints for `bc`, `rlwinm`, `or`, `mfspr`, and the
  like.
- **`tests/diff-fuzz-ppc`**, modelled on `tests/diff-fuzz`:
  - Random instruction words the decoder accepts, filtered to the
    integer/CR/FP/branch classes that can run in isolation.
  - They are wrapped in a harness that seeds r0–r28, CR, XER, LR, CTR and
    f0–f31 from a blob, then dumps them.
  - Assembled with `clang --target=powerpc-linux-musl` against the sysroot.
  - Run under `qemu-ppc` and `gexc run`, and the bytes are compared.
  - Branches are in a class of their own: the harness places a landing pad at
    both targets and dumps which one was reached.
  - Single-precision FP operations get single-representable operands (§8).
- **Corpus:** pewasm's `bench/programs/*` and `bench/harness/kernel.h`, built
  by `tools/ppc/cc.sh` (C) and zig's `powerpc-linux-musleabihf` C++ toolchain
  for the C++ kernels (§12). Checksums are compared against `qemu-ppc` and
  native x86-64, and the kernels run through `bench/gex-run.mjs` as S1 runs
  the AArch64 ones.
- **AArch64 and x86-64 stay byte-identical:** before every milestone report,
  compare the `.wasm` from `gexc build` of the corpus on this branch against
  `merge/main`, and run the workspace tests with the default and the x86
  features.

---

## 12. Merge notes

When this branch meets S2's function IR and S3's C backend, which are not on
merge/main yet, every exhaustive match over GIR needs arms for:

- `UnOp::Bswap`/`Bswap16`: the rotate/shift lowerings in
  `gex-emit/src/tier1.rs::emit_bswap`, and `__builtin_bswap32`/`16`/`64` in C;
- the PowerPC `GuestReg`s: `R`, `Lr`, `Ctr`, `Cr`, `So`, `Ov`, `Ca`,
  `XerBc`, `Fpscr`, `F`, with their context slots in `gex_abi::ctx::PPC_*`;
- `I32` memory addresses (§2.4).

Nothing else in GIR changed.

---

## 13. Status

| milestone | state |
| --- | --- |
| toolchain: sysroot and `cc.sh` | done: musl 1.2.5 + compiler-rt 21.1.8 builtins, `-mcpu=ppc`, no vector instructions; hello runs under `qemu-ppc` |
| P1.1 decoder, disassembler, objdump diff | done: 0 mismatches over the sysroot's libc.a and compiler-rt (128,633 words) and 200,000 random words |
| P1.2 static hello under `gexc run` | done |
| P1.3 corpus checksums, diff-fuzz-ppc green | corpus 30/30 under the native shim (result lines equal to qemu-ppc and x86-64); diff-fuzz-ppc green: int 2,000, cr 1,000, branch 1,000, mem 1,000, fp 3,000 cases. Tier-1 timing through the browser shim: pending |
| P1.4 FP complete, atomics, signals, threads, soundness corpus | — |
| P1.5 browser shim builds | — |

Found on the way:

- diff-fuzz-ppc found a bug in gex-emit's inline software FMA, shared by
  every guest. An exact zero result lost its sign: `fma(0, -1, -0)` gave +0,
  where IEEE says -0. It is fixed in its own commit, independent of this
  work.

- zig 0.16 *can* provide libc for `powerpc-linux-musleabihf` (hard-float
  EABI, the same ABI for everything here). It also brings libc++, which the
  corpus's C++ kernels need. The sysroot is still the primary C toolchain, as
  asked.
